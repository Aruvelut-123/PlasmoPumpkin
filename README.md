# PlasmoPumpkin

A **Plasmo Voice–compatible voice server**, written in pure Rust and packaged as a
[Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) WASM plugin.

The project is a Cargo workspace with two crates:

| Crate | Target | What it is |
| --- | --- | --- |
| `crates/plasmo-voice-core` | any (host) | Pure-Rust re-implementation of the Plasmo Voice 2.x wire format: UDP/TCP packet codecs, packet registry, and all serializable data models. Byte-for-byte compatible with the upstream Java `su.plo.voice.proto`. |
| `crates/plasmo-voice-plugin` | `wasm32-wasip2` | The voice server itself, compiled to a WebAssembly *component* and loaded by Pumpkin as a plugin. Runs the UDP data plane in-guest and persists state to the plugin data folder. |

> `.recon/` holds the reconnaissance that backs both halves of the project: the Java
> protocol dumps behind the wire codecs, and the snapshots of the pinned
> `pumpkin-plugin-api` behind the plugin. It is **not** part of the build. A curated
> subset is committed — e.g. [`.recon/WIRE.txt`](.recon/WIRE.txt) and
> [`.recon/plugin-api.txt`](.recon/plugin-api.txt) — and the rest is gitignored scratch.

---

## Status

| Area | State |
| --- | --- |
| `plasmo-voice-core` wire format (UDP + 26 TCP packets + data models) | ✅ implemented, 39 tests |
| `plasmo-voice-plugin` (Pumpkin component) | ✅ UDP voice server with a working control plane — 34 unit tests + 3 session + 2 socket tests |
| Native tests (`cargo test --workspace`) | ✅ 78 tests passing |
| `wasm32-wasip2` component build | ✅ verified: a component (layer `0x0d`) exporting all six host entry points |
| Lint & format (`cargo fmt`, `cargo clippy -D warnings`) | ✅ clean on the host **and** on `wasm32-wasip2` |
| CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) | ✅ four jobs: `core`, `policy`, `plugin`, `hygiene` |

### How a player gets connected (the control plane)

Upstream Plasmo Voice has **no TCP listener**: the 26 "TCP" packets travel over the
Minecraft plugin-message channel (`ServerChannelHandler` decodes them from plugin-message
bytes with `PacketDirection.SERVER`), and only UDP is a real socket. A player's session is
established like this:

1. the server mints a secret for the player — `VoiceUdpServerConnectionManager
   .getSecretByPlayerId` is the only place `secretByPlayerId` / `playerIdBySecret` are
   filled (`UUID.randomUUID()` per session, in memory);
2. it sends the client a clientbound `ConnectionPacket(secret, ip, port)` over the control
   plane, telling it where the UDP socket lives and which secret to speak with;
3. the client pings over UDP until it hears back; on the **first datagram whose secret is
   known**, `NettyPacketHandler` creates the UDP connection (`addConnection`) and answers
   with the config / player-list burst;
4. any datagram whose secret is *not* known is dropped without a reply.

This plugin reproduces that model with **IPC as the control plane**, because that is what a
Pumpkin plugin has instead of the Minecraft channel. `plasmo:voice/v2` therefore has four
commands:

| Message | Reply |
| --- | --- |
| `plasmo:voice/v2\nhandshake` | `port=…`, `secret=…` (the server-wide secret), `listening=…` |
| `plasmo:voice/v2\nstatus` | `listening`, `players`, `connections`, `received`, `sent`, `dropped` |
| `plasmo:voice/v2\nplayer-connect\nplayer=<uuid>\n[name=<name>]\n[ip=<public ip>]` | `player`, `secret` (this player's), `port`, and `packet=<hex>` |
| `plasmo:voice/v2\nplayer-disconnect\nplayer=<uuid>` | `player`, `secret` (forgotten), `players` |

`packet=<hex>` is the encoded clientbound `ConnectionPacket`, ready to forward to that
player's client verbatim; it is `packet=none` when no `ip=` was supplied, because a plugin
has no public address of its own to advertise and inventing `0.0.0.0` would point every
client at nothing. **Nothing else can create a connection** — an unregistered secret is
dropped on sight, so a public UDP port never relays a stranger's audio.

### Ping and keep-alive semantics

The two directions of `PingPacket` are not symmetric, and getting that wrong is a real bug:

* **A client's ping is a registration request / keep-alive ack, and the server never
  answers it.** A real client replies to *any* inbound ping with another ping, so echoing
  pings back would produce an unbounded ping-pong between server and client.
* **The server sends its own keep-alive pings** — an empty `PingPacket` (a timestamp and
  nothing else) at most once per second per connection, with 1.5–3 s of jitter derived from
  the secret. That first ping is also what makes a real client consider itself connected.
* A connection that has sent nothing for `keepAliveTimeoutMs` (upstream default **15 s**) is
  retired.

Both are driven from the one clock the guest has: the Pumpkin tick event (see
[`crates/plasmo-voice-plugin/src/runtime.rs`](crates/plasmo-voice-plugin/src/runtime.rs)),
since Pumpkin 0.2.0 puts no `on_tick` on the `Plugin` trait.

### What is not implemented yet

The session path above is complete — a registered player's audio reaches the other
connections — but this is not yet full upstream parity. In rough order of importance:

* **The post-registration control-plane burst.** Upstream answers the first UDP datagram
  with `sendConfigInfo` + `sendPlayerList` + `broadcastPlayerInfoUpdate`, i.e. a
  clientbound `ConfigPacket`, `PlayerListPacket` and `PlayerInfoUpdatePacket`. This plugin
  returns only the `ConnectionPacket`; the other codecs exist in `plasmo-voice-core` with
  round-trip tests, but nothing wires them to the control plane yet.
* **Audio is broadcast, not positional.** Upstream decides who receives a
  `PlayerAudioPacket` from activations, source lines and player positions. This server fans
  every frame out to every *other* connection: correct for one voice channel, wrong for
  anything distance-based.
* **Source and self audio are not relayed.** `SourceAudio` (server → clients, for non-player
  sources) and `SelfAudioInfo` are decoded and dropped.
* **The rest of the control plane.** `PlayerInfo`, `PlayerState`, `PlayerAudioEnd`,
  `SourceInfo`, activations and source-line synchronisation are all implemented in the core
  crate but have no plugin-side handler.

---

## Protocol summary

Plasmo Voice has **one transport** — UDP, the audio data plane and the only path a voice
server actually needs to serve — plus a control plane that upstream tunnels through the
Minecraft plugin-message channel. Every UDP datagram is prefixed by a fixed header:

  ```
  offset  size  field
  0       4     magic    0x4e9004e9  (big-endian)
  4       1     packet id (u8; the 0x100 "custom" id truncates to 0x00)
  5       8     secret UUID high 64 bits
  13      8     secret UUID low  64 bits
  21      8     timestamp (i64, big-endian)
  29      ...   packet body
  ```

  Registered UDP packets:

  | Id | Name | Direction | Body |
  | --- | --- | --- | --- |
  | `0x01` | `Ping` | ANY | optional server IP (UTF) + port (u16) |
  | `0x02` | `PlayerAudio` | SERVER | source id, sequence number, audio frame bytes |
  | `0x03` | `SourceAudio` | CLIENT | source id, sequence number, audio frame bytes |
  | `0x04` | `SelfAudioInfo` | CLIENT | activation id, distance, stereo flag |
  | `0x100` | `Custom` | ANY | addon-defined payload (truncates to `0x00` on the wire) |

  `Direction` uses upstream's `PacketDirection` naming, which is easy to misread:
  **`CLIENT` means the *client receives* it** (clientbound, server → client) and
  **`SERVER` means the *client sends* it** (serverbound, client → server). Checked
  against the upstream packages: `PlayerAudioPacket` lives in `udp.serverbound`, while
  `SourceAudioPacket` and `SelfAudioInfoPacket` live in `udp.clientbound`. This is why the
  server decodes every inbound datagram with `PacketDirection::Server`.

**The control plane** (26 packets, ids `0x01`–`0x1a`) covers handshake, config,
player/source state, activations and source lines. Upstream carries them over the
**Minecraft plugin-message channel**, not a socket: `PacketTcpCodec` is decoded from
plugin-message bytes on both sides, which is why the voice server has no TCP listener. This
plugin carries the same packets over IPC (see
[How a player gets connected](#how-a-player-gets-connected-the-control-plane)).

### Encoding rules (why this is not plain "Rust default")

The upstream implementation writes through Guava's `ByteArrayDataInput/Output`, so the
core crate reproduces those semantics exactly:

* all multi-byte integers, longs, floats and doubles are **big-endian**;
* strings use Java's **modified UTF-8** (`DataOutput.writeUTF`): `0x0000` encodes as two
  bytes, astral code points become a 6-byte surrogate pair, length is a **u16** prefix;
* `VoiceActivation::generate_id` / `VoiceSourceLine::generate_id` use a name-based UUID
  whose MD5 is taken **over the name bytes only** (no RFC 4122 namespace/version bits);
* "safe" reads (`readSafeUTF`, `readSafeInt`) clamp and reject out-of-range values the
  same way the Java code does.

IDs are derived from the packet struct, mirroring `PacketTcpCodec.getType(...)` and
`PacketUdpCodec.getType(...)`, including the `0x100` custom id.

---

## Building

Requirements: a stable Rust toolchain with the `wasm32-wasip2` target. Nothing else — the
plugin's only ABI dependency is fetched from GitHub through its git tag, and
`wasm32-wasip2` already emits a WebAssembly **component**, so no `cargo-component`
(or any other post-processing tool) is involved.

```powershell
rustup target add wasm32-wasip2
```

### Native build & tests (protocol core)

```powershell
cargo build
cargo test
```

### Plugin (wasm32-wasip2 component)

The plugin depends on `pumpkin-plugin-api`.

#### Pumpkin API version policy

> **This project uses, and only uses, the latest Pumpkin *stable* API — currently
> `0.2.0` (`0.2.0+26.3-26.51`).**
>
> Cargo resolves it as a **git tag**, which is the only way to get this release:

```toml
pumpkin-plugin-api = { git = "https://github.com/Pumpkin-MC/Pumpkin", tag = "0.2.0+26.3-26.51" }
```

Two consequences worth knowing:

* **Do not "upgrade" this to a crates.io version requirement casually.** The
  crates.io entry exists, but regional mirrors (e.g. some university/CN mirrors)
  frequently lag and still only carry the older `0.1.0-dev` prerelease. Resolving
  against such a mirror fails with *"failed to select a version for the
  requirement `pumpkin-plugin-api = "^0.2.0"`"*. The git tag bypasses mirrors
  entirely and names the exact upstream commit (`204a94e`).
* **Do not pin an older API.** `0.1.0-dev` is obsolete; code written against it
  does not describe the current ABI.

When a newer stable Pumpkin API is published, update the tag here **and** the
`pumpkin-plugin-api` entry in `crates/plasmo-voice-plugin/Cargo.toml`, then
refresh `Cargo.lock`. Treat "the pinned API is no longer the latest stable" as a
bug to be fixed, not a preference.

#### Building

```powershell
cargo build --target wasm32-wasip2 -p plasmo-voice-plugin --release
```

The artifact is `target/wasm32-wasip2/release/plasmo_voice_plugin.wasm`. (`cargo-component`
is deliberately not used: v0.21.1 cannot decode the component its own `wit-bindgen` 0.62
emits, and it would be redundant — the target output is already a component.)

Note: the historical `PUMPKIN_DIR` checkout override is no longer required — the
tag dependency fetches the API from GitHub. (If you deliberately need to build
against a local, unreleased Pumpkin checkout, add a `[patch]` entry pointing at
`crates/pumpkin-plugin-api`; do not change the published requirement.)

The commands CI runs are the canonical ones; there are no wrapper scripts that could drift
out of sync with them.

| Task | Command |
| --- | --- |
| Whole workspace tests | `cargo test --workspace` |
| One crate | `cargo test -p plasmo-voice-core` |
| Format check | `cargo fmt --all -- --check` |
| Host lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Component lint | `cargo clippy --target wasm32-wasip2 -p plasmo-voice-plugin --all-targets -- -D warnings` |
| Component build | `cargo build --target wasm32-wasip2 -p plasmo-voice-plugin --release` |

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) is the canonical definition of what
"passing" means. If it and this table ever disagree, the workflow wins.

---

## Installing into Pumpkin

1. Copy the built component into the server's plugin directory:

   ```
   <server cwd>/
     plugins/
       plasmo-voice.wasm
   ```

   Pumpkin scans `./plugins` for `*.wasm` files; no separate manifest file is needed —
   metadata is read out of the component itself.

2. Grant the plugin the permissions it needs. They are declared in
   `PluginMetadata.permissions` inside the component, and enforced by the host:

   | Permission | Why |
   | --- | --- |
   | `network.udp.bind` | Bind the voice UDP socket. Implies receive. |
   | `network.udp.connect` *(or* `network.udp`*/*`network.outbound`*)* | Required to **send** datagrams back to clients from the unconnected socket. |
   | `fs.read.data` / `fs.write.data` | Persist server state (secret UUID, activations, source lines) in the plugin data folder. |
   | `network.dns` | Only if hostnames are resolved. |

3. Make sure the global `plugins.loopback_only` config is **`false`** (the default).
   With `loopback_only = true` the guest socket is restricted to the loopback interface
   and clients on other machines will not reach the voice server.

---

## Repository layout

```
Cargo.toml                       workspace (members = crates/*)
README.md                        this file
AGENTS.md                        contributor / agent working agreement
crates/
  plasmo-voice-core/             the protocol (host-buildable, no I/O)
    src/
      lib.rs                     crate docs, re-exports, PROTOCOL_VERSION
      wire.rs                    TCP + UDP codecs, packet registry, 26 TCP + 5 UDP packets
      data.rs                    data models (VC activation, source info/lines, player info, ...)
      util.rs                    WireReader / WireWriter (big-endian, modified UTF-8, safe reads)
      md5.rs                     MD5 + name-based UUID generation
      error.rs                   VoiceError
  plasmo-voice-plugin/           the server (wasm32-wasip2 component)
    src/
      lib.rs                     crate docs, IPC parser + reply builder, the WASI-gated `glue` module
      server.rs                  UDP voice server: player registry, auth, audio fan-out, keep-alive
      runtime.rs                 process-global socket + protocol state, pumped once per tick
      state.rs                   secret UUID and persisted state in the plugin data folder
      tick.rs                    `ServerTickStartEvent` bridge (WASI only)
    tests/
      lifecycle.rs               end-to-end sessions: control message → encoded packet → relayed audio
      socket.rs                  the same relay over three real UDP sockets (server + two clients)
.github/workflows/ci.yml         the canonical build/lint/test definition
.recon/                          reconnaissance: wire dumps, pinned-API snapshots (not built)
```

---

## License

MIT — see [LICENSE](LICENSE).
