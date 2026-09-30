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
| `plasmo-voice-plugin` (Pumpkin component) | ✅ UDP voice server, `plasmo:voice/v2` IPC handshake, state persistence — 21 tests |
| Native tests (`cargo test --workspace`) | ✅ 60 tests passing |
| `wasm32-wasip2` component build | ✅ verified: a component (layer `0x0d`) exporting all six host entry points |
| Lint & format (`cargo fmt`, `cargo clippy -D warnings`) | ✅ clean on the host **and** on `wasm32-wasip2` |
| CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) | ✅ four jobs: `core`, `policy`, `plugin`, `hygiene` |

---

## Protocol summary

Plasmo Voice uses **two transports**:

* **UDP** — the audio data plane, and the only path a voice server actually needs to
  serve. Every datagram is prefixed by a fixed header:

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
  | `0x02` | `PlayerAudio` | CLIENT | source id, sequence number, audio frame bytes |
  | `0x03` | `SourceAudio` | CLIENT | source id, sequence number, audio frame bytes |
  | `0x04` | `SelfAudioInfo` | CLIENT | activation id, distance, stereo flag |
  | `0x100` | `Custom` | ANY | addon-defined payload (truncates to `0x00` on the wire) |

* **TCP** — the control plane (26 packets, ids `0x01`–`0x1a`), used for handshake,
  config, player/source state, activations and source lines.

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
      lib.rs                     crate docs, IPC parser, the WASI-gated `glue` module
      server.rs                  UDP voice server: pings, connections, audio fan-out
      runtime.rs                 process-global socket + protocol state, pumped once per tick
      state.rs                   secret UUID and persisted state in the plugin data folder
      tick.rs                    `ServerTickStartEvent` bridge (WASI only)
.github/workflows/ci.yml         the canonical build/lint/test definition
.recon/                          reconnaissance: wire dumps, pinned-API snapshots (not built)
```

---

## License

MIT — see [LICENSE](LICENSE).
