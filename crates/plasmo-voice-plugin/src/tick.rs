//! The Pumpkin event bridge: how the voice data plane gets its turn to run.
//!
//! ## Why this module exists
//!
//! `wasm32-wasip2` has **no threads**, so the UDP voice socket cannot own a
//! thread of its own. Something on the host has to call back into the guest on a
//! regular schedule, and in Pumpkin 0.2.0 the only recurring, non-cancellable
//! hook a plugin can register is [`ServerTickStartEvent`] — fired at the start of
//! every server tick (~20 Hz by default).
//!
//! `pumpkin-plugin-api` 0.2.0 does **not** expose an `on_tick` method on the
//! `Plugin` trait. The event handler is therefore the supported mechanism, and
//! this module is the only place that knows about it: swapping the pump's clock
//! source later means changing this file and nothing else.
//!
//! ## Why a global, and not a captured `Arc<Plugin>`
//!
//! `Plugin::on_load` receives `&self`, not an owned or shared handle, so it has
//! no `Arc<PlasmoVoicePlugin>` to hand to a handler. The guest is a single-threaded,
//! single-instance component — `register_plugin!` stores exactly one plugin and
//! the host may only call `init-plugin` once — so the data plane lives in a
//! process-global [`crate::runtime::GLOBAL`] that both the plugin methods and the
//! tick pump reach through [`crate::runtime::with_runtime`]. That keeps the socket
//! and the protocol state owned in one place instead of duplicated behind two
//! handles.
//!
//! ## Re-entrancy
//!
//! A tick can arrive while `on_unload` is running (or vice versa). All shared
//! state sits behind one [`Mutex`], so the two can never alias the socket;
//! whoever loses the race simply observes the post-unload state and does nothing.

use pumpkin_plugin_api::Server;
use pumpkin_plugin_api::events::{EventData, EventHandler, EventPriority, ServerTickStartEvent};
use pumpkin_plugin_api::text::TextComponent;

use crate::channel::{self, wit_uuid};
use crate::commands::broadcast_player_info;
use crate::mute;
use crate::runtime::with_runtime;

/// How often the vanish table is re-read from the host, in server ticks.
///
/// 20 ticks ≈ 1 s at the default 20 Hz, which is fine for a visibility mirror:
/// the host's `hidePlayer`/`showPlayer` calls take effect on the next sync.
const VISIBILITY_SYNC_TICKS: i32 = 20;

/// How often temporary mutes are re-checked, in server ticks.
///
/// 100 ticks ≈ 5 s at the default 20 Hz, mirroring upstream `VoiceMuteManager`'s
/// `scheduleAtFixedRate(..., 5, TimeUnit.SECONDS)` sweep. The store is also
/// consulted per packet (`mute::is_muted`), so this sweep only announces the
/// lift and tidies the table — nothing is enforced here.
const MUTE_SWEEP_TICKS: i32 = 100;

/// Cheap, zero-sized handle handed to the host as an event handler.
///
/// It carries no state: the data plane it pumps is the process-global runtime.
pub struct TickPump;

impl EventHandler<ServerTickStartEvent> for TickPump {
    /// Drains a bounded batch of voice datagrams and answers them, then delivers whatever
    /// the control plane produced over the `plasmo:voice` channel.
    ///
    /// The event data is returned unchanged: this pump observes the tick, it does
    /// not modify it. The tick number is only reported in traces.
    fn handle(
        &self,
        server: Server,
        event: EventData<ServerTickStartEvent>,
    ) -> EventData<ServerTickStartEvent> {
        // Re-mirror the host's vanish table a few times a second. Doing it here,
        // before the pump, means this tick's relay already uses fresh visibility.
        if event.tick % VISIBILITY_SYNC_TICKS == 0 {
            sync_visibility(&server);
        }
        // Lift expired temporary mutes on the same schedule as upstream
        // (`VoiceMuteManager.tick()` every 5 s). The pump then flushes the
        // PlayerInfoUpdate broadcasts below, like a `/vunmute` would.
        if event.tick % MUTE_SWEEP_TICKS == 0 {
            expire_mutes(&server);
        }
        let (received, sent) = with_runtime(|runtime| runtime.pump());
        // The socket and the channel cannot be driven from the same borrow, so the pump
        // queues control messages and they are written out here — inside the same tick,
        // so a client that connects is told about it before the next one.
        channel::flush(&server);
        if received > 0 {
            tracing::trace!(tick = event.tick, received, sent, "voice tick");
        }
        event
    }
}

/// Re-reads the host's per-player `canSee` and pushes the result into the server.
///
/// The host owns the vanish truth (its `hidePlayer`/`showPlayer` state); the
/// guest can only observe it through `canSee`. Rebuilding the whole table every
/// sync keeps it free of stale entries for players who left between syncs, at
/// the cost of an O(n²) `canSee` sweep over registered players — cheap at the
/// player counts a 20 Hz schedule implies.
fn sync_visibility(server: &Server) {
    use std::collections::HashSet;

    // Snapshot the ids inside the lock (cheap), then query the host outside it
    // (each `can_see` is a host round-trip), then write the table back.
    let ids = with_runtime(|runtime| {
        runtime
            .protocol()
            .map_or_else(Vec::new, |protocol| protocol.registered_player_ids())
    });
    if ids.len() < 2 {
        return;
    }

    let mut hidden = HashSet::new();
    for (i, &viewer) in ids.iter().enumerate() {
        for &target in &ids[i + 1..] {
            // The generated resource methods take `other` by value (the handle
            // is consumed), so fetch a fresh handle per direction.
            let viewer_sees_target =
                server
                    .get_player_by_uuid(wit_uuid(viewer))
                    .is_some_and(|viewer_player| {
                        server
                            .get_player_by_uuid(wit_uuid(target))
                            .is_some_and(|target_player| viewer_player.can_see(target_player))
                    });
            if !viewer_sees_target {
                hidden.insert((viewer, target));
            }

            let target_sees_viewer =
                server
                    .get_player_by_uuid(wit_uuid(target))
                    .is_some_and(|target_player| {
                        server
                            .get_player_by_uuid(wit_uuid(viewer))
                            .is_some_and(|viewer_player| target_player.can_see(viewer_player))
                    });
            if !target_sees_viewer {
                hidden.insert((target, viewer));
            }
        }
    }

    with_runtime(|runtime| {
        if let Some(protocol) = runtime.protocol_mut() {
            protocol.set_hidden_players(hidden);
        }
    });
}

/// Lifts every expired temporary mute, mirroring upstream `VoiceMuteManager.tick()`:
///
/// 1. the store record is removed (`expire_due`), exactly like an `unmute`;
/// 2. the voice clients hear about the change through a `PlayerInfoUpdate`;
/// 3. the player is told `"You've been unmuted"` — unless `notify.unmuted` is off,
///    upstream's `silent = ... || !notifications().unmuted()` gate.
///
/// Mute *enforcement* never relied on this sweep (packets are checked against
/// `mute::is_muted` per datagram), so a delayed sweep only delays the notice.
fn expire_mutes(server: &Server) {
    let freed = mute::expire_due(crate::server::now_ms());
    if freed.is_empty() {
        return;
    }
    for player_id in &freed {
        broadcast_player_info(player_id);
    }
    let notify = with_runtime(|runtime| {
        runtime
            .protocol()
            .is_some_and(|protocol| protocol.config().notify_unmuted())
    });
    if !notify {
        return;
    }
    let notice = mute::unmuted_notice();
    for player_id in freed {
        if let Some(player) = server.get_player_by_uuid(wit_uuid(player_id)) {
            player.send_system_message(TextComponent::text(&notice), false);
        }
    }
}

/// Registers the tick pump with the server.
///
/// `EventPriority::Normal` and `blocking = false` are both deliberate: the pump
/// must not jump ahead of game logic that may be tearing connections down, and it
/// has no interest in *modifying* the tick event, so it must not opt into a
/// blocking (slow-path) dispatch where the host waits for a new event value.
pub fn register(context: &pumpkin_plugin_api::Context) -> Result<u32, String> {
    context
        .register_event_handler(TickPump, EventPriority::Normal, false)
        .map_err(|error| format!("could not register the voice tick pump: {error}"))
}
