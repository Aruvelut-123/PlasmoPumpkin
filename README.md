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
| `plasmo-voice-plugin` (Pumpkin component) | ✅ UDP voice server with the full control plane over `plasmo:voice` — 62 unit tests + 3 session + 2 socket tests |
| Native tests (`cargo test --workspace`) | ✅ 106 tests passing |
| `wasm32-wasip2` component build | ✅ verified: a component (layer `0x0d`) exporting all six host entry points |
| Lint & format (`cargo fmt`, `cargo clippy -D warnings`) | ✅ clean on the host **and** on `wasm32-wasip2` |
| CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) | ✅ four jobs: `core`, `policy`, `plugin`, `hygiene` |

### How a player gets connected (the control plane)

Upstream Plasmo Voice has **no TCP listener**: the 26 "TCP" packets travel over the
Minecraft plugin-message channel (`ServerChannelHandler` decodes them from plugin-message
bytes with `PacketDirection.SERVER`), and only UDP is a real socket. A player's session is
established in three phases:

1. **Join.** The server asks the client who it is with a clientbound
   `PlayerInfoRequestPacket` (id 2), retrying at +1/3/5/10/15 s until an answer arrives.
2. **Answer.** The client replies with `PlayerInfoPacket` (id 10): its mod version and an
   RSA public key. The server gates on the *major* version (and a `2.0.0` minimum), mints
   the player's secret with `VoiceUdpServerConnectionManager.getSecretByPlayerId`, and
   replies with a clientbound `ConnectionPacket(secret, ip, port)` (id 1).
3. **Registration.** The client pings over UDP until it hears back. On the **first
   datagram whose secret is known**, `NettyPacketHandler` creates the UDP connection and
   answers with the registration burst: `ConfigPacket` (id 3) → `PlayerListPacket` (id 7) →
   `PlayerInfoUpdatePacket` (id 8), in that order. Any datagram whose secret is *not* known
   is dropped without a reply, and the first datagram of a session is **only** a
   registration — its body is never handled or relayed, exactly as upstream skips
   `handlePacket` on that path.

This plugin implements all of that **in-guest over the `plasmo:voice` plugin-message
channel** — the same channel upstream uses. Pumpkin 0.2.0 hands a plugin both halves of the
mechanism (`PlayerCustomPayloadEvent` in, `java-player::send-custom-payload` out), so there
is no companion plugin, no proxy and no IPC hop in the voice path:

| Module | Role |
| --- | --- |
| [`channel.rs`](crates/plasmo-voice-plugin/src/channel.rs) | the ABI shim: host events in, `Outbound` messages out. The only module that names the Pumpkin channel API. |
| [`control.rs`](crates/plasmo-voice-plugin/src/control.rs) | the state machine: the handshake, the burst, and every serverbound packet's reply. Host-testable, no Pumpkin types. |
| [`config.rs`](crates/plasmo-voice-plugin/src/config.rs) | the `ConfigPacket` this server advertises, and the version/distance gates. |
| [`server.rs`](crates/plasmo-voice-plugin/src/server.rs) | the UDP semantics: per-connection secrets, activation- and position-filtered fan-out, keep-alive. |

The control plane is **not** IPC. The `plasmo:voice/v2` IPC namespace still exists as an
optional *programmatic* front door for other plugins or scripts (it can drive a session
without a real client):

| Message | Reply |
| --- | --- |
| `plasmo:voice/v2\nhandshake` | `port=…`, `secret=…` (the server-wide secret), `listening=…` |
| `plasmo:voice/v2\nstatus` | `listening`, `players`, `connections`, `received`, `sent`, `dropped` |
| `plasmo:voice/v2\nplayer-connect\nplayer=<uuid>\n[name=<name>]\n[ip=<public ip>]` | `player`, `secret` (this player's), `port`, and `packet=<hex>` |
| `plasmo:voice/v2\nplayer-disconnect\nplayer=<uuid>` | `player`, `secret` (forgotten), `players` |

`packet=<hex>` is the encoded clientbound `ConnectionPacket`, ready to forward to that
player's client verbatim; it is `packet=none` when no `ip=` was supplied. **Nothing else can
create a connection** — an unregistered secret is dropped on sight, so a public UDP port
never relays a stranger's audio.

### How audio is relayed

Upstream's relay is a proximity fan-out, not a broadcast, and the plugin reproduces it:

* A serverbound `PlayerAudioPacket` (UDP id 2) is **never forwarded as-is**. The server
  builds a clientbound `SourceAudioPacket` (id 3) for every listener in range — same
  sequence number, same payload bytes (the Opus frame is not re-encoded), tagged with the
  speaker's stable *source id* — and a `SelfAudioInfoPacket` (id 4) for the speaker itself,
  whose overlay needs to know its own stream is live.
* Every recipient gets a frame re-keyed to **its own** secret with a fresh timestamp,
  because those live in the per-client part of the envelope.
* The distance is clamped exactly like `Activation.calculateAllowedDistance` (the proximity
  activation allows 8/16/32 blocks and defaults to 16), and the radius is
  `min(distance + maxExtraAudioBroadcastDistance, distance * 2)` — 32 blocks at the default
  distance.
* A listener hears the speaker only if it has voice chat on, is in the same world, is within
  the radius, and is not the speaker. An unknown position or world means "cannot verify", so
  the frame is **not** relayed.
* `PlayerAudioEndPacket` (over the control plane) becomes a clientbound
  `SourceAudioEndPacket` (id 18) for the same listeners, plus a `SelfSourceInfoPacket`
  (id 17) for the speaker with `sequenceNumber = -1`.
* A listener that hears an unknown source asks for it with `SourceInfoRequestPacket`; the
  server answers `SourceInfoPacket` (id 16) with a `PLAYER` source carrying the speaker's
  nick, state, proximity line and an Opus decoder.
* `PlayerStatePacket` updates the player's mute flags and is re-broadcast as
  `PlayerInfoUpdatePacket` (id 8) only when something actually changed.

### Ping and keep-alive semantics

The two directions of `PingPacket` are not symmetric, and getting that wrong is a real bug:

* **A client's ping is a registration request / keep-alive ack, and the server never
  answers it.** A real client replies to *any* inbound ping with another ping, so echoing
  pings back would produce an unbounded ping-pong between server and client.
* **The server sends its own keep-alive pings** — an empty `PingPacket` (a timestamp and
  nothing else), immediately for a new connection and then every 2.5–4 s (1.5 s plus up to
  1.5 s of jitter derived from the secret). That first ping is what makes a real client
  consider itself connected; without a ping at all it goes soft-dead at **7 s** (it stops
  recording) and tears the connection down at **30 s**.
* The endpoint a client puts in its ping (`serverIp` / `serverPort`) is recorded
  informationally, like upstream's `connectionAddress`. It is **not** a send target:
  datagrams go to the address they arrived from (`remoteAddress`). Conflating the two would
  make the server send audio to itself.
* A connection that has sent nothing for `keepAliveTimeoutMs` (upstream default **15 s**) is
  retired and announced with a `PlayerDisconnectPacket`, while its *registration* survives —
  the secret is sticky, so the same client can come back with it.
* A client that changes UDP address is *followed* (`setRemoteAddress`), never duplicated.

Both directions are driven from the one clock the guest has: the Pumpkin tick event (see
[`crates/plasmo-voice-plugin/src/runtime.rs`](crates/plasmo-voice-plugin/src/runtime.rs)),
since Pumpkin 0.2.0 puts no `on_tick` on the `Plugin` trait.

### What is not implemented yet

The full connect → config → relay path is in place, but this is not yet complete upstream
parity. In rough order of importance:

* **Audio is plaintext.** Upstream RSA-encrypts a 16-byte AES key with the public key from
  `PlayerInfoPacket` and sends it inside `ConfigPacket.encryption`. This server sends
  `encryption: null`, which the protocol and the client both accept (a null encryption
  means "no cipher"), so **UDP audio is not encrypted** and clients send plaintext Opus.
  Closing this needs RSA in the guest plus a persisted AES key.
* **Only one activation and one source line.** The proximity activation and the proximity
  line are hard-coded from upstream's defaults; there is no TOML config, so distances,
  sample rate, MTU, opus bitrate and `maxExtraAudioBroadcastDistance` are constants.
  Upstream's `ActivationRegister` / `ActivationUnregister` (ids 19/20) and
  `SourceLineRegister` / `SourceLineUnregister` (21/22) are never sent.
* **Permissions are not enforced.** `pv.allow_freecam` is advertised and always true, as
  upstream's default; there is no permission system, no per-player activation permissions,
  and no `canSee` / vanish integration — a vanished player would still be heard.
* **No server-side mute manager.** `VoicePlayerInfo.muted` is always false; only the
  client-reported `voiceDisabled` / `microphoneMuted` are tracked.
* **Positions come from move events.** Upstream reads positions live from the Minecraft
  server when it computes a listener set. A guest cannot read the world behind the host
  boundary, so `PlayerMoveEvent` and the per-move refresh push them in. A player who never
  moves is never placed, and is therefore never a listener.
* **No audio processing.** There is no jitter buffer, no packet reordering, no
  packet-loss concealment and no server-side mixing: frames are forwarded as they arrive.
  Upstream does not do this either, but its clients do.
* **Decoration packets are not sent.** `ConfigPlayerInfoPacket` (id 4),
  `DistanceVisualizePacket` (14), `AnimatedActionBarPacket` (23) and the addon/entity/static
  source variants are unimplemented; `LanguagePacket` (id 6) answers with the requested
  locale and an *empty* translation table, so the client falls back to its own keys.
  `CustomPacket` (UDP `0x100`) is decoded but not routed to any addon.

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
      config.rs                  the ConfigPacket this server advertises; version and distance gates
      control.rs                 control-plane state machine: handshake, registration burst, replies
      server.rs                  UDP voice server: player registry, auth, proximity audio fan-out, keep-alive
      runtime.rs                 process-global socket + protocol state, pumped once per tick
      state.rs                   secret UUID and persisted state in the plugin data folder
      channel.rs                 the `plasmo:voice` plugin-message channel (WASI only)
      tick.rs                    `ServerTickStartEvent` bridge, socket pump + control delivery (WASI only)
    tests/
      lifecycle.rs               end-to-end sessions: control message → encoded packet → relayed audio
      socket.rs                  the same relay over three real UDP sockets (server + two clients)
.github/workflows/ci.yml         the canonical build/lint/test definition
.recon/                          reconnaissance: wire dumps, pinned-API snapshots (not built)
```

---

## License

MIT — see [LICENSE](LICENSE).
