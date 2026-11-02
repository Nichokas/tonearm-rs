//! iroh transport for Listen Together (feature `live-net`).
//!
//! Star topology: guests dial the host's endpoint with ALPN `sdat-live/1` and
//! keep one bidirectional stream of length-prefixed messages. iroh tries a
//! direct connection first and falls back to a relay. Relays are configurable
//! ([`NetConfig`]); no key or secret lives in the code.

#![allow(clippy::clone_on_ref_ptr, clippy::semicolon_outside_block, clippy::needless_pass_by_value, reason = "async task wiring: clones move into spawned tasks")]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use iroh::endpoint::{Connection, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, RelayMap, RelayMode, RelayUrl};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

use super::drift::Correction;
use super::proto::{self, EndReason, Msg, RejectReason};
use super::queue::{OpKind, QueueItem};
use super::session::{GuestEvent, GuestInfo, GuestPhase, GuestSession, HostSession, Target, To};
use super::ticket::Ticket;
use super::{ALPN, PROTO_VERSION, mono_now_ns, new_id};

/// Relay of the SDAT project on iroh services (public URL, not a secret).
pub const DEFAULT_RELAYS: &[&str] = &["https://efc2pr1s527pykb0a.euc1.relay.iroh-svc.com/"];
/// Comma-separated relay URLs overriding [`DEFAULT_RELAYS`].
pub const RELAYS_ENV: &str = "SDAT_LIVE_RELAYS";

#[derive(Debug, Clone)]
pub struct NetConfig {
    /// Preferred relays, tried as home relay first.
    pub relays: Vec<String>,
    /// Also use n0's public relays as fallback.
    pub n0_fallback: bool,
}

impl Default for NetConfig {
    /// [`DEFAULT_RELAYS`] (or `SDAT_LIVE_RELAYS`) plus n0's public relays.
    fn default() -> Self {
        let relays = std::env::var(RELAYS_ENV)
            .ok()
            .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_RELAYS.iter().map(|s| (*s).to_owned()).collect());
        Self { relays, n0_fallback: true }
    }
}

impl NetConfig {
    fn relay_mode(&self) -> RelayMode {
        let mut urls: Vec<RelayUrl> = self.relays.iter().filter_map(|u| u.parse().ok()).collect();
        if self.n0_fallback {
            urls.extend(RelayMode::Default.relay_map().urls::<Vec<_>>());
        }
        if urls.is_empty() { RelayMode::Default } else { RelayMode::Custom(urls.into_iter().collect::<RelayMap>()) }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("{0}")]
    Ticket(#[from] super::ticket::TicketError),
    #[error("network: {0}")]
    Net(String),
}

fn net<E: std::fmt::Display>(e: E) -> NetError {
    NetError::Net(e.to_string())
}

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread().worker_threads(2).thread_name("live").enable_all().build().expect("tokio runtime")
    })
}

async fn bind(cfg: &NetConfig, alpns: Vec<Vec<u8>>) -> Result<Endpoint, NetError> {
    Endpoint::builder(presets::N0).relay_mode(cfg.relay_mode()).alpns(alpns).bind().await.map_err(net)
}

async fn write_msg(send: &mut SendStream, m: &Msg) -> Result<(), NetError> {
    let f = proto::encode(m).map_err(net)?;
    send.write_all(&f).await.map_err(net)
}

async fn read_msg(recv: &mut RecvStream) -> Result<Msg, NetError> {
    let mut h = [0u8; 4];
    recv.read_exact(&mut h).await.map_err(net)?;
    let mut body = vec![0u8; proto::frame_len(h).map_err(net)?];
    recv.read_exact(&mut body).await.map_err(net)?;
    proto::decode(&body).map_err(net)
}

/// Messages buffered between the frame reader and the session.
const READ_BUFFER: usize = 32;

/// One-shot stop signal. Unlike a bare `Notify::notify_waiters`, a task
/// that starts waiting after the signal still sees it.
#[derive(Debug, Default)]
struct Stop {
    flag: AtomicBool,
    n: tokio::sync::Notify,
}

impl Stop {
    /// Returns true the first time only.
    fn trigger(&self) -> bool {
        let first = !self.flag.swap(true, Ordering::SeqCst);
        self.n.notify_waiters();
        first
    }
    fn is_set(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }
    async fn wait(&self) {
        loop {
            let f = self.n.notified();
            tokio::pin!(f);
            f.as_mut().enable();
            if self.is_set() {
                return;
            }
            f.await;
        }
    }
}

/// Reads whole messages in their own task, so callers can `select!` on the
/// channel without cancelling a half-read frame (`read_exact` is not
/// cancel-safe).
/// The channel is bounded: when the consumer falls behind, the reader stops
/// reading and QUIC flow control pushes back on the host.
fn spawn_reader(mut recv: RecvStream) -> (mpsc::Receiver<Msg>, tokio::task::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(READ_BUFFER);
    let h = tokio::spawn(async move {
        while let Ok(m) = read_msg(&mut recv).await {
            if tx.send(m).await.is_err() {
                break;
            }
        }
    });
    (rx, h)
}

/// Everything the UI needs to hear about.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveEvent {
    /// Host: guests list changed.
    Guests(Vec<GuestInfo>),
    Guest(GuestEvent),
    /// Guest: connection dropped, retrying.
    Reconnecting,
}

pub trait LiveListener: Send + Sync {
    fn on_event(&self, ev: LiveEvent);
}

// ---------------------------------------------------------------- host

pub struct LiveHost {
    state: Arc<Mutex<HostSession>>,
    peers: Peers,
    endpoint: Endpoint,
    ticket: Ticket,
    stop: Arc<Stop>,
}

impl std::fmt::Debug for LiveHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveHost").finish_non_exhaustive()
    }
}

/// Accepted guests only: (connection token, sender). The token tells a
/// reconnected guest's new connection apart from its old one.
type Peers = Arc<Mutex<HashMap<String, (u64, Outbox)>>>;

/// Messages queued for one guest before it counts as too slow.
const OUT_BUFFER: usize = 64;

/// Bounded outbound queue of one guest connection. A guest that stops
/// reading fills it; it is then disconnected instead of growing memory.
#[derive(Debug, Clone)]
struct Outbox {
    tx: mpsc::Sender<Msg>,
    kicked: Arc<Stop>,
}

impl Outbox {
    fn new() -> (Self, mpsc::Receiver<Msg>) {
        let (tx, rx) = mpsc::channel(OUT_BUFFER);
        (Self { tx, kicked: Arc::default() }, rx)
    }
    fn send(&self, m: Msg) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(m) {
            self.kicked.trigger();
        }
    }
}

fn next_token() -> u64 {
    static N: AtomicU64 = AtomicU64::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn route(peers: &Peers, outs: Vec<super::session::Out>) {
    route_from(peers, outs, None);
}

/// Like [`route`]; `own` delivers replies (e.g. `Reject`) to a connection
/// that is not registered because its `Hello` was not accepted.
fn route_from(peers: &Peers, outs: Vec<super::session::Out>, own: Option<(&str, &Outbox)>) {
    let p = peers.lock().unwrap_or_else(PoisonError::into_inner);
    for o in outs {
        match o.to {
            To::All => p.values().for_each(|(_, tx)| tx.send(o.msg.clone())),
            To::Peer(id) => {
                if let Some((_, tx)) = p.get(&id) {
                    tx.send(o.msg);
                } else if let Some((own_id, tx)) = own
                    && own_id == id
                {
                    tx.send(o.msg);
                }
            }
        }
    }
}

/// Removes `peer` only if `token` still owns it. Returns whether it did.
fn unregister(peers: &Peers, peer: &str, token: u64) -> bool {
    let mut p = peers.lock().unwrap_or_else(PoisonError::into_inner);
    if p.get(peer).is_some_and(|(t, _)| *t == token) {
        p.remove(peer);
        true
    } else {
        false
    }
}

impl LiveHost {
    /// Opens a session. Returns once the endpoint is online (or after a
    /// short timeout, so a relay outage still leaves direct connections).
    pub fn start(name: String, cfg: &NetConfig, listener: Arc<dyn LiveListener>) -> Result<Self, NetError> {
        runtime().block_on(async {
            let endpoint = bind(cfg, vec![ALPN.to_vec()]).await?;
            drop(tokio::time::timeout(Duration::from_secs(5), endpoint.online()).await);
            let state = Arc::new(Mutex::new(HostSession::new(name.clone(), mono_now_ns())));
            let secret = state.lock().unwrap_or_else(PoisonError::into_inner).secret.clone();
            let addr = serde_json::to_string(&endpoint.addr()).map_err(net)?;
            let ticket = Ticket { v: PROTO_VERSION, addr, secret, name: Some(name) };
            let peers: Peers = Arc::default();
            let stop = Arc::new(Stop::default());
            // Accept loop.
            {
                let (ep, state, peers, listener, stop) = (endpoint.clone(), state.clone(), peers.clone(), listener.clone(), stop.clone());
                tokio::spawn(async move {
                    loop {
                        let inc = tokio::select! { i = ep.accept() => i, () = stop.wait() => None };
                        let Some(inc) = inc else { break };
                        let (state, peers, listener) = (state.clone(), peers.clone(), listener.clone());
                        tokio::spawn(async move {
                            if let Ok(conn) = inc.await {
                                serve_guest(conn, state, peers, listener).await;
                            }
                        });
                    }
                });
            }
            // Ticks: heartbeats, debounced anchors.
            {
                let (state, peers, stop) = (state.clone(), peers.clone(), stop.clone());
                tokio::spawn(async move {
                    let mut iv = tokio::time::interval(Duration::from_millis(250));
                    loop {
                        tokio::select! { _ = iv.tick() => {}, () = stop.wait() => break }
                        let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
                        let outs = s.tick(mono_now_ns());
                        route(&peers, outs);
                    }
                });
            }
            Ok(Self { state, peers, endpoint, ticket, stop })
        })
    }

    pub fn ticket_url(&self) -> String {
        self.ticket.to_url()
    }

    pub fn guests(&self) -> Vec<GuestInfo> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).guests()
    }

    pub fn set_playback(&self, item_id: Option<String>, track_id: Option<String>, position_ms: i64, playing: bool) {
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let outs = s.set_playback(item_id, track_id, position_ms, playing, mono_now_ns());
        route(&self.peers, outs);
    }

    /// Replaces the shared queue; returns the entry ids given to each track.
    pub fn replace_queue(&self, track_ids: Vec<String>) -> Result<Vec<String>, NetError> {
        let items: Vec<QueueItem> = track_ids.into_iter().take(super::MAX_QUEUE).map(|track_id| QueueItem { item_id: new_id(), track_id }).collect();
        let ids = items.iter().map(|i| i.item_id.clone()).collect();
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let outs = s.queue_op(OpKind::Replace { items }).map_err(net)?;
        route(&self.peers, outs);
        drop(s);
        Ok(ids)
    }

    /// Ends the session for everyone.
    pub fn end(&self) {
        if !self.stop.trigger() {
            return;
        }
        let mut s = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let outs = s.end();
        route(&self.peers, outs);
        drop(s);
        let ep = self.endpoint.clone();
        runtime().spawn(async move {
            // Let the End message flush before closing.
            tokio::time::sleep(Duration::from_millis(500)).await;
            ep.close().await;
        });
    }
}

async fn serve_guest(conn: Connection, state: Arc<Mutex<HostSession>>, peers: Peers, listener: Arc<dyn LiveListener>) {
    let peer = conn.remote_id().to_string();
    let Ok((mut send, mut recv)) = conn.accept_bi().await else { return };
    let (tx, mut rx) = Outbox::new();
    let token = next_token();
    let mut registered = false;
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let end = matches!(m, Msg::End { .. } | Msg::Reject { .. });
            if write_msg(&mut send, &m).await.is_err() {
                break;
            }
            if end {
                drop(send.finish());
                break;
            }
        }
    });
    let mut known = 0;
    loop {
        // Dropping a half-read frame is fine here: a kicked guest is gone.
        let m = tokio::select! {
            m = read_msg(&mut recv) => m,
            () = tx.kicked.wait() => break,
        };
        let Ok(m) = m else { break };
        let now = mono_now_ns();
        // Snapshot (Welcome), registration and routing happen under the
        // state lock, and every other sender routes under it too, so no
        // queue or playback update can fall between them.
        let guests = {
            let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
            let outs = s.on_message(&peer, m, now);
            let guests = s.guests();
            // Broadcasts reach a connection only once its Hello is accepted.
            if !registered && guests.iter().any(|g| g.peer == peer) {
                registered = true;
                peers.lock().unwrap_or_else(PoisonError::into_inner).insert(peer.clone(), (token, tx.clone()));
            }
            route_from(&peers, outs, Some((&peer, &tx)));
            guests
        };
        if guests.len() != known {
            known = guests.len();
            listener.on_event(LiveEvent::Guests(guests));
        }
    }
    if tx.kicked.is_set() {
        conn.close(1u32.into(), b"too slow");
    }
    drop(tx);
    // A newer connection from the same guest may own the entry by now.
    if !registered || !unregister(&peers, &peer, token) {
        writer.abort();
        return;
    }
    let guests = {
        let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
        s.peer_disconnected(&peer);
        s.guests()
    };
    listener.on_event(LiveEvent::Guests(guests));
    writer.abort();
}

impl Drop for LiveHost {
    /// Releasing the handle (e.g. Swift ARC) ends the session.
    fn drop(&mut self) {
        self.end();
    }
}

// ---------------------------------------------------------------- guest

pub struct LiveGuest {
    state: Arc<Mutex<GuestSession>>,
    tx: Arc<Mutex<Option<mpsc::UnboundedSender<Msg>>>>,
    stop: Arc<Stop>,
    left: AtomicBool,
}

impl std::fmt::Debug for LiveGuest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveGuest").finish_non_exhaustive()
    }
}

fn finished(s: &GuestSession) -> bool {
    matches!(s.phase(), GuestPhase::Ended(_) | GuestPhase::Rejected(_))
}

impl LiveGuest {
    /// Parses `sdat://join/...` and starts connecting in the background.
    /// Progress arrives through `listener` (`Joined`, `Rejected`, `Ended`).
    pub fn join(url: &str, name: String, cfg: &NetConfig, listener: Arc<dyn LiveListener>) -> Result<Self, NetError> {
        let ticket = Ticket::parse(url)?;
        let addr: EndpointAddr = serde_json::from_str(&ticket.addr).map_err(|_e| NetError::Ticket(super::ticket::TicketError::Malformed))?;
        let state = Arc::new(Mutex::new(GuestSession::new(ticket.secret, name, mono_now_ns())));
        let tx: Arc<Mutex<Option<mpsc::UnboundedSender<Msg>>>> = Arc::default();
        let stop = Arc::new(Stop::default());
        let cfg = cfg.clone();
        {
            let (state, tx, stop, listener) = (state.clone(), tx.clone(), stop.clone(), listener.clone());
            runtime().spawn(async move {
                let ep = match bind(&cfg, Vec::new()).await {
                    Ok(ep) => ep,
                    Err(_) => {
                        listener.on_event(LiveEvent::Guest(GuestEvent::Ended(EndReason::HostLost)));
                        return;
                    }
                };
                let mut first = true;
                loop {
                    if finished(&state.lock().unwrap_or_else(PoisonError::into_inner)) {
                        break;
                    }
                    if !first {
                        listener.on_event(LiveEvent::Reconnecting);
                    }
                    // The host timeout keeps running while we dial.
                    let attempt = tokio::select! {
                        r = tokio::time::timeout(Duration::from_secs(8), ep.connect(addr.clone(), ALPN)) => r,
                        () = watch_timeout(&state, &listener) => break,
                        () = stop.wait() => break,
                    };
                    if let Ok(Ok(conn)) = attempt
                        && let Ok((send, recv)) = conn.open_bi().await
                    {
                        let hello = {
                            let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
                            if first { vec![s.hello()] } else { s.on_reconnect(mono_now_ns()) }
                        };
                        first = false;
                        run_guest_conn(send, recv, hello, &state, &tx, &listener, &stop).await;
                        conn.close(0u32.into(), b"bye");
                    }
                    // The session's own timeout (no host for 10 s) ends it.
                    let (_, ev) = state.lock().unwrap_or_else(PoisonError::into_inner).tick(mono_now_ns());
                    ev.into_iter().for_each(|e| listener.on_event(LiveEvent::Guest(e)));
                    tokio::select! { () = tokio::time::sleep(Duration::from_secs(1)) => {}, () = stop.wait() => break }
                }
                ep.close().await;
            });
        }
        Ok(Self { state, tx, stop, left: AtomicBool::new(false) })
    }

    /// Where the local player should be (`latency_ms`: output latency +
    /// manual offset). `None` until the clock is calibrated.
    pub fn target(&self, latency_ms: i64) -> Option<Target> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).target(mono_now_ns(), latency_ms)
    }

    pub fn correction(&self, latency_ms: i64, actual_ms: i64) -> Correction {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).correction(mono_now_ns(), latency_ms, actual_ms)
    }

    pub fn queue(&self) -> Vec<QueueItem> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).queue_items().to_vec()
    }

    pub fn host_name(&self) -> String {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).host_name.clone()
    }

    pub fn set_seek_only(&self, on: bool) {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).set_seek_only(on);
    }

    pub fn reject_reason(&self) -> Option<RejectReason> {
        match self.state.lock().unwrap_or_else(PoisonError::into_inner).phase() {
            GuestPhase::Rejected(r) => Some(*r),
            _ => None,
        }
    }

    pub fn leave(&self) {
        if self.left.swap(true, Ordering::SeqCst) {
            return;
        }
        let msgs = self.state.lock().unwrap_or_else(PoisonError::into_inner).leave();
        if let Some(tx) = self.tx.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
            msgs.into_iter().for_each(|m| drop(tx.send(m)));
        }
        let stop = self.stop.clone();
        runtime().spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            stop.trigger();
        });
    }
}

impl Drop for LiveGuest {
    /// Releasing the handle (e.g. Swift ARC) leaves the session.
    fn drop(&mut self) {
        self.leave();
    }
}

/// Ticks the guest session every 250 ms; returns once it has finished
/// (host timeout, end or reject), emitting its events.
async fn watch_timeout(state: &Arc<Mutex<GuestSession>>, listener: &Arc<dyn LiveListener>) {
    let mut iv = tokio::time::interval(Duration::from_millis(250));
    loop {
        iv.tick().await;
        let ev = {
            let mut s = state.lock().unwrap_or_else(PoisonError::into_inner);
            let (_, ev) = s.tick(mono_now_ns());
            if !finished(&s) && ev.is_empty() {
                continue;
            }
            (ev, finished(&s))
        };
        ev.0.into_iter().for_each(|e| listener.on_event(LiveEvent::Guest(e)));
        if ev.1 {
            return;
        }
    }
}

async fn run_guest_conn(
    mut send: SendStream,
    recv: RecvStream,
    hello: Vec<Msg>,
    state: &Arc<Mutex<GuestSession>>,
    tx_slot: &Arc<Mutex<Option<mpsc::UnboundedSender<Msg>>>>,
    listener: &Arc<dyn LiveListener>,
    stop: &Arc<Stop>,
) {
    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    *tx_slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(tx.clone());
    hello.into_iter().for_each(|m| drop(tx.send(m)));
    let writer = tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let bye = matches!(m, Msg::Bye);
            if write_msg(&mut send, &m).await.is_err() {
                break;
            }
            if bye {
                drop(send.finish());
                break;
            }
        }
    });
    let emit = |ev: Vec<GuestEvent>| ev.into_iter().for_each(|e| listener.on_event(LiveEvent::Guest(e)));
    let (mut inbox, reader) = spawn_reader(recv);
    let mut iv = tokio::time::interval(Duration::from_millis(100));
    loop {
        tokio::select! {
            m = inbox.recv() => {
                let Some(m) = m else { break };
                let (out, ev) = state.lock().unwrap_or_else(PoisonError::into_inner).on_message(m, mono_now_ns());
                out.into_iter().for_each(|m| drop(tx.send(m)));
                emit(ev);
            }
            _ = iv.tick() => {
                let (out, ev) = state.lock().unwrap_or_else(PoisonError::into_inner).tick(mono_now_ns());
                out.into_iter().for_each(|m| drop(tx.send(m)));
                emit(ev);
            }
            () = stop.wait() => break,
        }
        if finished(&state.lock().unwrap_or_else(PoisonError::into_inner)) {
            break;
        }
    }
    reader.abort();
    *tx_slot.lock().unwrap_or_else(PoisonError::into_inner) = None;
    drop(tx);
    drop(tokio::time::timeout(Duration::from_millis(500), writer).await);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_prefers_project_relay_then_n0() {
        let cfg = NetConfig { relays: DEFAULT_RELAYS.iter().map(|s| (*s).to_owned()).collect(), n0_fallback: true };
        let RelayMode::Custom(map) = cfg.relay_mode() else { panic!() };
        let urls: Vec<RelayUrl> = map.urls();
        assert!(urls.len() > 1);
        assert!(urls.iter().any(|u| u.as_str().contains("iroh-svc.com")));
        let only = NetConfig { relays: vec![], n0_fallback: false };
        assert_eq!(only.relay_mode(), RelayMode::Default);
    }

    fn sender() -> Outbox {
        Outbox::new().0
    }

    #[test]
    fn guest_that_stops_reading_is_kicked() {
        let (out, mut rx) = Outbox::new();
        for _ in 0..OUT_BUFFER {
            out.send(Msg::Bye);
        }
        assert!(!out.kicked.is_set(), "a full but not overflowing buffer is fine");
        out.send(Msg::Bye);
        assert!(out.kicked.is_set(), "overflow disconnects the guest");
        // Memory stays bounded: nothing beyond the buffer was queued.
        let mut n = 0;
        while rx.try_recv().is_ok() {
            n += 1;
        }
        assert_eq!(n, OUT_BUFFER);
    }

    #[test]
    fn stale_connection_does_not_remove_newer_one() {
        let peers: Peers = Arc::default();
        peers.lock().unwrap().insert("g".into(), (1, sender()));
        // Guest reconnects: new connection (token 2) takes over the entry.
        peers.lock().unwrap().insert("g".into(), (2, sender()));
        assert!(!unregister(&peers, "g", 1), "old handler must not remove the new entry");
        assert!(peers.lock().unwrap().contains_key("g"));
        assert!(unregister(&peers, "g", 2));
        assert!(peers.lock().unwrap().is_empty());
    }

    #[test]
    fn unaccepted_connection_gets_only_its_own_replies() {
        let peers: Peers = Arc::default();
        let (atx, mut arx) = Outbox::new();
        peers.lock().unwrap().insert("accepted".into(), (1, atx));
        let (utx, mut urx) = Outbox::new();
        let outs = vec![
            super::super::session::Out { to: To::All, msg: Msg::Bye },
            super::super::session::Out { to: To::Peer("stranger".into()), msg: Msg::Reject { reason: RejectReason::BadSecret, proto: PROTO_VERSION } },
        ];
        route_from(&peers, outs, Some(("stranger", &utx)));
        assert!(matches!(arx.try_recv(), Ok(Msg::Bye)));
        assert!(matches!(urx.try_recv(), Ok(Msg::Reject { .. })), "reject reaches the caller");
        assert!(urx.try_recv().is_err(), "broadcast does not reach an unaccepted connection");
    }

    #[test]
    fn stop_seen_by_late_waiter() {
        let stop = Stop::default();
        assert!(stop.trigger());
        assert!(!stop.trigger());
        runtime().block_on(async { tokio::time::timeout(Duration::from_secs(1), stop.wait()).await.expect("late waiter sees stop") });
    }

    #[derive(Default)]
    struct Rec(Mutex<Vec<LiveEvent>>);
    impl LiveListener for Rec {
        fn on_event(&self, ev: LiveEvent) {
            self.0.lock().unwrap().push(ev);
        }
    }

    /// Real iroh connection between two endpoints on this machine. Needs
    /// network (relays / address lookup), so it is ignored by default:
    /// `cargo test -p tonearm-core --features live-net -- --ignored live_loopback`
    #[test]
    #[ignore = "needs network"]
    fn live_loopback() {
        let cfg = NetConfig::default();
        let hl = Arc::new(Rec::default());
        let host = LiveHost::start("Host".into(), &cfg, hl.clone()).unwrap();
        host.replace_queue(vec!["1".into(), "2".into(), "3".into()]).unwrap();
        let q = host.state.lock().unwrap().queue_items().to_vec();
        host.set_playback(Some(q[1].item_id.clone()), Some("2".into()), 30_000, true);
        let gl = Arc::new(Rec::default());
        let guest = LiveGuest::join(&host.ticket_url(), "Guest".into(), &cfg, gl.clone()).unwrap();
        let mut target = None;
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(100));
            target = guest.target(0);
            if target.is_some() {
                break;
            }
        }
        let t = target.expect("guest calibrated and got the anchor");
        assert_eq!(t.track_id.as_deref(), Some("2"));
        assert_eq!(guest.queue().len(), 3);
        assert!(hl.0.lock().unwrap().iter().any(|e| matches!(e, LiveEvent::Guests(g) if g.len() == 1)));
        host.end();
        std::thread::sleep(Duration::from_secs(2));
        assert!(gl.0.lock().unwrap().contains(&LiveEvent::Guest(GuestEvent::Ended(EndReason::HostEnded))));
    }
}
