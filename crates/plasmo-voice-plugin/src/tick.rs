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

use crate::channel;
use crate::runtime::with_runtime;

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
