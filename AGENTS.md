# AGENTS.md

Working agreement for humans and automated agents contributing to **PlasmoPumpkin**.
Read this before touching anything.

---

## 1. What this repository is

A Plasmo Voice–compatible voice server in Rust, shipped as a Pumpkin WASM plugin.
Two crates, one workspace:

* `crates/plasmo-voice-core` — the protocol. Pure Rust, no I/O, host-buildable, fully
  unit-tested. **This is the source of truth for anything on the wire.**
* `crates/plasmo-voice-plugin` — the server. Compiles to `wasm32-wasip2`, implements the
  `pumpkin-plugin-api::Plugin` trait, runs the UDP data plane in-guest.

  Inside that crate, everything that is *not* Pumpkin-specific — the protocol server, the
  config, the control-plane state machine, the state file, the socket pump, the IPC parser —
  is ordinary Rust and is covered by `cargo test` on the host. Only the ABI glue
  (`channel.rs`, `tick.rs` and the `glue` module) names `pumpkin-plugin-api`, and that
  dependency is declared for `target_os = "wasi"` only. See rule 6 for why that is
  load-bearing.

## 2. Non-negotiable rules

1. **Never break `cargo test`.** The workspace must build and test cleanly on the host
   before you consider any change done.
2. **Never break the `wasm32-wasip2` build.** The plugin is only useful as a component.
   Verify both halves after touching plugin code:

   ```powershell
   cargo build  --target wasm32-wasip2 -p plasmo-voice-plugin --release
   cargo clippy --target wasm32-wasip2 -p plasmo-voice-plugin --all-targets -- -D warnings
   ```

   The clippy run matters as much as the build: the ABI glue is invisible to the host
   lint, so this is the only place it gets checked (see rule 6).
3. **Use, and only use, the latest Pumpkin *stable* API — currently `0.2.0`
   (`0.2.0+26.3-26.51`).** This is pinned in
   `crates/plasmo-voice-plugin/Cargo.toml` as a **git tag**, not a crates.io version:

   ```toml
   pumpkin-plugin-api = { git = "https://github.com/Pumpkin-MC/Pumpkin", tag = "0.2.0+26.3-26.51" }
   ```

   * Do **not** replace it with a crates.io semver requirement. `0.2.0` is on
     crates.io, but regional mirrors lag and often only carry the obsolete
     `0.1.0-dev` prerelease, which makes the build fail with *"failed to select a
     version for the requirement `pumpkin-plugin-api = "^0.2.0"`"*. The git tag
     bypasses mirrors and pins the exact upstream commit.
   * Do **not** pin `0.1.0-dev` or otherwise work against an older ABI.
   * When Pumpkin publishes a newer stable API, updating the tag is part of your
     change — an out-of-date pin is a bug. Refresh `Cargo.lock` too.
4. **`plasmo-voice-core` stays I/O-free and target-agnostic.** No sockets, no files, no
   `std::time`-dependent behaviour, no `#[cfg(target_arch = "wasm32")]` in the core. If you
   need I/O, it belongs in the plugin crate.
5. **Wire compatibility beats ergonomics.** The Java implementation is the reference.
   When a design choice is ambiguous, match upstream byte-for-byte, even when the Rust
   equivalent would be prettier.
5. **Do not add a third-party dependency to `plasmo-voice-core` without a written
   justification** in the commit message. The crate currently depends only on `uuid`.
6. **The plugin crate must not be a workspace member dependency of the host build.**
   `wasm32-wasip2` code does not link in native tests; keep the component build isolated.
   Concretely: `pumpkin-plugin-api` is declared under
   `[target.'cfg(target_os = "wasi")'.dependencies]`, and every line that names it lives in
   `channel.rs`, `tick.rs` or the `glue` module of `lib.rs` — all three are `cfg`-gated.

   This is not a preference. The API generates the component's guest exports, whose symbol
   names are WIT-mangled (`pumpkin:plugin/metadata@0.1.0#get-metadata`). rustc lists the
   exported symbols in the ELF *version script* it hands the linker for a `cdylib`, and a
   version script cannot contain `:`, `@` or `#`, so linking this crate for a native target
   fails outright:

   ```
   rust-lld: error: list:10: ; expected, but got :
       cabi_post_pumpkin:plugin/metadata@0.1.0#get-metadata;
   ```

   Do not "fix" that by excluding the crate from `cargo test`; fix the dependency scope.

## 3. Source-of-truth hierarchy

When documents disagree, the order is:

1. the upstream Java source of `su.plo.voice.proto` (local checkout used during recon);
2. the pinned `pumpkin-plugin-api` source for the Pumpkin plugin ABI — the exact commit the
   git tag resolves to (`.recon/plugin-api.txt` and `.recon/plugin-trait.txt` are tracked
   snapshots of it);
3. the recon dumps under `.recon/` (`WIRE.txt`, `udp-packets.txt`, `proto-*/`, …);
4. code comments in `crates/*/src`.

`.recon/` is **evidence, not code**. Never let it become a build input, never add it to
`workspace.members`, and never "fix" code to match a stale dump without re-checking the
upstream source. Note that a curated subset of `.recon/` is tracked in git while the rest
is gitignored scratch; `git ls-files .recon` is the authoritative list.

## 4. Wire-format invariants

If you change anything in `util.rs` / `wire.rs` / `data.rs`, these must stay true:

* Multi-byte scalars are **big-endian**, matching Guava `ByteArrayDataInput/Output`.
* Strings are Java **modified UTF-8** with a `u16` length prefix. `0x0000` → two bytes;
  astral code points → 6-byte surrogate pairs *in the encoded form*.
* Packet ids come from the packet type, exactly as `PacketTcpCodec.getType` /
  `PacketUdpCodec.getType` compute them. The UDP "custom" packet is `0x100` but is
  **truncated to `0x00`** by `writeByte`, and `0x100` never decodes back.
* `UDP_MAGIC == 0x4e9004e9`; a datagram with a wrong magic must be ignored, not panicked on.
* Name-based UUIDs hash the **name bytes only** (upstream MD5 scheme), not RFC 4122 v3.
* `read_safe_utf` / `read_safe_int` reproduce upstream limits — the limit values are part
  of the protocol, not arbitrary.

Every new packet or model **must** come with a round-trip test that asserts the exact
byte sequence, not just `encode → decode == original`.

## 5. Pumpkin plugin ABI invariants

Derived from the pinned API source and verified against a real wasm build; re-read the
pinned API before changing plugin structure.

* Target is `wasm32-wasip2` (a **component**). `wasm32-wasip1` modules will not load.
* `[lib] crate-type = ["cdylib"]`.
* A plugin **never calls `wit_bindgen::generate!` itself** — `pumpkin-plugin-api` does it.
* Registration is exactly one `register_plugin!(MyPlugin);`, and it lives inside the
  WASI-only `glue` module (rule 6) rather than at the crate root, so the host build never
  sees the ABI.
* Only `Plugin::new` and `Plugin::metadata` are required; the rest are defaulted.
* **IPC (`send-ipc-message`) is a control plane, not a data plane.** It is synchronous and
  carries `Vec<u8>`; never push audio frames through it. Audio goes over the UDP socket.
* The guest socket is policy-gated by the host. `network.udp.bind` implies receive;
  sending back to clients additionally needs `network.udp.connect` / `network.udp` /
  `network.outbound` / `network.udp.outgoingdatagram`.
* `plugins.loopback_only` must be `false` for a publicly reachable voice server.

## 6. Style

* `crates/plasmo-voice-core` uses Rust **2021**; the plugin crate uses **2024** (Pumpkin
  requires it). Do not bump the core edition without a reason.
* `cargo fmt` before committing. Keep the diff minimal; do not reformat untouched code.
* Public items in the core get doc comments that name the upstream Java counterpart
  (e.g. "`PacketUdpCodec.MAGIC_NUMBER`"). Keep that habit — it is how the wire format is
  audited.
* Errors are `VoiceError` variants with actionable messages; do not `unwrap()` in library
  code paths that can see remote data.
* Prefer explicit, boring code over clever abstractions in the codec. Wire code is read far
  more often than it is written.

## 7. Testing

```powershell
cargo test --workspace                   # whole workspace
cargo test -p plasmo-voice-core          # one crate
cargo fmt --all -- --check               # formatting
cargo clippy --workspace --all-targets -- -D warnings
```

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) is the canonical definition of what
"passing" means. If a command here and the workflow ever disagree, the workflow wins — and
fixing the disagreement is part of your change.

* Add tests beside the implementation (`#[cfg(test)] mod tests`) for unit round-trips, and
  use the recon dumps as expected byte fixtures where available.
* Cover the **negative** paths: bad magic, unknown id, wrong direction, truncated body,
  trailing bytes, over-long UTF.
* Do not weaken an existing assertion to make a change pass. If an assertion is genuinely
  wrong, fix it *and* note in the commit message which upstream behaviour it conflicted with.

## 8. Commits

* Conventional-commit subject (`feat:`, `fix:`, `docs:`, `chore:`, `test:`), imperative,
  ≤ 72 chars.
* Group reconnaissance artefacts and code separately — a commit that changes the wire
  format must not also shuffle `.recon/` files.
* Never commit `target/` or editor/OS files. **`Cargo.lock` stays tracked**: it pins the
  exact Pumpkin API commit the git tag resolves to, and CI fails if it goes missing.
  Reconnaissance scratch is gitignored under `.recon/`.

## 9. Known traps

* `cargo test` walks `workspace.members`; anything added there gets built and tested. Add
  scratch/probe crates as `exclude`, never as members.
* The Pumpkin workspace denies a large clippy set (`unwrap_used`, `panic`, `print_stdout`,
  …). If the plugin is built inside a Pumpkin-derived workspace, use `tracing`, not
  `println!`, and avoid `unwrap()`/`expect()` outside tests.
* `std::net` UDP on wasip2 is real but young: only bind/send/receive are permitted by host
  policy; anything else traps.
* Do **not** reintroduce `cargo-component`. Version 0.21.1 (current) cannot decode the
  component its own `wit-bindgen` 0.62 emits — it fails with `enum tag name 'generic-9x1'
  is not in kebab case` *after* writing a valid component. `wasm32-wasip2` already produces
  a component, so plain `cargo build --target wasm32-wasip2` is both sufficient and
  correct.
* Editing `.recon/*.jsonl` session logs is never useful — they are raw captured history.

## 10. Current work queue

1. **The connect → config → relay path is complete end to end.** The `Plugin` impl, the
   in-guest UDP server (`network.udp.bind`), the keep-alive/timeout sweep and persistence
   into `context.get_data_folder()` are all in place.

   **The control plane runs in-guest over the Minecraft plugin-message channel**
   (`plasmo:voice/v2`, upstream's `BaseVoiceServer.CHANNEL_STRING`), which is the transport
   upstream itself uses — not IPC. Pumpkin 0.2.0 exposes both halves
   (`PlayerCustomPayloadEvent` / `java-player::send-custom-payload`), so there is no
   companion plugin and no IPC hop in the voice path. Module split:

   * `channel.rs` — the only module that names the channel API. Host events in,
     `Outbound` messages out.
   * `control.rs` — the state machine: the three-phase handshake (`PlayerInfoRequestPacket`
     with 1/3/5/10/15 s retries → version gates → `ConnectionPacket`), the registration
     burst (`ConfigPacket` → `PlayerListPacket` → `PlayerInfoUpdatePacket`), and every
     serverbound packet's reply. The channel name lives here too, because `channel.rs` is
     `cfg`-gated to WASI and its code is therefore never unit-tested on the host.

   Two failure modes in this area are silent from both ends, so do not "simplify" either
   away:

   * The channel name must stay exactly `plasmo:voice/v2`. The client subscribes to that
     string; a payload on `plasmo:voice` is dropped without an error on either side, and the
     client reports the server as having no voice plugin.
   * The handshake starts on **`PlayerJoinEvent`**. The client never announces the channel
     — it only answers `PlayerInfoRequestPacket` — so a channel-registration trigger can
     wait forever. `ChannelRegisterHandler` is a secondary trigger, not the primary one.
   * `LanguagePacket` (id 6) is **not cosmetic**. The volume tab labels the proximity source
     line with `translatable(sourceLine.getTranslation())` = `pv.activation.proximity`, and
     the mod's own `lang/en_us.json` does not define that key, so an empty translation map
     makes the client render the raw key. Ship the one entry upstream ships
     (`client_language()` in `config.rs`); do not "simplify" it back to an empty vector.

   The `plasmo:voice/v2` IPC namespace remains as an optional *programmatic* front door
   (there are still tests that drive it); it is no longer how a real client connects.

   Invariants in `server.rs` that look like bugs if you do not know the upstream:
   * **An unregistered secret is dropped without a reply** — never relax that to "register
     anyone who pings", or a public UDP port becomes an open audio relay.
   * **The first datagram of a session is only a registration.** It creates the connection
     and triggers the burst; its body is never handled or relayed, because upstream's
     `handlePacket` sits in the branch that datagram does not take.
   * the server **never answers a ping** — a real client answers *any* inbound ping with
     another ping, so echoing pings back ping-pongs without bound. The server sends its own
     keep-alive pings instead (immediately, then every 1.5–3 s); a client goes soft-dead
     after 7 s without one and drops the connection at 30 s.
   * a client that changes UDP address is *followed* (`setRemoteAddress`), not duplicated.
   * a ping's `serverIp` / `serverPort` is **informational** (`connectionAddress`); the send
     target is always the address the datagram arrived from (`remoteAddress`). Swapping the
     two makes the server talk to itself.
   * audio is relayed as a **new** `SourceAudioPacket` per listener (same payload bytes, the
     listener's own secret, a fresh timestamp), never as the inbound `PlayerAudioPacket`.
2. Integration coverage lives in two files under `crates/plasmo-voice-plugin/tests/`:
   `lifecycle.rs` drives whole sessions through the public API (control message → minted
   secret → decoded `ConnectionPacket` → UDP registration → registration burst → proximity
   relay → keep-alive → disconnect), and `socket.rs` runs the same relay over three real
   `UdpSocket`s so the receive loop, the address parsing and the send path are covered too —
   that is where a `parse::<SocketAddr>` mistake would silently drop every outgoing frame
   while the unit tests stayed green. Keep the WASI-only glue thin so this stays possible:
   anything host-testable (for example `player_connect_reply`, the reply builder) belongs in
   `lib.rs` **outside** the `cfg` gate.
3. Known gaps, in the order they matter (all documented in `README.md` under "What is not
   implemented yet"): `ConfigPacket.encryption` is `null`, so **audio is plaintext** (no RSA
   in the guest yet); only the proximity activation and source line exist, with no TOML
   config; permissions and `canSee`/vanish are not enforced; there is no server-side mute
   manager; positions come from `PlayerMoveEvent` rather than live world reads; and the
   decoration packets (`ConfigPlayerInfo`, `DistanceVisualize`, `AnimatedActionBar`, the
   addon/entity/static source variants) are not sent. Do not describe the plugin as "fully
   compatible" while those are open.
4. **Keep this file and `README.md` in sync with reality.** If a claim in either document
   is wrong, fixing it is part of your change.
