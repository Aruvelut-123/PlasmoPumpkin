//! The `plasmo:voice` plugin-message channel — the control plane's real transport.
//!
//! Upstream Plasmo Voice has **no TCP listener**: its control plane is the Minecraft
//! plugin-message channel. `ServerChannelHandler` receives `plasmo:voice` payloads and
//! feeds them to `PacketTcpCodec` in the SERVER direction, while
//! `VoiceTcpServerConnectionManager` owns the per-player secrets and replies with
//! clientbound TCP packets on the same channel.
//!
//! Pumpkin 0.2.0 hands a plugin the same two primitives — `PlayerCustomPayloadEvent` and
//! `java-player::send-custom-payload` — so the control plane runs **in-guest**, with no
//! companion plugin and no IPC hop. This module is the ABI shim: it converts host events
//! into [`crate::control`] calls and delivers the resulting [`Outbound`] messages. All the
//! protocol decisions live in `control.rs`, which is host-testable.
//!
//! ## The session, in host terms
//!
//! | Moment | Pumpkin event | What we do |
//! | --- | --- | --- |
//! | the client announces the channel | `PlayerRegisterChannelEvent` | phase 1: queue a `PlayerInfoRequestPacket` |
//! | the client answers | `PlayerCustomPayloadEvent` | the whole `control.rs` state machine |
//! | the client moves | `PlayerMoveEvent` | record its position and world, which is what proximity needs |
//! | the player leaves | `PlayerLeaveEvent` | forget the registration, broadcast `PlayerDisconnectPacket` |

use pumpkin_plugin_api::Server;
use pumpkin_plugin_api::events::{
    EventData, EventHandler, EventPriority, PlayerCustomPayloadEvent, PlayerLeaveEvent,
    PlayerMoveEvent, PlayerRegisterChannelEvent,
};
use pumpkin_plugin_api::uuid::Uuid as WitUuid;
use uuid::Uuid;

use crate::control::Outbound;
use crate::runtime::with_runtime;

/// The channel Plasmo Voice registers with the server.
///
/// Upstream spells it `plasmo:voice`; the `v2` in [`crate::VOICE_IPC_NAMESPACE`] is a
/// *protocol* namespace, not a channel name — do not conflate the two.
pub const CHANNEL: &str = "plasmo:voice";

/// Converts a host UUID into a `uuid` crate UUID.
///
/// The WIT type is a `{ high: u64, low: u64 }` record, i.e. the two halves of the 128-bit
/// value in big-endian order — the same layout `uuid::Uuid::as_u64_pair` splits on.
#[must_use]
pub fn player_uuid(id: &WitUuid) -> Uuid {
    Uuid::from_u64_pair(id.high, id.low)
}

/// Converts back, for `server.get_player_by_uuid`.
#[must_use]
pub fn wit_uuid(id: Uuid) -> WitUuid {
    let (high, low) = id.as_u64_pair();
    WitUuid { high, low }
}

/// Delivers one control-plane message over the channel.
///
/// A broadcast goes to every player with an **active voice connection** — upstream's
/// `broadcast` only ever reaches players whose UDP connection exists, and reaching the
/// rest would tell a client about players it has not been introduced to yet.
fn deliver(server: &Server, message: Outbound) {
    match message.to {
        Some(player_id) => {
            if let Some(player) = server.get_player_by_uuid(wit_uuid(player_id)) {
                send(&player, &message.payload);
            } else {
                tracing::debug!(
                    player = %player_id,
                    "dropping a control message for a player who is gone"
                );
            }
        }
        None => {
            let recipients = with_runtime(|runtime| {
                runtime
                    .protocol()
                    .map_or_else(Vec::new, |protocol| protocol.connected_player_ids())
            });
            for player_id in recipients {
                if let Some(player) = server.get_player_by_uuid(wit_uuid(player_id)) {
                    send(&player, &message.payload);
                }
            }
        }
    }
}

/// Writes one payload to one player.
fn send(player: &pumpkin_plugin_api::Player, payload: &[u8]) {
    let Some(java) = player.as_java() else {
        // Bedrock clients cannot run the Plasmo Voice mod, so there is nothing to say
        // to them.
        tracing::trace!(
            name = %player.get_name(),
            "skipping a non-Java player on the voice channel"
        );
        return;
    };
    java.send_custom_payload(CHANNEL, payload);
}

/// Delivers every message the runtime has produced since the last call.
///
/// Called from the tick pump, right after the socket has been drained.
pub fn flush(server: &Server) {
    for message in with_runtime(|runtime| runtime.take_control()) {
        deliver(server, message);
    }
}

/// Handles every client → server `plasmo:voice` payload (upstream `ServerChannelHandler`).
pub struct PayloadHandler;

impl EventHandler<PlayerCustomPayloadEvent> for PayloadHandler {
    fn handle(
        &self,
        server: Server,
        event: EventData<PlayerCustomPayloadEvent>,
    ) -> EventData<PlayerCustomPayloadEvent> {
        if event.channel != CHANNEL {
            return event;
        }
        let player = &event.player;
        let player_id = player_uuid(&player.get_id());
        let name = player.get_name();

        let (messages, identified) = with_runtime(|runtime| {
            let Some(port) = runtime.port() else {
                return (Vec::new(), false);
            };
            let ip = runtime.advertised_ip().to_string();
            let Some(protocol) = runtime.protocol_mut() else {
                return (Vec::new(), false);
            };
            let messages = protocol.handle_control(player_id, &name, &ip, port, &event.data);
            let identified = protocol.registered_player(&player_id).is_some();
            (messages, identified)
        });

        // Any answer at all stops the `PlayerInfoRequestPacket` retries, exactly like
        // upstream's scheduler, which is cleared by any inbound TCP packet.
        if identified {
            with_runtime(|runtime| runtime.player_identified(&player_id));
        }
        for message in messages {
            deliver(&server, message);
        }
        event
    }
}

/// Kicks off the handshake as soon as a client announces the channel.
///
/// Upstream starts the same handshake on `McPlayerJoinEvent`, but the channel is the more
/// honest trigger in a guest: until the client has registered it, a payload sent to that
/// player has nowhere to land.
pub struct ChannelRegisterHandler;

impl EventHandler<PlayerRegisterChannelEvent> for ChannelRegisterHandler {
    fn handle(
        &self,
        server: Server,
        event: EventData<PlayerRegisterChannelEvent>,
    ) -> EventData<PlayerRegisterChannelEvent> {
        if event.channel == CHANNEL {
            let player_id = player_uuid(&event.player.get_id());
            tracing::debug!(
                name = %event.player.get_name(),
                ip = %event.player.get_ip(),
                "the client registered the voice channel"
            );
            with_runtime(|runtime| runtime.request_player_info(player_id, crate::server::now_ms()));
            // The request itself is queued; the pump sends it on the next tick, so a
            // client that registers the channel mid-tick is answered in order.
            flush(&server);
        }
        event
    }
}

/// Forgets a player, and tells the others (upstream's disconnect broadcast).
pub struct LeaveHandler;

impl EventHandler<PlayerLeaveEvent> for LeaveHandler {
    fn handle(
        &self,
        server: Server,
        event: EventData<PlayerLeaveEvent>,
    ) -> EventData<PlayerLeaveEvent> {
        let player_id = player_uuid(&event.player.get_id());
        tracing::debug!(name = %event.player.get_name(), "a voice player left");
        let messages = with_runtime(|runtime| {
            runtime
                .protocol_mut()
                .map_or_else(Vec::new, |protocol| protocol.control_disconnect(player_id))
        });
        for message in messages {
            deliver(&server, message);
        }
        event
    }
}

/// Tracks positions, which is what makes distance filtering possible.
///
/// Upstream reads positions live from the Minecraft server when it computes a listener
/// set. A guest cannot: the world is behind the host boundary, so positions have to be
/// pushed in, and this event is the push.
pub struct MoveHandler;

impl EventHandler<PlayerMoveEvent> for MoveHandler {
    fn handle(
        &self,
        _server: Server,
        event: EventData<PlayerMoveEvent>,
    ) -> EventData<PlayerMoveEvent> {
        let player = &event.player;
        let player_id = player_uuid(&player.get_id());
        let position = player.get_position();
        let world = player.get_world().get_id();
        with_runtime(|runtime| {
            if let Some(protocol) = runtime.protocol_mut() {
                protocol.set_position(&player_id, Some(world), position);
            }
        });
        event
    }
}

/// Registers the channel handlers with the server.
///
/// All four are `EventPriority::Normal` and non-blocking: none of them wants to modify the
/// event it observes, and a payload must never jump ahead of the game logic that may be
/// tearing a connection down.
pub fn register(context: &pumpkin_plugin_api::Context) -> Result<Vec<u32>, String> {
    let payload = context
        .register_event_handler(PayloadHandler, EventPriority::Normal, false)
        .map_err(|error| format!("could not register the voice payload handler: {error}"))?;
    let channel = context
        .register_event_handler(ChannelRegisterHandler, EventPriority::Normal, false)
        .map_err(|error| format!("could not register the voice channel handler: {error}"))?;
    let leave = context
        .register_event_handler(LeaveHandler, EventPriority::Normal, false)
        .map_err(|error| format!("could not register the voice leave handler: {error}"))?;
    let movement = context
        .register_event_handler(MoveHandler, EventPriority::Normal, false)
        .map_err(|error| format!("could not register the voice move handler: {error}"))?;
    Ok(vec![payload, channel, leave, movement])
}
