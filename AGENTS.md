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

## 2. Non-negotiable rules

1. **Never break `cargo test`.** The workspace must build and test cleanly on the host
   before you consider any change done.
2. **Never break the `wasm32-wasip2` build.** The plugin is only useful as a component.
   Verify with `scripts/build-plugin.ps1` after touching plugin code.
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

## 3. Source-of-truth hierarchy

When documents disagree, the order is:

1. the upstream Java source of `su.plo.voice.proto` (local checkout used during recon);
2. `.recon/PLUGIN-INTEGRATION.md` for the Pumpkin plugin ABI (verified against a real
   Pumpkin checkout and a real wasm build);
3. the recon dumps under `.recon/` (`WIRE.txt`, `udp-packets.txt`, `proto-*/`, …);
4. code comments in `crates/*/src`.

`.recon/` is **evidence, not code**. Never let it become a build input, never add it to
`workspace.members`, and never "fix" code to match a stale dump without re-checking the
upstream source.

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

Derived from `.recon/PLUGIN-INTEGRATION.md`; re-read it before changing plugin structure.

* Target is `wasm32-wasip2` (a **component**). `wasm32-wasip1` modules will not load.
* `[lib] crate-type = ["cdylib"]`.
* A plugin **never calls `wit_bindgen::generate!` itself** — `pumpkin-plugin-api` does it.
* Registration is exactly one crate-level `register_plugin!(MyPlugin);`.
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
scripts\test.ps1                 # whole workspace
scripts\test.ps1 plasmo-voice-core
```

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
* Never commit `target/`, `Cargo.lock` (currently ignored for this library-style repo), or
  editor/OS files.

## 9. Known traps

* `cargo test` walks `workspace.members`; anything added there gets built and tested. Add
  scratch/probe crates as `exclude`, never as members.
* The Pumpkin workspace denies a large clippy set (`unwrap_used`, `panic`, `print_stdout`,
  …). If the plugin is built inside a Pumpkin-derived workspace, use `tracing`, not
  `println!`, and avoid `unwrap()`/`expect()` outside tests.
* `std::net` UDP on wasip2 is real but young: only bind/send/receive are permitted by host
  policy; anything else traps.
* Editing `.recon/*.jsonl` session logs is never useful — they are raw captured history.

## 10. Current work queue

1. Implement `plasmo-voice-plugin` fully: `Plugin` impl, `plasmo:voice/v2` IPC handshake
   handler, in-guest UDP voice server (`network.udp.bind`), persistence of the secret UUID
   and state into `context.get_data_folder()`.
2. Add integration tests that drive `UdpCodec`/`TcpCodec` across the full client lifecycle.
3. Keep this file and `README.md` in sync with reality — if the status table in `README.md`
   is wrong, fixing it is part of your change.
