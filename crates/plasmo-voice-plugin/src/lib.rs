//! # plasmo-voice-plugin
//!
//! A **Plasmo Voice–compatible voice server** running as a Pumpkin WebAssembly
//! component.
//!
//! ## Layout
//!
//! | Module | Target | Responsibility |
//! | --- | --- | --- |
//! | [`state`] | any | The persisted server state (port, secret UUID, version), its file I/O, and secret generation. Plain Rust, unit-tested on the host. |
//! | [`config`] | any | The `ConfigPacket` this server advertises, and the version/distance rules the client's packets are gated on. |
//! | [`control`] | any | The control plane state machine: the join handshake, the registration burst, and every serverbound packet's reply. Returns messages to deliver; knows nothing about Pumpkin. |
//! | [`server`] | any | The UDP protocol semantics — packet decoding, per-connection secrets, activation- and position-filtered audio fan-out. Socket-free, so it is fully unit-tested on the host. |
//! | [`runtime`] | any | The process-global socket + protocol state, the socket bind, and the bounded per-tick pump. |
//! | `channel` | WASI | The `plasmo:voice` plugin-message channel: inbound `PlayerCustomPayloadEvent`, outbound `send_custom_payload`, and the join/leave/move events that feed the control plane. |
//! | `tick` | WASI | The Pumpkin event bridge: a `ServerTickStartEvent` handler that drives the pump and delivers the control plane's outgoing messages. |
//! | `glue` | WASI | The `Plugin` impl, `plasmo:voice/v2` IPC handling, and persistence into `context.get_data_folder()`. |
//!
//! ## Why the Pumpkin ABI is gated on `target_os = "wasi"`
//!
//! `pumpkin-plugin-api` generates the component's guest exports, whose symbol
//! names are WIT-mangled (`pumpkin:plugin/metadata@0.1.0#get-metadata`). rustc
//! lists every exported symbol in the ELF *version script* it gives the linker for
//! a `cdylib`, and a version script cannot contain `:`, `@` or `#` — so linking
//! this crate for a native target fails outright. The dependency is therefore
//! declared only for WASI (see `Cargo.toml`), and everything that names it lives
//! in `tick`/`glue`.
//!
//! That split is what makes the interesting half testable: the protocol server,
//! the state format and the pump are ordinary Rust with no Pumpkin types, so
//! they run under `cargo test` on the host. The ABI glue is covered by the
//! `wasm32-wasip2` build and the component-shape checks in CI.
//!
//! ## Threading model
//!
//! `wasm32-wasip2` has **no threads**. The UDP listener therefore cannot own a
//! thread: it is polled cooperatively from a Pumpkin tick event (see `tick`), with
//! a bounded per-tick datagram budget so a burst of voice traffic can never starve
//! the server tick. Audio is never pushed over IPC — IPC is a control plane only.
//!
//! Pumpkin 0.2.0 does not put an `on_tick` callback on the `Plugin` trait, so the
//! tick *event* is the supported clock source; that choice is confined to `tick`.

use plasmo_voice_core::wire::ConnectionPacket;
use plasmo_voice_core::{TcpCodec, TcpPacket};
use uuid::Uuid;

pub mod config;
pub mod control;
pub mod runtime;
pub mod server;
pub mod state;

#[cfg(target_os = "wasi")]
pub mod channel;

#[cfg(target_os = "wasi")]
pub mod tick;

/// The plugin id other plugins address in `send-ipc-message`.
pub const PLUGIN_ID: &str = "plasmo-voice";

/// The IPC protocol namespace for Plasmo Voice v2 handshakes.
///
/// The first line of a well-formed message is this namespace followed by a newline,
/// then a command token.
pub const VOICE_IPC_NAMESPACE: &str = "plasmo:voice/v2";

/// The plugin.
///
/// Deliberately **stateless**: the socket, the protocol state machine and the
/// counters live in the process-global [`runtime`], because the tick handler that
/// drives them is registered with the host and outlives the `&self` borrow that
/// `on_load` hands out. Keeping a second copy here would only invite the two to
/// drift apart.
pub struct PlasmoVoicePlugin;

impl Default for PlasmoVoicePlugin {
    fn default() -> Self {
        // `Plugin::new` only exists under the WASI gate, so this must not call it.
        Self
    }
}

/// A parsed `plasmo:voice/v2` IPC message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceIpc {
    /// `handshake` — a client-facing plugin asks for the voice server's endpoint
    /// and secret so it can point its players at it.
    Handshake,
    /// `status` — report the running server's counters.
    Status,
    /// `player-connect\nplayer=<uuid>\n[name=<name>]\n[ip=<public ip>]` — the
    /// Minecraft side announces that a player joined and wants voice.
    ///
    /// This is the plugin's control plane, the job upstream gives to the
    /// Minecraft plugin-message channel: `VoiceUdpServerConnectionManager
    /// .getSecretByPlayerId` mints the player's secret and answers with a
    /// clientbound `ConnectionPacket(secret, ip, port)`. Nothing else can create a
    /// UDP connection — an unregistered secret is dropped on sight.
    PlayerConnect {
        /// The raw `player=` value; the handler validates it as a UUID so the error
        /// can name the offending value.
        player: String,
        /// The optional `name=` value, for logs and the status reply.
        name: Option<String>,
        /// The optional `ip=` value: where clients should reach this server.
        /// Upstream reads it from the config; a Pumpkin plugin has no public
        /// address of its own, so the caller supplies it.
        ip: Option<String>,
    },
    /// `player-disconnect\nplayer=<uuid>` — the player left; forget its secret and
    /// drop its socket.
    PlayerDisconnect {
        /// The raw `player=` value; the handler validates it as a UUID.
        player: String,
    },
    /// Anything else, echoed back as unsupported.
    Unknown(String),
}

impl VoiceIpc {
    /// Parses an IPC payload: `plasmo:voice/v2\n<command>[\n<key>=<value>]…`.
    ///
    /// Returns `None` when the payload is not addressed to this namespace, which
    /// lets the caller stay silent rather than erroring on foreign traffic.
    /// Otherwise the command is tokenised, not validated: an argument typo
    /// becomes an actionable error in the handler instead of silence here.
    #[must_use]
    pub fn parse(message: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(message).ok()?;
        let (namespace, rest) = text.split_once('\n')?;
        if namespace != VOICE_IPC_NAMESPACE {
            return None;
        }

        let mut lines = rest.trim().lines();
        let command = lines.next()?.trim();
        // Unknown keys are ignored on purpose, so a newer caller can send extra
        // fields without breaking an older build.
        let args: Vec<(&str, &str)> = lines
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim(), value.trim()))
            .collect();
        let argument = |key: &str| {
            args.iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| *value)
        };
        let text_argument = |key: &str| {
            argument(key)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };

        Some(match command {
            "handshake" => Self::Handshake,
            "status" => Self::Status,
            "player-connect" => Self::PlayerConnect {
                player: argument("player").unwrap_or_default().to_string(),
                name: text_argument("name"),
                ip: text_argument("ip"),
            },
            "player-disconnect" => Self::PlayerDisconnect {
                player: argument("player").unwrap_or_default().to_string(),
            },
            other => Self::Unknown(other.to_string()),
        })
    }
}

/// Lower-case hex, so an IPC reply can carry opaque bytes on one text line.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut out, byte| {
        // Writing into a `String` cannot fail.
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Builds the `player-connect` reply: the control-plane answer a Minecraft-side
/// plugin forwards to the client.
///
/// Upstream hands the client `new ConnectionPacket(secret, ip, port)`
/// (`VoiceUdpServerConnectionManager.connect`), whose secret is the one minted for
/// that player. The packet is returned already encoded so the caller only has to
/// forward bytes; `ip` must come from the caller, because a Pumpkin plugin has no
/// public address of its own. With no `ip` there is no honest endpoint to
/// advertise, so the packet is reported as `none` rather than pointing every client
/// at `0.0.0.0`.
///
/// # Errors
///
/// Fails only when the core codec cannot encode the packet, which would be a bug.
pub fn player_connect_reply(
    player_id: Uuid,
    secret: Uuid,
    port: u16,
    ip: Option<&str>,
) -> Result<String, String> {
    let packet = match ip {
        Some(ip) => hex_encode(
            &TcpCodec::new()
                .encode(&TcpPacket::Connection(ConnectionPacket {
                    secret,
                    ip: ip.to_string(),
                    port: i32::from(port),
                }))
                .map_err(|error| format!("could not encode ConnectionPacket: {error}"))?,
        ),
        None => "none".to_string(),
    };

    Ok(format!(
        "{VOICE_IPC_NAMESPACE}\nplayer-connect\nplayer={player_id}\nsecret={secret}\nport={port}\npacket={packet}"
    ))
}

/// The Pumpkin ABI surface: everything that needs `pumpkin-plugin-api`.
///
/// Compiled only for WASI, because that dependency is (see the module docs).
#[cfg(target_os = "wasi")]
mod glue {
    use pumpkin_plugin_api::wit::{IpcMessage, PluginId};
    use pumpkin_plugin_api::{Context, Plugin, PluginMetadata, permissions, register_plugin};
    use uuid::Uuid;

    use crate::runtime::{VoiceRuntime, bind, with_runtime};
    use crate::server::VoiceServer;
    use crate::state::VoiceServerState;
    use crate::{
        PLUGIN_ID, PlasmoVoicePlugin, VOICE_IPC_NAMESPACE, VoiceIpc, channel, player_connect_reply,
        tick,
    };

    impl Plugin for PlasmoVoicePlugin {
        fn new() -> Self {
            Self
        }

        fn metadata(&self) -> PluginMetadata {
            PluginMetadata {
                name: PLUGIN_ID.to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                authors: vec!["PlasmoPumpkin".to_string()],
                description:
                    "Plasmo Voice-compatible voice server: runs the UDP voice data plane in-guest."
                        .to_string(),
                // No hard dependencies: the plugin is self-contained, and the IPC
                // namespace is resolved at runtime rather than declared up front.
                dependencies: Vec::new(),
                // The host enforces these at load time; they are the minimum needed
                // to bind a UDP socket, talk to peers, and persist `state.json`.
                permissions: vec![
                    // Bind and read the voice socket.
                    permissions::NETWORK_UDP.to_string(),
                    permissions::NETWORK_UDP_CONNECT.to_string(),
                    permissions::NETWORK_UDP_OUTGOING_DATAGRAM.to_string(),
                    // Persist state.json in plugins/data/<name>.
                    permissions::FS_READ_DATA.to_string(),
                    permissions::FS_WRITE_DATA.to_string(),
                ],
            }
        }

        fn on_load(&self, context: Context) -> Result<(), String> {
            let folder = context.get_data_folder();
            tracing::info!(%folder, "loading the Plasmo Voice server");

            let mut persisted = VoiceServerState::load(&folder);

            // Port 0 means "not configured yet": ask the host for an ephemeral port
            // and remember what we actually got, so a reload keeps listening on the
            // same one.
            let (socket, port) = bind(persisted.port)?;
            let secret = if persisted.secret.is_empty() {
                VoiceServerState::generate_secret(port)
            } else {
                persisted.secret.clone()
            };

            persisted.port = port;
            persisted.secret.clone_from(&secret);
            persisted.protocol_version = plasmo_voice_core::PROTOCOL_VERSION.to_string();
            persisted.save(&folder)?;

            let parsed_secret = Uuid::parse_str(&secret)
                .map_err(|error| format!("generated secret {secret:?} is not a UUID: {error}"))?;

            with_runtime(|runtime| {
                runtime.start(socket, VoiceServer::new(parsed_secret), folder.clone());
            });

            // The tick pump is the data plane's clock: without it the socket is never
            // drained. Registering it here (and not in `new`) guarantees the runtime
            // is already installed, so the very first tick finds something to pump.
            let handler_id = tick::register(&context)?;

            // The control plane travels over the `plasmo:voice` plugin-message channel:
            // the handshake, the registration burst and every reply to a client packet
            // are written from the tick pump, and read from the payload handler.
            let channel_handlers = channel::register(&context)?;

            // The values are inlined into the message on purpose: Pumpkin's non-TTY
            // "simple logger" prints the message text only and drops every structured
            // field, so a field-only log line is an empty log line on a headless server —
            // exactly the server an operator has to debug from a log file.
            tracing::info!(
                port,
                handler_id,
                channel_handlers = ?channel_handlers,
                enabled = persisted.enabled,
                protocol = plasmo_voice_core::PROTOCOL_VERSION,
                "the Plasmo Voice server is listening on UDP port {port} (tick handler \
                 {handler_id}, channel handlers {channel_handlers:?})"
            );
            Ok(())
        }

        fn on_unload(&self, _context: Context) -> Result<(), String> {
            // Flush the current endpoint before the socket goes away. Everything is
            // read out first and the runtime torn down in the same lock scope, so a
            // tick landing mid-unload can only observe the already-stopped state.
            let (folder, listening, connections, received, sent) = with_runtime(|runtime| {
                let folder = runtime.data_folder().to_string();
                let endpoint = runtime
                    .port()
                    .map(|port| (port, runtime.server_secret().unwrap_or_else(Uuid::nil)));
                let connections = runtime.connection_count();
                let received = runtime.received();
                let sent = runtime.sent();
                runtime.stop();
                (folder, endpoint, connections, received, sent)
            });

            if let Some((port, secret)) = listening {
                let state = VoiceServerState {
                    port,
                    secret: secret.to_string(),
                    protocol_version: plasmo_voice_core::PROTOCOL_VERSION.to_string(),
                    enabled: true,
                };
                state.save(&folder)?;
                tracing::info!(
                    port,
                    connections,
                    received,
                    sent,
                    "the Plasmo Voice server stopped"
                );
            }
            Ok(())
        }

        /// Answers `plasmo:voice/v2` control messages from other plugins.
        ///
        /// The namespace lives in the payload, not in the plugin id, so a message
        /// from an unrelated plugin is rejected with an explanation rather than
        /// ignored: the sender is waiting on a reply and deserves to know why it
        /// will not get one.
        fn handle_ipc_message(
            &self,
            _sender: PluginId,
            message: IpcMessage,
        ) -> Result<IpcMessage, String> {
            let Some(request) = VoiceIpc::parse(&message) else {
                return Err(format!(
                    "plasmo-voice only speaks {VOICE_IPC_NAMESPACE} messages"
                ));
            };

            let reply = with_runtime(|runtime| {
                let runtime: &mut VoiceRuntime = runtime;
                match request {
                    // A handshake is answered even when the socket is down: the caller
                    // learns the configured endpoint and can decide to retry later.
                    VoiceIpc::Handshake => {
                        let port = runtime
                            .port()
                            .map_or_else(String::new, |port| port.to_string());
                        let secret = runtime
                            .server_secret()
                            .map_or_else(String::new, |secret| secret.to_string());
                        Ok(format!(
                            "{VOICE_IPC_NAMESPACE}\nhandshake\nport={port}\nsecret={secret}\nlistening={}",
                            runtime.is_listening()
                        ))
                    }
                    VoiceIpc::Status => {
                        let dropped = runtime.protocol().map_or(0, VoiceServer::dropped);
                        let players = runtime
                            .protocol()
                            .map_or(0, VoiceServer::registered_player_count);
                        Ok(format!(
                            "{VOICE_IPC_NAMESPACE}\nstatus\nlistening={}\nplayers={}\nconnections={}\nreceived={}\nsent={}\ndropped={}",
                            runtime.is_listening(),
                            players,
                            runtime.connection_count(),
                            runtime.received(),
                            runtime.sent(),
                            dropped
                        ))
                    }
                    // The control plane's "a player joined": mint the secret that
                    // player's client will speak with, and hand back the exact
                    // clientbound `ConnectionPacket` upstream would send, already
                    // encoded, so the caller only has to forward bytes.
                    VoiceIpc::PlayerConnect { player, name, ip } => {
                        let player_id = Uuid::parse_str(&player)
                            .map_err(|error| format!("player={player:?} is not a UUID: {error}"))?;
                        let port = runtime
                            .port()
                            .ok_or_else(|| "the voice server is not listening yet".to_string())?;

                        let protocol = runtime
                            .protocol_mut()
                            .ok_or_else(|| "the voice server is not running".to_string())?;
                        let secret = protocol.register_player(player_id, name);

                        tracing::info!(
                            %player_id,
                            %secret,
                            port,
                            endpoint = ip.as_deref().unwrap_or("unspecified"),
                            "registered a voice player"
                        );

                        player_connect_reply(player_id, secret, port, ip.as_deref())
                    }
                    VoiceIpc::PlayerDisconnect { player } => {
                        let player_id = Uuid::parse_str(&player)
                            .map_err(|error| format!("player={player:?} is not a UUID: {error}"))?;

                        let removed = runtime
                            .protocol_mut()
                            .and_then(|protocol| protocol.unregister_player(&player_id));
                        let secret = removed.map_or_else(String::new, |secret| secret.to_string());
                        let players = runtime
                            .protocol()
                            .map_or(0, VoiceServer::registered_player_count);

                        tracing::info!(%player_id, forgotten = removed.is_some(), "voice player left");

                        Ok(format!(
                            "{VOICE_IPC_NAMESPACE}\nplayer-disconnect\nplayer={player_id}\nsecret={secret}\nplayers={players}"
                        ))
                    }
                    VoiceIpc::Unknown(command) => Err(format!(
                        "unsupported {VOICE_IPC_NAMESPACE} command {command:?}"
                    )),
                }
            })?;
            Ok(reply.into_bytes())
        }
    }

    register_plugin!(PlasmoVoicePlugin);
}

#[cfg(test)]
mod tests {
    use super::*;
    use plasmo_voice_core::PacketDirection;

    #[test]
    fn parses_the_handshake_and_status_commands() {
        assert_eq!(
            VoiceIpc::parse(b"plasmo:voice/v2\nhandshake"),
            Some(VoiceIpc::Handshake)
        );
        assert_eq!(
            VoiceIpc::parse(b"plasmo:voice/v2\nstatus"),
            Some(VoiceIpc::Status)
        );
    }

    #[test]
    fn parses_a_player_connect_with_its_arguments() {
        assert_eq!(
            VoiceIpc::parse(
                b"plasmo:voice/v2\nplayer-connect\nplayer=1f8f0d3a-1c2b-4d5e-8f90-1234567890ab\nname=Alice\nip=203.0.113.7"
            ),
            Some(VoiceIpc::PlayerConnect {
                player: "1f8f0d3a-1c2b-4d5e-8f90-1234567890ab".to_string(),
                name: Some("Alice".to_string()),
                ip: Some("203.0.113.7".to_string()),
            })
        );
    }

    #[test]
    fn player_arguments_are_optional_and_unknown_keys_are_ignored() {
        // The handler validates the UUID, so parsing must not swallow the value.
        assert_eq!(
            VoiceIpc::parse(b"plasmo:voice/v2\nplayer-connect\nplayer=\ncolour=red"),
            Some(VoiceIpc::PlayerConnect {
                player: String::new(),
                name: None,
                ip: None,
            })
        );
        assert_eq!(
            VoiceIpc::parse(b"plasmo:voice/v2\nplayer-disconnect\nplayer=abc\nfuture=1"),
            Some(VoiceIpc::PlayerDisconnect {
                player: "abc".to_string()
            })
        );
    }

    #[test]
    fn tolerates_surrounding_whitespace_and_a_trailing_newline() {
        assert_eq!(
            VoiceIpc::parse(b"plasmo:voice/v2\n  handshake  \n"),
            Some(VoiceIpc::Handshake)
        );
    }

    #[test]
    fn an_unknown_command_is_reported_rather_than_ignored() {
        assert_eq!(
            VoiceIpc::parse(b"plasmo:voice/v2\nfrobnicate"),
            Some(VoiceIpc::Unknown("frobnicate".to_string()))
        );
    }

    #[test]
    fn a_foreign_namespace_is_not_ours() {
        // Another plugin's traffic must not be answered at all.
        assert_eq!(VoiceIpc::parse(b"other:plugin/v1\nhandshake"), None);
    }

    #[test]
    fn payloads_without_a_namespace_line_are_ignored() {
        assert_eq!(VoiceIpc::parse(b"handshake"), None);
        assert_eq!(VoiceIpc::parse(b""), None);
    }

    #[test]
    fn invalid_utf8_is_ignored_instead_of_panicking() {
        assert_eq!(VoiceIpc::parse(&[0xff, 0xfe, 0x00]), None);
    }

    /// Decodes the `packet=` line of a reply back into a `TcpPacket`.
    fn decode_reply_packet(reply: &str) -> TcpPacket {
        let hex = reply
            .lines()
            .find_map(|line| line.strip_prefix("packet="))
            .expect("a packet= line");
        assert_ne!(hex, "none", "the reply carries no encoded packet");

        let bytes: Vec<u8> = (0..hex.len() / 2)
            .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).expect("hex"))
            .collect();
        TcpCodec::new()
            .decode(&bytes, PacketDirection::Client)
            .expect("decode ok")
            .expect("a packet")
    }

    #[test]
    fn player_connect_reply_carries_an_encoded_connection_packet() {
        let player_id = Uuid::from_u128(0xa11ce);
        let secret = Uuid::from_u128(0x005e_c5e7);
        let reply =
            player_connect_reply(player_id, secret, 8830, Some("203.0.113.7")).expect("reply");

        assert!(reply.starts_with("plasmo:voice/v2\nplayer-connect\n"));
        assert!(reply.contains(&format!("player={player_id}")));
        assert!(reply.contains(&format!("secret={secret}")));
        assert!(reply.contains("port=8830"));

        // The bytes are the clientbound `ConnectionPacket` upstream sends, so a
        // caller can forward them without knowing the wire format.
        match decode_reply_packet(&reply) {
            TcpPacket::Connection(packet) => {
                assert_eq!(packet.secret, secret);
                assert_eq!(packet.ip, "203.0.113.7");
                assert_eq!(packet.port, 8830);
            }
            other => panic!("expected a connection packet, got {other:?}"),
        }
    }

    #[test]
    fn player_connect_reply_without_an_ip_has_no_packet() {
        // No public address means no honest endpoint: the caller gets the secret
        // and the port, and is told to build the packet itself.
        let reply =
            player_connect_reply(Uuid::from_u128(1), Uuid::from_u128(2), 1, None).expect("reply");
        assert!(reply.ends_with("packet=none"), "{reply}");
    }

    #[test]
    fn hex_encoding_is_lower_case_and_byte_wise() {
        assert_eq!(hex_encode(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex_encode(&[]), "");
    }
}
