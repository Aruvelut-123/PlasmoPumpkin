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
7. **One licence — LGPL-3.0 — and keep the credit.** The whole repository is LGPL-3.0,
   because it reimplements Plasmo Voice's protocol from their source and bundles their
   locale data (`crates/plasmo-voice-plugin/languages/`, verbatim from
   `plasmoapp/plasmo-voice-crowdin` branch `pv`, plus the jar's `en_us.toml`). `LICENSE` is
   the verbatim FSF text, the same bytes upstream ships. The credit in `README.md`
   ("Credits") is what that licence asks for: do not delete it, do not strip the
   `SPDX-License-Identifier: LGPL-3.0-only` line from the locale files, and keep both
   `Cargo.toml`s at `license = "LGPL-3.0-only"`.
8. **Bump only the crates a change actually touches.** The two crates are versioned
   independently, and a crate that this change does not modify keeps its version — no
   "keep the numbers in step" bump, no bumping `plasmo-voice-core` because the plugin moved.
   A change that alters the core's public API or its wire behaviour bumps the core; a change
   that only adds server behaviour on top of an unchanged core bumps the plugin and leaves
   the core where it was. Say in the commit message which crate moved and why. Release tags
   name the project as a whole (`v0.2.0`), which is a separate thing from a crate version:
   never tag a version just because it is unused.

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
* The subject is not only for the log: `.github/workflows/release.yml` publishes it verbatim
  as a changelog line when a `v*` tag is pushed (see §11, "Releasing"). Write it for someone
  reading the release page, not for the diff.

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
     makes the client render the raw key. See the translations block below.

   ### Translations (`src/language.rs`, `languages/`)

   Upstream keeps its table in `languages/list` plus one `languages/<name>.toml` per locale
   inside its jar, tops it up from Crowdin at runtime, and answers a `LanguageRequestPacket`
   with the **client** scope of that locale
   (`VoiceServerLanguages.getClientLanguage` → `PlayerChannelHandler:203-213`). A guest
   cannot read files it was not handed, so the same two things are compiled in with
   `include_str!`: `languages/list` is upstream's list (`readLanguagesList`) and
   `language.rs`'s `FILES` table is the `languages/` directory.

   `src/language.rs` reproduces `getLanguage`/`languageToMapOfStrings`, and each of these
   details is load-bearing:

   * the request is lowercased before the lookup (`languageName?.lowercase()`), which is why
     Minecraft's `zh_cn` finds a file named `zh_cn.toml` — keys in `languages/list` are
     lowercase locale codes, not upstream's Crowdin directory names (`zh_CN`);
   * a locale this server does not ship falls back to `en_us`
     (`FALLBACK_LANGUAGE`, `getLanguage(null, scope)`), and `en_us` also fills any key a
     translation does not carry (`fillMissing` → `putIfAbsent`, so the locale's own value
     wins);
   * nested tables are flattened with `.` and the scope name is dropped, so
     `[client.pv.activation] proximity = "…"` becomes `pv.activation.proximity`. The
     `server` scope is ignored: a `LanguagePacket` only carries the client half.
   * the reader is a deliberate subset of TOML (table headers, `key = "basic string"`,
     escapes, `#` comments). A value that is not a basic string is skipped rather than
     guessed at; do not grow it into a general parser — if the data ever needs more, revisit
     the data, not the parser.

   The files are the client-scope half of upstream's `server.toml` from
   `github.com/plasmoapp/plasmo-voice-crowdin`, branch `pv` (the branch
   `BuildConstants.GITHUB_CROWDIN_URL` archives), plus the `en_us.toml` from the jar. To
   refresh: fetch `contents/<locale>/server.toml?ref=pv` from the GitHub API, keep the
   `[client.*]` tables, write them to `languages/<lowercase locale>.toml`. Then
   `cargo test -p plasmo-voice-plugin`: `the_language_list_matches_the_files` fails if the
   list and the table disagree, and `every_locale_speaks_the_proximity_line` fails if a file
   stops defining the key the volume tab renders (it asserts against
   `config::PROXIMITY_TRANSLATION`, so the config and the table cannot drift apart).

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
3. Known gaps, in the order they matter (the user-facing summary is `README.md` →
   "Limitations"): only the proximity activation
   and source line exist, and the plugin's own `config.toml` covers only the
   server-runner knobs (`port`, `keep_alive_timeout_ms`, `advertised_ip`,
   `max_datagrams_per_tick`, `sample_rate`, `mtu_size`, `distances`,
   `default_distance`, `max_extra_audio_broadcast_distance`, `notify_unmuted`);
   permissions **are** enforced (`pv.mute`/`pv.unmute`/`pv.mutelist`, op by
   default, double-checked inside the handlers), both target commands take
   `@`-selectors, and the client-side duration suggestions are not mirrored;
   vanish **is** honored: `tick.rs` re-mirrors the host's `canSee` into
   `server.set_hidden_players` once a second and `proximity_listener_ids` mutes a
   vanished pair in both directions; positions are **live** — `tick.rs` re-reads
   every registered player's position from the host every tick, with the
   `PlayerMoveEvent` handler in `channel.rs` (`MoveHandler`) kept as a sub-tick
   hint — and `ConfigPlayerInfo` is not re-sent when a player's permissions change
   mid-session (`AnimatedActionBar` and the addon/entity/static source variants are
   not sent either — upstream's own server never sends the former and never creates
   the latter; `DistanceVisualize` **is**, on
   proximity distance changes — see `control.rs`'s `on_activation_distances`). Do not
   describe the plugin as "fully
   compatible" while those are open.
   The missing-public-key fallback is a design choice, not a gap: a real client always
   generates an RSA key pair and sends the public half in `PlayerInfoPacket`
   (`ModServerConnection.generateKeyPair` / the `PlayerInfoPacket` send at
   `ModServerConnection.java:354`), so the fallback is nearly unreachable. Upstream's
   `sendConfigInfo` does `receiver.getPublicKey().orElseThrow(...)` and then `return`s from
   the `catch`, so it never sends that player a `ConfigPacket` at all
   (`VoiceTcpServerConnectionManager.java:109-125`). This server instead logs the failure
   and sends a `ConfigPacket` with `encryption: None`, which the client accepts natively —
   `ConfigPacket.encryption` is `@Nullable` and `ModServerConnection.handle` leaves
   `Encryption` null when it is. Describe it in "How it works", never in "Limitations", and
   do not present the fallback as parity with upstream: upstream refuses the client.
4. **Keep this file and `README.md` in sync with reality.** If a claim in either document
   is wrong, fixing it is part of your change. Keep the split deliberate: `README.md` is a
   short quick start plus the user-facing limitations, and the reference material below
   lives here.

## 11. Reference

Moved out of `README.md` so that file stays a quick start. Nothing here is optional reading
for a change in the area it describes; the code comments name the upstream counterpart.

### The control plane, phase by phase

Upstream has **no TCP listener**: the 26 "TCP" packets (ids `0x01`–`0x1a`, covering
handshake, config, player/source state, activations and source lines) travel over the
Minecraft plugin-message channel. `ServerChannelHandler` decodes them from plugin-message
bytes with `PacketDirection.SERVER`, and `PacketTcpCodec` is used on both sides; only UDP is
a real socket. A session is established in three phases:

1. **Join.** The server asks who the client is with a clientbound `PlayerInfoRequestPacket`
   (id 2), retrying at +1/3/5/10/15 s until an answer arrives.
2. **Answer.** The client replies with `PlayerInfoPacket` (id 10): its mod version and an RSA
   public key. The server gates on the *major* version (and a `2.0.0` minimum), mints the
   player's secret with `VoiceUdpServerConnectionManager.getSecretByPlayerId`, and replies
   with a clientbound `ConnectionPacket(secret, ip, port)` (id 1).
3. **Registration.** The client pings over UDP until it hears back. On the first datagram
   whose secret is known, `NettyPacketHandler` creates the connection and answers with the
   registration burst `ConfigPacket` (id 3) → `PlayerListPacket` (id 7) →
   `PlayerInfoUpdatePacket` (id 8), in that order.

The `plasmo:voice/v2` IPC namespace remains an optional *programmatic* front door (other
plugins, scripts, and several tests drive it). It can mint a session without a real client:

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

### Module map (plugin crate)

| Module | Role |
| --- | --- |
| `channel.rs` | the ABI shim: host events in, `Outbound` messages out. The only module that names the Pumpkin channel API. |
| `control.rs` | the state machine: the handshake, the burst, and every serverbound packet's reply, plus the `DistanceVisualize` reply on an activation-distance change. Host-testable, no Pumpkin types. |
| `config.rs` | the `ConfigPacket` this server advertises, the version/distance gates, and the plugin's own `config.toml` (the `port` / `keep_alive_timeout_ms` / `advertised_ip` / `max_datagrams_per_tick` / `sample_rate` / `mtu_size` / `distances` / `default_distance` / `max_extra_audio_broadcast_distance` / `notify_unmuted` knobs). |
| `commands.rs` | the `/vmute`, `/vunmute` and `/vmutelist` chat commands (WASI only). |
| `mute.rs` | the persisted mute store (`mutes.toml`), the duration parser, and the mute/unmute notice texts. |
| `language.rs` | the translation table and the locale lookup (see §10). |
| `server.rs` | the UDP semantics: per-connection secrets, activation- and position-filtered fan-out, keep-alive. |
| `runtime.rs` | the process-global socket, the bind, and the bounded per-tick pump (the budget comes from `config.toml`'s `max_datagrams_per_tick`). |
| `state.rs` | the persisted state: port, server secret, protocol version. |
| `tick.rs` | the `ServerTickStartEvent` bridge (WASI only). |
| the `glue` module in `lib.rs` | the `Plugin` impl and the IPC front door (WASI only). |

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
* `PlayerAudioEndPacket` (over the control plane) becomes a clientbound `SourceAudioEndPacket`
  (id 18) for the same listeners, plus a `SelfSourceInfoPacket` (id 17) for the speaker with
  `sequenceNumber = -1`.
* A listener that hears an unknown source asks for it with `SourceInfoRequestPacket`; the
  server answers `SourceInfoPacket` (id 16) with a `PLAYER` source carrying the speaker's
  nick, state, proximity line and an Opus decoder.
* `PlayerStatePacket` updates the player's mute flags and is re-broadcast as
  `PlayerInfoUpdatePacket` (id 8) only when something actually changed.

### Ping and keep-alive

The two directions of `PingPacket` are not symmetric, and getting that wrong is a real bug:

* **A client's ping is a registration request / keep-alive ack, and the server never answers
  it.** A real client replies to *any* inbound ping with another ping, so echoing pings back
  would produce an unbounded ping-pong between server and client.
* **The server sends its own keep-alive pings** — an empty `PingPacket` (a timestamp and
  nothing else), immediately for a new connection and then every 1.5–3 s (a 1.5 s base plus
  up to 1.5 s of jitter derived from the secret). That first ping is what makes a real client
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
* Both directions are driven from the one clock the guest has: the Pumpkin tick event, since
  Pumpkin 0.2.0 puts no `on_tick` on the `Plugin` trait.

### Protocol summary

Plasmo Voice has **one transport** — UDP, the audio data plane and the only path a voice
server actually needs to serve — plus the control plane above. Every UDP datagram is prefixed
by a fixed header:

```text
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

`Direction` uses upstream's `PacketDirection` naming, which is easy to misread: **`CLIENT`
means the *client receives* it** (clientbound, server → client) and **`SERVER` means the
*client sends* it** (serverbound, client → server). Checked against the upstream packages:
`PlayerAudioPacket` lives in `udp.serverbound`, while `SourceAudioPacket` and
`SelfAudioInfoPacket` live in `udp.clientbound`. This is why the server decodes every
inbound datagram with `PacketDirection::Server`.

### Encoding rules (why this is not plain "Rust default")

The upstream implementation writes through Guava's `ByteArrayDataInput/Output`, so the core
crate reproduces those semantics exactly:

* all multi-byte integers, longs, floats and doubles are **big-endian**;
* strings use Java's **modified UTF-8** (`DataOutput.writeUTF`): `0x0000` encodes as two
  bytes, astral code points become a 6-byte surrogate pair, length is a **u16** prefix;
* `VoiceActivation::generate_id` / `VoiceSourceLine::generate_id` use a name-based UUID
  whose MD5 is taken **over the name bytes only** (no RFC 4122 namespace/version bits);
* "safe" reads (`readSafeUTF`, `readSafeInt`) clamp and reject out-of-range values the same
  way the Java code does.

IDs are derived from the packet struct, mirroring `PacketTcpCodec.getType(...)` and
`PacketUdpCodec.getType(...)`, including the `0x100` custom id.

### Deploying into Pumpkin

Pumpkin reads the plugin's metadata — and its permission list — out of the component itself:
there is no manifest file, and `plugins/*.wasm` is scanned automatically. A permission
decision is reached in this order:

1. an `allowed_permissions` entry (`[plugins]`, or a `[plugins.overrides."<name>"]` block)
   approves without asking;
2. otherwise a cached decision in `plugins/permission_cache.json` is reused;
3. otherwise, with `plugins.ask_permission_confirmation = false`, everything is
   auto-approved;
4. otherwise the host prompts on the console.

A server started without a TTY **cannot prompt**, so the prompt fails and the denial is
cached:

```text
[WARN] Console readline is not available; cannot prompt for plugin "plasmo-voice" permissions
[WARN] Permission denied for plugin "plasmo-voice", skipping loading.
```

That means *the plugin is not running at all*. The denial is cached per plugin and survives
restarts and even plugin updates, so clear `plugins/permission_cache.json` or pre-approve.
The permissions this plugin needs:

| Permission | Why |
| --- | --- |
| `network.udp.bind` | Bind the voice UDP socket. Implies receive. |
| `network.udp.connect` *(or* `network.udp`*/*`network.outbound`*)* | Required to **send** datagrams back to clients from the unconnected socket. |
| `fs.read.data` / `fs.write.data` | Persist server state (secret UUID, activations, source lines) in the plugin data folder. |
| `network.dns` | Only if hostnames are resolved. |

`plugins.loopback_only` must be `false` for clients on other machines, and the voice UDP
port must be forwarded: forwarding only the Minecraft TCP port is not enough, and a host
panel's firewall usually has to be told about UDP explicitly.

### Build notes

* The historical `PUMPKIN_DIR` checkout override is no longer needed — the tag dependency
  fetches the API from GitHub. To deliberately build against a local, unreleased Pumpkin
  checkout, add a `[patch]` entry pointing at `crates/pumpkin-plugin-api`; do not change the
  published requirement.
* `cargo-component` is deliberately not used (see §9) — plain
  `cargo build --target wasm32-wasip2` is sufficient and correct.
* There are no wrapper scripts. `.github/workflows/ci.yml` is the only definition of the
  build, so nothing can drift out of sync with it.

### Releasing

Pushing a `v*` tag is the whole procedure:

```powershell
git tag v0.2.0
git push origin v0.2.0
```

[`.github/workflows/release.yml`](.github/workflows/release.yml) then builds the component,
checks that the file really is a component (and not a core module), generates the release
notes from the commits since the previous `v*` tag, and attaches both to the GitHub release.
**Nobody writes release notes by hand.** GitHub's own `generate_release_notes` is deliberately
*not* used: it lists merged pull requests, and this repository commits straight to `main`, so
all it ever produced was a bare "Full Changelog" link. Instead the notes are the
conventional-commit subjects of §8, grouped by type, with the pinned Pumpkin API version read
out of the manifest. A subject that is not a conventional commit lands under "Other" rather
than being dropped.

Two things to know before cutting a release:

* A tag-triggered run executes the workflow file **as of that tag**, so a release cut from a
  commit older than a workflow change keeps the old behaviour. Tag a commit that carries the
  workflow you want. (The first release, `v0.1.0`, was cut before the changelog generator
  existed and needed one refresh.)
* `gh workflow run release.yml --ref main -f tag=v0.1.0` regenerates the notes of a tag that
  already exists. It replaces the release body and uploads **nothing**: that run checks out
  the branch, not the tag, so its component is not the bytes that tag released.

### Attribution and licensing

The credit is in [`README.md`](README.md) ("Credits"). The licence is **LGPL-3.0-only for
the whole repository** — not a per-part split — because the project reimplements Plasmo
Voice's protocol from their published source and bundles their locale data, so the licence
upstream chose is the honest one for all of it.

`LICENSE` is a **verbatim** copy of the FSF LGPL-3.0 text, and of the file upstream ships
(`plasmoapp/plasmo-voice`, blob `0a041280bd00a9d068f503b8ee7ce35214bd24a1`). The FSF forbids
modifying the licence text, so provenance notes like this one live outside it; re-verify a
re-fetch with `git hash-object LICENSE`, which must still print that blob. LGPL-3.0
incorporates GPL-3.0 by reference (§3, §6); upstream ships only the LGPL text, and so do we.
