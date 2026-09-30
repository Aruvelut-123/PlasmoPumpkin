//! The `plasmo:voice/v2` plugin-message channel — the control plane's real transport.
//!
//! Upstream Plasmo Voice has **no TCP listener**: its control plane is the Minecraft
//! plugin-message channel. `ServerChannelHandler` receives `plasmo:voice/v2` payloads and
//! feeds them to `PacketTcpCodec` in the SERVER direction, while
//! `VoiceTcpServerConnectionManager` owns the per-player secrets and replies with
//! clientbound TCP packets on the same channel.
//!
//! The channel name is load-bearing and easy to get wrong: `BaseVoiceServer.CHANNEL_STRING`
//! is `plasmo:voice/v2`, and the client registers its receiver on exactly that string, so a
//! payload sent to plain `plasmo:voice` vanishes silently. The name and its pinned
//! assertion live in [`crate::control`], which the host build can test.
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
//! | the player joins | `PlayerJoinEvent` | phase 1: queue a `PlayerInfoRequestPacket` |
//! | the client announces the channel | `PlayerRegisterChannelEvent` | phase 1 again, as a second trigger |
//! | the client answers | `PlayerCustomPayloadEvent` | the whole `control.rs` state machine |
//! | the client moves | `PlayerMoveEvent` | record its position and world, which is what proximity needs |
//! | the player leaves | `PlayerLeaveEvent` | forget the registration, broadcast `PlayerDisconnectPacket` |
//!
//! ## Why the handshake starts on join, and not on channel registration
//!
//! Upstream asks the client who it is on join (`PlayerChannelHandler:103` →
//! `VoiceTcpServerConnectionManager.requestPlayerInfo` →
//! `new PlayerInfoRequestPacket()`), and the client only ever *answers* that request —
//! `ModServerConnection.handle(PlayerInfoRequestPacket)` is the sole path that sends a
//! `PlayerInfoPacket`. Nothing in the client registers the channel with the server: the
//! mod listens on `plasmo:voice/v2` unconditionally, because a custom payload needs no
//! registration to be delivered.
//!
//! Waiting for a registration event therefore waits forever, and the symptom is a client
//! that reports "Plasmo Voice is not installed on this server" against a voice server that
//! is running perfectly. The registration event is kept as a *second* trigger (it costs
//! nothing and a client that does announce itself gets asked sooner), but join is the one
//! that matters.

use pumpkin_plugin_api::Server;
use pumpkin_plugin_api::events::{
    EventData, EventHandler, EventPriority, PlayerCustomPayloadEvent, PlayerJoinEvent,
    PlayerLeaveEvent, PlayerMoveEvent, PlayerRegisterChannelEvent,
};
use pumpkin_plugin_api::uuid::Uuid as WitUuid;
use uuid::Uuid;

use crate::control::{Outbound, is_voice_channel};
use crate::runtime::with_runtime;

/// The channel every payload is written to, and the one the client listens on.
///
/// Defined in [`crate::control`] so the host build can pin it with a test.
pub use crate::control::CHANNEL;

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

/// Handles every client → server `plasmo:voice/v2` payload (upstream `ServerChannelHandler`).
pub struct PayloadHandler;

impl EventHandler<PlayerCustomPayloadEvent> for PayloadHandler {
    fn handle(
        &self,
        server: Server,
        event: EventData<PlayerCustomPayloadEvent>,
    ) -> EventData<PlayerCustomPayloadEvent> {
        if !is_voice_channel(&event.channel) {
            // Worth a line: a payload on an unexpected channel is either another plugin's
            // traffic (ignore it) or a client that spells the voice channel differently
            // from us — and the second case is invisible from the outside, because a
            // client talking on a channel nobody reads simply looks like a broken client.
            tracing::debug!(
                channel = %event.channel,
                bytes = event.data.len(),
                "ignoring a custom payload that is not on a voice channel"
            );
            return event;
        }
        let player = &event.player;
        let player_id = player_uuid(&player.get_id());
        let name = player.get_name();
        tracing::debug!(
            channel = %event.channel,
            bytes = event.data.len(),
            name = %name,
            "a voice payload arrived"
        );

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
/// The join handler is the trigger that matters (upstream asks on join); this is a cheap
/// second one, in case a host or a client does announce `plasmo:voice/v2` explicitly.
pub struct ChannelRegisterHandler;

impl EventHandler<PlayerRegisterChannelEvent> for ChannelRegisterHandler {
    fn handle(
        &self,
        server: Server,
        event: EventData<PlayerRegisterChannelEvent>,
    ) -> EventData<PlayerRegisterChannelEvent> {
        if is_voice_channel(&event.channel) {
            let player_id = player_uuid(&event.player.get_id());
            // Info, not debug: this single line is the handshake's trigger. Without it a
            // voice client never learns there is a voice server, and no amount of UDP
            // debugging can find the cause.
            tracing::info!(
                name = %event.player.get_name(),
                ip = %event.player.get_ip(),
                "the client {} registered the voice channel (from {})",
                event.player.get_name(),
                event.player.get_ip()
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

/// Starts the handshake when a player joins — the trigger that actually matters.
///
/// Upstream asks on join (`PlayerChannelHandler:103`), because the client is a passive
/// responder: it never announces the channel, so waiting for a registration event means
/// never asking at all. See the module docs.
pub struct JoinHandler;

impl EventHandler<PlayerJoinEvent> for JoinHandler {
    fn handle(
        &self,
        server: Server,
        event: EventData<PlayerJoinEvent>,
    ) -> EventData<PlayerJoinEvent> {
        let player_id = player_uuid(&event.player.get_id());
        tracing::info!(
            name = %event.player.get_name(),
            "starting the voice handshake with {}",
            event.player.get_name()
        );
        with_runtime(|runtime| runtime.request_player_info(player_id, crate::server::now_ms()));
        // Queued by the request; the tick pump writes it out.
        flush(&server);
        event
    }
}

/// Registers the channel handlers with the server.
///
/// All are `EventPriority::Normal` and non-blocking: none of them wants to modify the
/// event it observes, and a payload must never jump ahead of the game logic that may be
/// tearing a connection down.
pub fn register(context: &pumpkin_plugin_api::Context) -> Result<Vec<u32>, String> {
    let join = context
        .register_event_handler(JoinHandler, EventPriority::Normal, false)
        .map_err(|error| format!("could not register the voice join handler: {error}"))?;
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
    Ok(vec![join, payload, channel, leave, movement])
}
