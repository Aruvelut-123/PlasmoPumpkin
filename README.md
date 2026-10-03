# PlasmoPumpkin

A [Plasmo Voice](https://github.com/plasmoapp/plasmo-voice)-compatible voice server for
[Pumpkin](https://github.com/Pumpkin-MC/Pumpkin), written in Rust and shipped as a
`wasm32-wasip2` WebAssembly component. Players who have the Plasmo Voice mod connect over
UDP and hear each other by proximity, using the normal in-game voice settings screen.

Verified against **Plasmo Voice 2.1.17** on **Minecraft 26.3** (Pumpkin `0.2.0+26.3-26.51`).

## Workspace

| Crate | Target | What it is |
| --- | --- | --- |
| [`plasmo-voice-core`](crates/plasmo-voice-core) | any (host) | the Plasmo Voice wire format: UDP + TCP codecs, packet registry, data models |
| [`plasmo-voice-plugin`](crates/plasmo-voice-plugin) | `wasm32-wasip2` | the voice server: control plane, UDP audio relay, keep-alive, persistence, translated `LanguagePacket` replies |

## Status

| Check | State |
| --- | --- |
| `cargo test --workspace` | ✅ 116 tests |
| `cargo fmt --all -- --check` | ✅ clean |
| `cargo clippy -D warnings`, host and `wasm32-wasip2` | ✅ clean |
| `wasm32-wasip2` component build | ✅ builds and loads in Pumpkin |
| CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) | ✅ `core`, `policy`, `plugin`, `hygiene` |

## Quick start

```powershell
rustup target add wasm32-wasip2
cargo build --target wasm32-wasip2 -p plasmo-voice-plugin --release
```

Copy `target/wasm32-wasip2/release/plasmo_voice_plugin.wasm` to the server's
`plugins/plasmo-voice.wasm`, then pre-approve its permissions in `pumpkin.toml`:

```toml
[plugins]
enabled = true
ask_permission_confirmation = false
allowed_permissions = [
    "network.udp",              # bind the voice socket (implies receive)
    "network.udp.connect",      # send datagrams back to clients
    "network.udp.outgoingdatagram",
    "fs.read.data",             # persisted state, and the translations it reports
    "fs.write.data",
]
```

Keep `plugins.loopback_only = false`, and forward the **voice UDP port** the plugin prints
at startup — it is a second port, separate from Minecraft's. Until the permission prompt is
answered — on a headless server it cannot be, and Pumpkin then caches the denial in
`plugins/permission_cache.json` — the plugin is not running at all, so the block above is
the way out.

## How it works

* **End-to-end audio encryption** — at registration the server generates a 16-byte AES
  key and wraps it per client with RSA/PKCS1v15, exactly like upstream
  `VoiceTcpServerConnectionManager`; the `ConfigPacket.encryption` field carries it, and
  the already-encrypted Opus frames are relayed verbatim. The server never touches the
  audio cipher. The protocol leaves `encryption` nullable and the client
  (`ModServerConnection.handle`) natively runs plaintext when it is — so a client that
  sends no usable public key gets a `ConfigPacket` without one and speaks in the clear,
  where upstream instead aborts the config packet entirely for that client.
* **Proximity voice** — players hear each other by distance, with the activation and
  source line from the in-game voice settings screen. When a player changes the
  distance, everyone who hears them is sent a `DistanceVisualize` circle update —
  after the first set, exactly like upstream `onActivationDistanceChange`.
* **Server-side mutes** — `/vmute`, `/vunmute` and `/vmutelist` enforce silence in
  voice chat (the relay drops the muted player's frames and their mute state is
  broadcast to every voice client), persist to `mutes.toml`, and announce to the
  muted player with upstream's own wording — `"You've been muted …"`, and
  `"You've been unmuted"` when a temporary mute expires (`voice.notify.unmuted`).
* **Hosted configuration** — on first run the plugin writes a commented
  `config.toml` template (with the state file, in the plugin data folder); the UDP
  port, keep-alive timeout, advertised IP, the per-tick datagram budget, the audio
  sample rate and MTU size, the proximity distances (`8,16,32`), the default
  distance, the extra broadcast radius and the unmute notification are read from it
  at load. Every key is optional — delete one to keep the default.
* **Localized replies** — `LanguagePacket` is answered in the client's own locale.

## Logs

A loaded server prints, at startup:

```text
[INFO] the Plasmo Voice server is listening on UDP port 51572 (tick handler 0, channel handlers [1, 2, 3, 4, 5], 3 commands)
[INFO] the voice tick pump is running: the UDP socket on port Some(51572) is being drained
```

A client joining adds:

```text
[INFO] starting the voice handshake with <player>
[INFO] a voice client identified itself: <uuid> on <mod version> (mc <mc version>), told to use UDP <ip>:<port> with secret <uuid>
[INFO] a voice client opened its UDP connection from <address> (player <uuid>)
[INFO] the client asked for the server's translations; replying with 1 entries
```

* No startup line at all → the plugin is not loaded; see the permission trap above.
* The startup lines but no handshake → Pumpkin is not delivering the join event.
* A handshake but no "opened its UDP connection" → the client's datagrams never arrive:
  address, UDP port, firewall. That is exactly the client's *"Cannot connect to the UDP
  server. It's likely that the UDP port is closed."*

## Translations

`LanguagePacket` is answered in the client's own locale from
[`crates/plasmo-voice-plugin/languages/`](crates/plasmo-voice-plugin/languages) — 18 locales,
with `en_us` filling what a translation does not carry. It is not cosmetic: without it the
voice settings screen prints the raw `pv.activation.proximity` key in place of the source
line's name.

## Limitations

The connect → config → relay path works end to end, but this is not full upstream parity:

* Only the **proximity** activation and source line exist, and the plugin's own
  `config.toml` covers only the server-runner knobs (`port`, `keep_alive_timeout_ms`,
  `advertised_ip`, `max_datagrams_per_tick`, `sample_rate`, `mtu_size`, `distances`,
  `default_distance`, `max_extra_audio_broadcast_distance`, `notify_unmuted`,
  `notify_muted`, `default_language`, `forced_language`, `client_mod_min_version`) —
  there is no per-world voice config yet.
* **Vanish is honored**: a periodic `canSee` sweep (once a second) mirrors the host's
  hide/show state into the relay, and a vanished pair goes silent in **both** directions.
* **Mutes are enforced**: `/vmute`, `/vunmute` and `/vmutelist` are gated on the
  upstream nodes `pv.mute`, `pv.unmute` and `pv.mutelist` (operator-only by default),
  and the handlers re-check the same node. The two target commands accept names,
  UUIDs and `@`-selectors; mutes persist across restarts in `mutes.toml`, lift on
  schedule, and the muted player's client hears about every change.
* **Positions are live**: the relay re-reads every voice player's position from the
  host on a per-tick schedule (the move event is only a sub-tick hint), so distance
  filtering never depends on events arriving.
* **Permissions are live**: `ConfigPacket` still carries upstream's default
  `pv.allow_freecam: true`, but the server re-reads the host's real permission
  every second and re-sends `ConfigPlayerInfo` as soon as it differs — a
  mid-session change reaches the client the way upstream's event would. Only
  **player** voice sources exist — no addon/entity/static variants, like upstream's
  own server. (`DistanceVisualize` **is** sent on proximity distance changes.)
* No server-side jitter buffer, reordering or packet-loss concealment — frames are
  relayed as they arrive, and the client's own jitter buffer smooths the stream (the
  same split upstream relies on).

## Development

Contributor rules, the protocol and plugin-ABI invariants, the upstream reference notes and
the longer list of gaps live in [`AGENTS.md`](AGENTS.md). The commands CI runs are the
canonical ones:

| Task | Command |
| --- | --- |
| Whole workspace tests | `cargo test --workspace` |
| Format check | `cargo fmt --all -- --check` |
| Host lint | `cargo clippy --workspace --all-targets -- -D warnings` |
| Component lint | `cargo clippy --target wasm32-wasip2 -p plasmo-voice-plugin --all-targets -- -D warnings` |
| Component build | `cargo build --target wasm32-wasip2 -p plasmo-voice-plugin --release` |

There are no wrapper scripts; if this table and the workflow ever disagree, the workflow
wins.

## Credits

This server speaks the protocol of **[Plasmo Voice](https://github.com/plasmoapp/plasmo-voice)**
by [plasmoapp](https://github.com/plasmoapp), and could not exist without it. Packet ids,
field layouts and wire semantics were reimplemented from `su.plo.voice.proto`, and the locale
files under
[`crates/plasmo-voice-plugin/languages/`](crates/plasmo-voice-plugin/languages) are upstream's
own translations, taken from
[plasmoapp/plasmo-voice-crowdin](https://github.com/plasmoapp/plasmo-voice-crowdin). Thank you.

## License

**LGPL-3.0** — see [LICENSE](LICENSE). The licence Plasmo Voice itself uses, and therefore
the one this project uses too.
