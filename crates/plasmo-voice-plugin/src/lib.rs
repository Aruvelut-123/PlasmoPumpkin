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
//! | [`server`] | any | The UDP protocol semantics — packet decoding, per-connection secrets, audio fan-out. Socket-free, so it is fully unit-tested on the host. |
//! | [`runtime`] | any | The process-global socket + protocol state, the socket bind, and the bounded per-tick pump. |
//! | `tick` | WASI | The Pumpkin event bridge: a `ServerTickStartEvent` handler that drives the pump. |
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

pub mod runtime;
pub mod server;
pub mod state;

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
    /// Anything else, echoed back as unsupported.
    Unknown(String),
}

impl VoiceIpc {
    /// Parses an IPC payload: `plasmo:voice/v2\n<command>`.
    ///
    /// Returns `None` when the payload is not addressed to this namespace, which
    /// lets the caller stay silent rather than erroring on foreign traffic.
    #[must_use]
    pub fn parse(message: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(message).ok()?;
        let (namespace, rest) = text.split_once('\n')?;
        if namespace != VOICE_IPC_NAMESPACE {
            return None;
        }
        Some(match rest.trim() {
            "handshake" => Self::Handshake,
            "status" => Self::Status,
            other => Self::Unknown(other.to_string()),
        })
    }
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
    use crate::{PLUGIN_ID, PlasmoVoicePlugin, VOICE_IPC_NAMESPACE, VoiceIpc, tick};

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

            tracing::info!(
                port,
                handler_id,
                enabled = persisted.enabled,
                protocol = plasmo_voice_core::PROTOCOL_VERSION,
                "the Plasmo Voice server is listening"
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
                        Ok(format!(
                            "{VOICE_IPC_NAMESPACE}\nstatus\nlistening={}\nconnections={}\nreceived={}\nsent={}\ndropped={}",
                            runtime.is_listening(),
                            runtime.connection_count(),
                            runtime.received(),
                            runtime.sent(),
                            dropped
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
}
