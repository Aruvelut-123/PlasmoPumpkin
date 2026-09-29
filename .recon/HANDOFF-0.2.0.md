# Handoff: Pumpkin API 0.2.0 upgrade — follow-up work for the host agent

**Status:** the version upgrade is **done, committed, and pushed** (`10fedec` on `main`).
Compiling the plugin against the new API exposes **two API-shape differences** that were
deliberately **not** fixed here. They are the host agent's work.

---

## 1. What was done (do not redo)

`crates/plasmo-voice-plugin/Cargo.toml`:

```toml
pumpkin-plugin-api = { git = "https://github.com/Pumpkin-MC/Pumpkin", tag = "0.2.0+26.3-26.51" }
```

* Upgraded `0.1.0-dev+26.2-26.45` → **`0.2.0+26.3-26.51`** (latest stable).
* Pinned as a **git tag**, not a crates.io version. Regional mirrors lag and only carry
  `0.1.0-dev`; resolving via crates.io fails with *"failed to select a version for the
  requirement `pumpkin-plugin-api = "^0.2.0"`"*. The tag bypasses mirrors and pins commit
  `204a94e`. This mirrors what `D:\TrChat-Neoforge\pumpkin` does.
* `Cargo.lock` is now tracked (removed from `.gitignore`) so the pin is reproducible.
* `Cargo.toml` (workspace) excludes `.recon/probe`, a throwaway recon crate that pinned the
  stale `0.1.0-dev` and dragged it into the lockfile.
* Version policy documented in `README.md` ("Pumpkin API version policy") and `AGENTS.md`
  (rule 3): use the latest stable API, never an older prerelease, and treat an out-of-date
  pin as a bug.

**Verified:** `cargo test -p plasmo-voice-core` → **39 passed, 0 failed**.

---

## 2. What is broken, and what the host agent must decide

`cargo check --workspace` now fails with **two errors and one warning**. These are genuine
ABI differences between `0.1.0-dev` and `0.2.0`, not incidental breakage.

### Error 1 — `on_tick` is not a `Plugin` trait method in 0.2.0

```
error[E0407]: method `on_tick` is not a member of trait `Plugin`
   --> crates\plasmo-voice-plugin\src\lib.rs:439:5
```

The 0.2.0 `Plugin` trait (`crates/pumpkin-plugin-api/src/lib.rs:443`) has only:

| Method | Signature |
| --- | --- |
| `new` | `fn new() -> Self where Self: Sized` |
| `metadata` | `fn metadata(&self) -> PluginMetadata` |
| `on_load` | `fn on_load(&self, _context: Context) -> Result<()>` |
| `on_unload` | `fn on_unload(&self, _context: Context) -> Result<()>` |
| `handle_ipc_message` | `fn handle_ipc_message(&self, ...) -> Result<IpcMessage, String>` |

There is **no tick hook at all**. This is the important one, because the plugin's entire
UDP data plane is driven from `on_tick` — see the module doc in `lib.rs`:

> `wasm32-wasip2` has no threads, so this cooperative pump *is* the data plane: every host
> tick drains a bounded batch of datagrams and answers them.

So the voice server currently **has no way to pump its socket** under 0.2.0. The pin was
chosen correctly (0.2.0 is latest stable), so this is an architectural question, not a
version question. Options the host agent should evaluate against the real Pumpkin source:

1. **Event-driven pump.** `pumpkin-plugin-wit/v0.1/event.wit` declares
   `server-tick-start-event-data { tick: s32 }` and `server-tick-end-event-data { tick: s32 }`.
   If those are subscribable as `EventMode` handlers, register one on load and drive
   `pump()` from it. This is the closest replacement for `on_tick`.
2. **Pump from `handle_ipc_message`.** Only correct if something else already calls it
   regularly; it will not sustain real-time audio on its own.
3. **Upstream gap.** If neither works, the plugin needs a host-provided tick/IPC-drive
   mechanism, and that is a Pumpkin-side request.

Note the host-tick cadence question: 20 Hz (Minecraft's tick rate) against a 256-datagram
budget per tick is the figure `MAX_DATAGRAMS_PER_TICK` was sized for. Re-check that
tradeoff against whatever mechanism is chosen.

### Error 2 — `UdpPacket` is not exported at the core crate root

```
error[E0432]: unresolved import `plasmo_voice_core::UdpPacket`
  --> crates\plasmo-voice-plugin\src\server.rs:12:32
```

`server.rs` imports `{ PacketDirection, UdpCodec, UdpPacket, VoiceError }` from
`plasmo_voice_core`, but `UdpPacket` is not re-exported at the root. The accompanying
warning confirms the codec actually lives under a module path:

```
warning: unused import: `plasmo_voice_core::wire::UdpCodec`
  --> crates\plasmo-voice-plugin\src\lib.rs:31:5
```

This is a **`plasmo-voice-core` public-API question**, and the core is the protocol source
of truth — fix it there deliberately rather than papering over it in the plugin:

* Decide whether `UdpPacket` (and friends) should be re-exported from the core root, or
  whether the plugin should import from `plasmo_voice_core::wire::`.
* Whichever is chosen, `lib.rs` and `server.rs` should agree; the unused-import warning is
  a symptom of them currently disagreeing.
* Add a core re-export/test so this cannot silently regress.

---

## 3. Suggested order of work

1. Resolve Error 2 first — it is self-contained and keeps `plasmo-voice-core` coherent.
2. Investigate the 0.2.0 tick/event mechanism for Error 1 against the vendored source at
   `~/.cargo/git/checkouts/pumpkin-2a40f7d907b85f73/204a94e` (that is the exact pinned
   commit) before designing the replacement pump.
3. Re-run `cargo check --workspace`, then `scripts/build-plugin.ps1` for the
   `wasm32-wasip2` component.
4. Update `README.md` / `AGENTS.md` if the data-plane design changes — the `lib.rs` module
   doc still describes the tick-driven pump and will be wrong afterwards.

## 4. Commands

```powershell
cargo test -p plasmo-voice-core          # 39 passed — must stay green
cargo check --workspace                  # currently fails: the 2 errors above
scripts/build-plugin.ps1                 # wasm32-wasip2 component
```
