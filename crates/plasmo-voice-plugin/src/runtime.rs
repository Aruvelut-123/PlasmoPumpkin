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

use crate::control::Outbound;
use crate::server::{KEEP_ALIVE_TIMEOUT_MS, Outgoing, VoiceServer, now_ms};

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
    /// How long a connection may stay silent before it is retired, mirroring
    /// upstream's `VoiceServerConfig.keepAliveTimeoutMs`.
    keep_alive_timeout_ms: u64,
    /// Per-tick datagram budget, the rate limit that bounds the pump; see
    /// [`MAX_DATAGRAMS_PER_TICK`] and [`VoiceRuntime::set_max_datagrams_per_tick`].
    max_datagrams_per_tick: usize,
    /// Control-plane messages waiting to be delivered over the `plasmo:voice` channel.
    ///
    /// The socket and the channel are driven from the same tick but cannot be used from
    /// the same borrow: the tick pump drains the socket here, then hands these to the
    /// channel sender. A queue is what keeps the protocol logic free of both.
    control: Vec<Outbound>,
    /// Players whose `PlayerInfoRequestPacket` is still unanswered, with the retry
    /// schedule (`PlayerInfoRequestScheduler` upstream).
    waiting_info: Vec<WaitingInfo>,
    /// The address advertised in `ConnectionPacket.ip`.
    ///
    /// `0.0.0.0` is not a placeholder: a Plasmo Voice client that reads it substitutes the
    /// host it is already connected to for Minecraft (`ModServerConnection:243-244`), which
    /// is exactly the right answer for a server whose voice port sits on the same machine
    /// — and it is the only answer a guest can give without knowing its own public IP.
    advertised_ip: String,
    /// Failed sends. Only the first is reported at `warn` (see [`send_datagrams`]).
    send_failures: u64,
    /// Whether the first tick has been announced, so a host that never ticks is
    /// noticeable from a log that only contains the load line.
    pump_announced: bool,
}

/// One unanswered `PlayerInfoRequestPacket`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WaitingInfo {
    player: Uuid,
    /// When the next request is due.
    next_at_ms: u64,
    /// How many requests have been sent already (upstream's maximum is 5).
    attempts: u8,
}

/// The delays between `PlayerInfoRequestPacket` retries, in ms.
///
/// `PlayerInfoRequestScheduler.schedule` upstream: the first request goes out on join, then
/// the 500 ms sweep retries at +1/3/5/10/15 s and gives up.
const INFO_RETRY_DELAYS_MS: [u64; 5] = [1_000, 3_000, 5_000, 10_000, 15_000];

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
            keep_alive_timeout_ms: KEEP_ALIVE_TIMEOUT_MS,
            max_datagrams_per_tick: MAX_DATAGRAMS_PER_TICK,
            control: Vec::new(),
            waiting_info: Vec::new(),
            advertised_ip: "0.0.0.0".to_string(),
            send_failures: 0,
            pump_announced: false,
        }
    }

    /// Overrides the address advertised to clients in `ConnectionPacket.ip`.
    pub fn set_advertised_ip(&mut self, ip: impl Into<String>) {
        self.advertised_ip = ip.into();
    }

    /// The address advertised to clients in `ConnectionPacket.ip`.
    #[must_use]
    pub fn advertised_ip(&self) -> &str {
        &self.advertised_ip
    }

    /// Queues a `PlayerInfoRequestPacket` for a player, with upstream's retry schedule.
    pub fn request_player_info(&mut self, player: Uuid, now: u64) {
        if self
            .waiting_info
            .iter()
            .any(|waiting| waiting.player == player)
        {
            return;
        }
        self.waiting_info.push(WaitingInfo {
            player,
            next_at_ms: now,
            attempts: 0,
        });
    }

    /// Stops retrying `player`: it answered, so the handshake is under way.
    pub fn player_identified(&mut self, player: &Uuid) {
        self.waiting_info
            .retain(|waiting| waiting.player != *player);
    }

    /// Emits the `PlayerInfoRequestPacket`s that are due.
    ///
    /// Called from [`Self::pump`], so the retry schedule rides the server tick — the same
    /// 500 ms cadence upstream's scheduler runs at, only finer (~20 Hz).
    fn schedule_info_requests(&mut self, now: u64) {
        let Some(protocol) = self.protocol.as_ref() else {
            return;
        };
        for waiting in &mut self.waiting_info {
            if now < waiting.next_at_ms {
                continue;
            }
            let payload = protocol.request_player_info();
            tracing::debug!(
                player = %waiting.player,
                attempt = waiting.attempts + 1,
                "asking a client to identify itself"
            );
            self.control
                .push(Outbound::to_player(waiting.player, payload));
            let delay = INFO_RETRY_DELAYS_MS
                [usize::from(waiting.attempts).min(INFO_RETRY_DELAYS_MS.len() - 1)];
            waiting.attempts = waiting.attempts.saturating_add(1);
            waiting.next_at_ms = now.saturating_add(delay);
        }
        // Upstream sends the request on join and retries five times; after that the client
        // is left alone until it registers the channel again.
        let max_attempts = INFO_RETRY_DELAYS_MS.len() as u8 + 1;
        self.waiting_info
            .retain(|waiting| waiting.attempts < max_attempts);
    }

    /// Overrides how long a silent connection may live.
    ///
    /// Upstream reads this from `voice.keepAliveTimeoutMs`; the plugin keeps the
    /// upstream default and exposes the knob so a host can tune it later.
    pub fn set_keep_alive_timeout(&mut self, timeout_ms: u64) {
        self.keep_alive_timeout_ms = timeout_ms;
    }

    /// How long a silent connection may live.
    #[must_use]
    pub fn keep_alive_timeout(&self) -> u64 {
        self.keep_alive_timeout_ms
    }

    /// Overrides the per-tick datagram budget — the rate limit that keeps a UDP
    /// flood from stalling a server tick. Read from `config.toml`'s
    /// `max_datagrams_per_tick`; `0` means "keep the plugin default",
    /// [`MAX_DATAGRAMS_PER_TICK`].
    pub fn set_max_datagrams_per_tick(&mut self, budget: usize) {
        self.max_datagrams_per_tick = if budget == 0 {
            MAX_DATAGRAMS_PER_TICK
        } else {
            budget
        };
    }

    /// The current per-tick datagram budget.
    #[must_use]
    pub fn max_datagrams_per_tick(&self) -> usize {
        self.max_datagrams_per_tick
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
        self.control.clear();
        self.waiting_info.clear();
        // A reload gets its own listening period, so the "the pump is running" line is
        // emitted again for it.
        self.pump_announced = false;
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

    /// A clone of the server-wide AES key, or `None` when not loaded.
    ///
    /// Cloned because the protocol keeps it owned; the only caller re-persists it
    /// after `stop` has torn the protocol down.
    #[must_use]
    pub fn server_aes_key(&self) -> Option<crate::crypto::AesKey> {
        self.protocol
            .as_ref()
            .map(|protocol| protocol.aes_key().clone())
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

    /// Drains up to [`MAX_DATAGRAMS_PER_TICK`] datagrams, answers them, and keeps
    /// idle connections alive.
    ///
    /// Returns `(received, sent)` for this tick. A no-op when not listening, which
    /// is what makes it safe to call unconditionally from the tick pump.
    pub fn pump(&mut self) -> (u64, u64) {
        let timeout_ms = self.keep_alive_timeout_ms;
        let (Some(socket), Some(protocol)) = (self.socket.as_ref(), self.protocol.as_mut()) else {
            return (0, 0);
        };

        // The tick pump is the data plane's only clock: if the host never calls this
        // handler, the socket is never drained and no ping is ever sent, which a real
        // client reports as "Can't connect to the UDP server". Saying so once turns the
        // hardest failure in the whole plugin into a single missing log line.
        if !self.pump_announced {
            self.pump_announced = true;
            let port = socket.local_addr().map(|address| address.port()).ok();
            tracing::info!(
                ?port,
                "the voice tick pump is running: the UDP socket on port {:?} is being drained",
                port
            );
        }

        let mut received = 0u64;
        let mut sent = 0u64;
        let mut buffer = [0u8; RECV_BUFFER];
        let mut control: Vec<Outbound> = Vec::new();

        for _ in 0..self.max_datagrams_per_tick {
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
            sent += send_datagrams(socket, handled.outgoing, &mut self.send_failures);
            control.extend(handled.control);
        }

        // Keep-alive shares the socket's clock. Pumpkin 0.2.0 puts no `on_tick` on
        // the `Plugin` trait, so this tick *event* is the only timer the guest has
        // — the same job upstream schedules every 100ms in `NettyUdpKeepAlive`.
        //
        // The cadence matters more than it looks: a Plasmo Voice client treats itself as
        // `connected` only while it keeps *receiving* pings, goes soft-dead after 7s
        // without one (it stops sending audio) and tears the whole connection down at
        // 30s. The sweep pings a new connection immediately and then every 1.5-3s, which
        // sits comfortably inside that window.
        let sweep = protocol.keep_alive(now_ms(), timeout_ms);
        sent += send_datagrams(socket, sweep.outgoing, &mut self.send_failures);
        control.extend(sweep.control);
        self.control.extend(control);

        // A timed-out connection is not the end of the session: upstream re-asks the
        // player for its info, which sends a fresh `ConnectionPacket` and lets a client
        // whose UDP path broke recover without rejoining. Doing the same here is what
        // keeps a momentary network blip from requiring a reconnect.
        for player in sweep.timed_out_players {
            self.request_player_info(player, now_ms());
        }

        // `PlayerInfoRequestPacket` retries, on the same clock as everything else.
        self.schedule_info_requests(now_ms());

        self.received.fetch_add(received, Ordering::Relaxed);
        self.sent.fetch_add(sent, Ordering::Relaxed);
        (received, sent)
    }

    /// Takes the control-plane messages produced since the last call.
    ///
    /// The tick pump drains the socket; whoever owns the channel sends these. Splitting
    /// the two keeps the socket borrow and the channel borrow from overlapping.
    pub fn take_control(&mut self) -> Vec<Outbound> {
        std::mem::take(&mut self.control)
    }

    /// Queues a control-plane message for the next [`Self::pump`] consumer.
    pub fn push_control(&mut self, message: Outbound) {
        self.control.push(message);
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

/// Sends each datagram, returning how many left the socket.
///
/// Split out of [`VoiceRuntime::pump`] because both the reply path and the
/// keep-alive sweep send through it.
///
/// A send failure is *not* automatically benign. Losing an audio frame to
/// congestion is normal, but the host denying `network.udp.outgoing-datagram`
/// (or `network.udp.connect`) fails *every* send, and from the client's side that
/// is indistinguishable from "the server is not running" — its symptom is exactly
/// the client's "Can't connect to the UDP server" screen. So the first failure is
/// reported at `warn` and the rest at `debug`: loud enough to diagnose, quiet
/// enough to survive a lossy link.
fn send_datagrams(socket: &UdpSocket, outgoing: Vec<Outgoing>, failures: &mut u64) -> u64 {
    let mut sent = 0u64;
    for datagram in outgoing {
        match datagram.to.parse::<SocketAddr>() {
            Ok(target) => match socket.send_to(&datagram.data, target) {
                Ok(_) => sent += 1,
                Err(error) => {
                    if *failures == 0 {
                        tracing::warn!(
                            %error,
                            peer = %datagram.to,
                            bytes = datagram.data.len(),
                            "voice send to {} failed: {error}. If this repeats for every \
                             packet, the host is denying the plugin's network.udp \
                             permissions",
                            datagram.to
                        );
                    } else {
                        tracing::debug!(%error, peer = %datagram.to, "voice send failed");
                    }
                    *failures = failures.saturating_add(1);
                }
            },
            Err(error) => {
                tracing::warn!(%error, peer = %datagram.to, "could not parse a peer address");
            }
        }
    }
    sent
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
            VoiceServer::new(
                Uuid::nil(),
                crate::crypto::AesKey::from_hex("00112233445566778899aabbccddeeff")
                    .expect("test key"),
            ),
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
