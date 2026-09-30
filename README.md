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

## Logs

A loaded server prints, at startup:

```text
[INFO] the Plasmo Voice server is listening on UDP port 51572 (tick handler 0, channel handlers [1, 2, 3, 4, 5])
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

* **Audio is plaintext** — `ConfigPacket.encryption` is `null`.
* Only the **proximity** activation and source line exist; there is no config file.
* **Permissions and `canSee`/vanish are not enforced**, and there is no server-side mute
  manager.
* **Positions come from move events**, not live world reads.
* **Decoration packets** (`ConfigPlayerInfo`, `DistanceVisualize`, `AnimatedActionBar`, the
  addon/entity/static source variants) are not sent.
* No jitter buffer, reordering or packet-loss concealment — frames are relayed as they arrive.

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

## License

MIT — see [LICENSE](LICENSE).
