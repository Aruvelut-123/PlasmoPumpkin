//! The process-global voice data plane.
//!
//! ## Why global state is the right call here
//!
//! On the host this crate is an ordinary library, but the artifact that actually
//! runs is a `wasm32-wasip2` component. In that world:
//!
//! * there is exactly **one** instance per component — `register_plugin!` stores
//!   the plugin in a `OnceLock` and the host may only call `init-plugin` once;
//! * there are **no threads**, so there is no parallelism to reason about, only
//!   re-entrancy (a tick landing inside `on_unload`);
//! * the socket and the protocol state must outlive `on_load`'s borrow, because
//!   the tick pump is invoked by the host on later turns.
//!
//! A single [`Mutex`]-guarded global satisfies all three, and gives the plugin
//! methods and the tick pump (`crate::tick`) one unambiguous place to meet. The
//! alternative — trying to smuggle an `Arc` out of `on_load` — cannot work through
//! the `Plugin` trait, which only ever hands out `&self`.
//!
//! ## Poisoning
//!
//! Every lock is taken with `unwrap_or_else(PoisonError::into_inner)`. A panic
//! while holding the lock means a bug in *this* crate, not corrupt user data, and
//! trapping the host server on the next voice packet would be far worse than
//! continuing with a possibly half-updated counter.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use uuid::Uuid;

use crate::server::VoiceServer;

/// Maximum datagrams drained from the socket per tick.
///
/// Voice is real-time but loss-tolerant: a bounded budget keeps the server tick
/// responsive under a flood, and anything beyond it is simply dropped by the OS
/// buffer, exactly like an overloaded real-time server would.
pub const MAX_DATAGRAMS_PER_TICK: usize = 256;

/// Maximum size of a single inbound datagram. Plasmo Voice frames are far smaller,
/// but a generous buffer avoids truncating a malformed oversized packet before the
/// codec can reject it.
const RECV_BUFFER: usize = 4096;

/// The single voice data plane.
pub struct VoiceRuntime {
    /// The UDP listener. `None` before load, after a failed bind, or after unload.
    socket: Option<UdpSocket>,
    /// The running protocol state machine. `None` under the same conditions.
    protocol: Option<VoiceServer>,
    /// Where `state.json` lives, filled in by `on_load`.
    data_folder: String,
    /// Datagrams received over the lifetime of the plugin (for status reporting).
    received: AtomicU64,
    /// Datagrams sent over the lifetime of the plugin.
    sent: AtomicU64,
}

impl VoiceRuntime {
    /// An unloaded runtime: no socket, no protocol state, no data folder.
    #[must_use]
    pub fn new() -> Self {
        Self {
            socket: None,
            protocol: None,
            data_folder: String::new(),
            received: AtomicU64::new(0),
            sent: AtomicU64::new(0),
        }
    }

    /// Installs the socket, the protocol state machine and the data folder.
    ///
    /// This is the single transition from "not listening" to "listening".
    pub fn start(&mut self, socket: UdpSocket, protocol: VoiceServer, data_folder: String) {
        self.socket = Some(socket);
        self.protocol = Some(protocol);
        self.data_folder = data_folder;
    }

    /// Tears the data plane down, returning the data folder it was using.
    ///
    /// The socket is dropped here, which releases the port. The counters are left
    /// alone: they describe the plugin's lifetime, not one listening period, and
    /// the status IPC command reports them after a reload.
    pub fn stop(&mut self) -> String {
        self.socket = None;
        self.protocol = None;
        std::mem::take(&mut self.data_folder)
    }

    /// Whether the data plane is currently listening.
    #[must_use]
    pub fn is_listening(&self) -> bool {
        self.socket.is_some()
    }

    /// The data folder, or an empty string when not loaded.
    #[must_use]
    pub fn data_folder(&self) -> &str {
        &self.data_folder
    }

    /// The bound UDP port, or `None` when not listening.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.socket
            .as_ref()
            .and_then(|socket| socket.local_addr().ok())
            .map(|address| address.port())
    }

    /// The server secret UUID, or `None` when not loaded.
    #[must_use]
    pub fn server_secret(&self) -> Option<Uuid> {
        self.protocol.as_ref().map(VoiceServer::server_secret)
    }

    /// The number of connected voice clients, or `0` when not loaded.
    #[must_use]
    pub fn connection_count(&self) -> usize {
        self.protocol
            .as_ref()
            .map_or(0, VoiceServer::connection_count)
    }

    /// Datagrams received over the plugin's lifetime.
    #[must_use]
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Datagrams sent over the plugin's lifetime.
    #[must_use]
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }

    /// Drains up to [`MAX_DATAGRAMS_PER_TICK`] datagrams and answers them.
    ///
    /// Returns `(received, sent)` for this tick. A no-op when not listening, which
    /// is what makes it safe to call unconditionally from the tick pump.
    pub fn pump(&mut self) -> (u64, u64) {
        let (Some(socket), Some(protocol)) = (self.socket.as_ref(), self.protocol.as_mut()) else {
            return (0, 0);
        };

        let mut received = 0u64;
        let mut sent = 0u64;
        let mut buffer = [0u8; RECV_BUFFER];

        for _ in 0..MAX_DATAGRAMS_PER_TICK {
            let (len, from) = match socket.recv_from(&mut buffer) {
                Ok(read) => read,
                // The socket is non-blocking: an empty queue is the normal exit.
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    tracing::warn!(%error, "voice socket receive failed; stopping this tick");
                    break;
                }
            };
            received += 1;

            let handled = protocol.handle_datagram(&from.to_string(), &buffer[..len]);
            for outgoing in handled.outgoing {
                match outgoing.to.parse::<SocketAddr>() {
                    Ok(target) => match socket.send_to(&outgoing.data, target) {
                        Ok(_) => sent += 1,
                        Err(error) => {
                            // A dropped voice frame is expected under congestion and is
                            // not worth a warning: audio tolerates loss by design.
                            tracing::trace!(%error, peer = %outgoing.to, "voice send failed");
                        }
                    },
                    Err(error) => {
                        tracing::debug!(%error, peer = %outgoing.to, "could not parse peer address");
                    }
                }
            }
        }

        self.received.fetch_add(received, Ordering::Relaxed);
        self.sent.fetch_add(sent, Ordering::Relaxed);
        (received, sent)
    }

    /// Borrows the protocol state machine, or `None` when not loaded.
    ///
    /// Used by the IPC control plane, which reads connection tables and counters
    /// without touching the socket.
    pub fn protocol(&self) -> Option<&VoiceServer> {
        self.protocol.as_ref()
    }

    /// Borrows the protocol state machine mutably, or `None` when not loaded.
    pub fn protocol_mut(&mut self) -> Option<&mut VoiceServer> {
        self.protocol.as_mut()
    }
}

impl Default for VoiceRuntime {
    fn default() -> Self {
        Self::new()
    }
}

/// The one runtime instance.
fn runtime_slot() -> &'static Mutex<VoiceRuntime> {
    static SLOT: OnceLock<Mutex<VoiceRuntime>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(VoiceRuntime::new()))
}

/// Locks the global runtime.
///
/// The guard is handed out so callers can hold it across a `pump()`; it is
/// deliberately not re-entrant, because a tick arriving inside a tick would mean
/// the host is dispatching the same handler concurrently, which the single-threaded
/// guest cannot do.
pub fn runtime_lock() -> MutexGuard<'static, VoiceRuntime> {
    runtime_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Runs `f` against the global runtime.
///
/// A convenience wrapper for the many small read paths (counters, port, secret)
/// that do not want to name the guard type.
pub fn with_runtime<T>(f: impl FnOnce(&mut VoiceRuntime) -> T) -> T {
    let mut guard = runtime_lock();
    f(&mut guard)
}

/// Binds the UDP voice socket on `port`, returning it and the actual port.
///
/// Port `0` asks the OS for an ephemeral port, which is what a first run wants
/// before the real port is known: the caller persists whatever comes back so the
/// next load listens on the same one. The socket is put in non-blocking mode
/// because the pump must never stall a server tick.
///
/// # Errors
///
/// Fails when the port is taken or the host policy denies `network.udp.bind`.
pub fn bind(port: u16) -> Result<(UdpSocket, u16), String> {
    let socket = UdpSocket::bind(("0.0.0.0", port))
        .map_err(|error| format!("could not bind UDP 0.0.0.0:{port}: {error}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|error| format!("could not set the voice socket non-blocking: {error}"))?;
    let local = socket
        .local_addr()
        .map_err(|error| format!("could not query the voice socket address: {error}"))?;
    Ok((socket, local.port()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_runtime_is_not_listening() {
        let runtime = VoiceRuntime::new();
        assert!(!runtime.is_listening());
        assert_eq!(runtime.port(), None);
        assert_eq!(runtime.server_secret(), None);
        assert_eq!(runtime.connection_count(), 0);
        assert_eq!(runtime.received(), 0);
        assert_eq!(runtime.sent(), 0);
        assert_eq!(runtime.data_folder(), "");
    }

    #[test]
    fn pumping_an_unloaded_runtime_is_a_noop() {
        let mut runtime = VoiceRuntime::new();
        assert_eq!(runtime.pump(), (0, 0));
        assert_eq!(runtime.received(), 0);
    }

    #[test]
    fn stop_clears_the_data_plane_and_returns_the_folder() {
        let mut runtime = VoiceRuntime::new();
        runtime.start(
            UdpSocket::bind("127.0.0.1:0").expect("ephemeral bind"),
            VoiceServer::new(Uuid::nil()),
            "/tmp/voice".to_string(),
        );
        assert!(runtime.is_listening());
        assert_eq!(runtime.data_folder(), "/tmp/voice");

        assert_eq!(runtime.stop(), "/tmp/voice");
        assert!(!runtime.is_listening());
        assert_eq!(runtime.data_folder(), "");
        // The counters survive: they describe the plugin lifetime, not one period.
        assert_eq!(runtime.received(), 0);
    }
}
