//! Listen Together for Swift: host/guest handles over `tonearm_core::live`.

use std::sync::Arc;

use tonearm_core::live::drift::Correction;
use tonearm_core::live::net::{self, LiveEvent, LiveListener, NetConfig};
use tonearm_core::live::proto::{EndReason, RejectReason};
use tonearm_core::live::session::GuestEvent;

use crate::FfiError;

#[derive(Debug, Clone, uniffi::Record)]
pub struct LiveGuestInfo {
    pub peer: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum LiveEnd {
    /// The host ended the session.
    HostEnded,
    /// Lost contact with the host.
    HostLost,
    Left,
    /// Host refused: wrong or old invitation.
    BadInvite,
    /// Host runs another protocol version: update SDAT.
    Incompatible,
    Full,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum LiveUpdate {
    Guests { guests: Vec<LiveGuestInfo> },
    Joined { host_name: String },
    QueueChanged,
    PlaybackChanged,
    Reconnecting,
    Ended { reason: LiveEnd },
}

/// Implemented in Swift; called from a background thread.
#[uniffi::export(with_foreign)]
pub trait LiveObserver: Send + Sync {
    fn on_update(&self, update: LiveUpdate);
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct LiveRelayConfig {
    /// Preferred relay URLs; empty uses the built-in default.
    pub relays: Vec<String>,
    pub n0_fallback: bool,
}

fn cfg(c: Option<LiveRelayConfig>) -> NetConfig {
    match c {
        Some(c) if !c.relays.is_empty() => NetConfig { relays: c.relays, n0_fallback: c.n0_fallback },
        Some(c) => NetConfig { n0_fallback: c.n0_fallback, ..NetConfig::default() },
        None => NetConfig::default(),
    }
}

struct Bridge(Arc<dyn LiveObserver>);

fn end_of(r: EndReason) -> LiveEnd {
    match r {
        EndReason::HostEnded | EndReason::Other => LiveEnd::HostEnded,
        EndReason::HostLost => LiveEnd::HostLost,
        EndReason::Left => LiveEnd::Left,
    }
}

fn reject_of(r: RejectReason) -> LiveEnd {
    match r {
        RejectReason::IncompatibleVersion => LiveEnd::Incompatible,
        RejectReason::Full => LiveEnd::Full,
        RejectReason::BadSecret | RejectReason::Other => LiveEnd::BadInvite,
    }
}

impl LiveListener for Bridge {
    fn on_event(&self, ev: LiveEvent) {
        let u = match ev {
            LiveEvent::Guests(g) => LiveUpdate::Guests { guests: g.into_iter().map(|g| LiveGuestInfo { peer: g.peer, name: g.name }).collect() },
            LiveEvent::Reconnecting => LiveUpdate::Reconnecting,
            LiveEvent::Guest(GuestEvent::Joined { host_name }) => LiveUpdate::Joined { host_name },
            LiveEvent::Guest(GuestEvent::QueueChanged) => LiveUpdate::QueueChanged,
            LiveEvent::Guest(GuestEvent::PlaybackChanged) => LiveUpdate::PlaybackChanged,
            LiveEvent::Guest(GuestEvent::Ended(r)) => LiveUpdate::Ended { reason: end_of(r) },
            LiveEvent::Guest(GuestEvent::Rejected(r)) => LiveUpdate::Ended { reason: reject_of(r) },
        };
        self.0.on_update(u);
    }
}

fn err(e: net::NetError) -> FfiError {
    FfiError::Other { message: e.to_string() }
}

/// A session this device hosts.
#[derive(Debug, uniffi::Object)]
pub struct LiveHost(net::LiveHost);

#[uniffi::export]
impl LiveHost {
    /// Opens a session (blocks up to ~5 s while the endpoint goes online).
    #[uniffi::constructor]
    pub async fn start(name: String, relays: Option<LiveRelayConfig>, observer: Arc<dyn LiveObserver>) -> Result<Arc<Self>, FfiError> {
        let c = cfg(relays);
        let h = blocking::unblock(move || net::LiveHost::start(name, &c, Arc::new(Bridge(observer)))).await.map_err(err)?;
        Ok(Arc::new(Self(h)))
    }
    /// `sdat://join/...`, for the QR code.
    pub fn invite_url(&self) -> String {
        self.0.ticket_url()
    }
    pub fn guests(&self) -> Vec<LiveGuestInfo> {
        self.0.guests().into_iter().map(|g| LiveGuestInfo { peer: g.peer, name: g.name }).collect()
    }
    /// Replaces the shared queue (max 500); returns one entry id per track.
    pub fn set_queue(&self, track_ids: Vec<String>) -> Result<Vec<String>, FfiError> {
        self.0.replace_queue(track_ids).map_err(err)
    }
    /// Call on play, pause, seek and track change.
    pub fn set_playback(&self, item_id: Option<String>, track_id: Option<String>, position_ms: i64, playing: bool) {
        self.0.set_playback(item_id, track_id, position_ms, playing);
    }
    pub fn end(&self) {
        self.0.end();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LiveQueueEntry {
    pub item_id: String,
    pub track_id: String,
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct LiveTarget {
    pub item_id: Option<String>,
    pub track_id: Option<String>,
    pub position_ms: i64,
    pub playing: bool,
}

#[derive(Debug, Clone, Copy, uniffi::Enum)]
pub enum LiveCorrection {
    None,
    Rate { rate: f64 },
    Seek { position_ms: i64 },
}

/// A session this device joined.
#[derive(Debug, uniffi::Object)]
pub struct LiveGuest(net::LiveGuest);

#[uniffi::export]
impl LiveGuest {
    /// Starts joining from an `sdat://join/...` link or QR content.
    #[uniffi::constructor]
    pub fn join(invite: String, name: String, relays: Option<LiveRelayConfig>, observer: Arc<dyn LiveObserver>) -> Result<Arc<Self>, FfiError> {
        let g = net::LiveGuest::join(&invite, name, &cfg(relays), Arc::new(Bridge(observer))).map_err(err)?;
        Ok(Arc::new(Self(g)))
    }
    pub fn host_name(&self) -> String {
        self.0.host_name()
    }
    /// Shared queue in order; `item_id` matches [`LiveTarget::item_id`], so
    /// repeated tracks can be told apart.
    pub fn queue(&self) -> Vec<LiveQueueEntry> {
        self.0.queue().into_iter().map(|i| LiveQueueEntry { item_id: i.item_id, track_id: i.track_id }).collect()
    }
    /// Shared queue as track ids, in order.
    pub fn queue_track_ids(&self) -> Vec<String> {
        self.0.queue().into_iter().map(|i| i.track_id).collect()
    }
    /// Where the player should be now; `None` until synced.
    pub fn target(&self, latency_ms: i64) -> Option<LiveTarget> {
        self.0.target(latency_ms).map(|t| LiveTarget { item_id: t.item_id, track_id: t.track_id, position_ms: t.position_ms, playing: t.playing })
    }
    /// Drift correction for a player at `actual_ms` on the target track.
    pub fn correction(&self, latency_ms: i64, actual_ms: i64) -> LiveCorrection {
        match self.0.correction(latency_ms, actual_ms) {
            Correction::None => LiveCorrection::None,
            Correction::Rate(rate) => LiveCorrection::Rate { rate },
            Correction::Seek(position_ms) => LiveCorrection::Seek { position_ms },
        }
    }
    /// Low power mode: no rate nudging, seeks only.
    pub fn set_seek_only(&self, on: bool) {
        self.0.set_seek_only(on);
    }
    pub fn leave(&self) {
        self.0.leave();
    }
}

/// Validates an invitation without joining (QR scanner feedback).
#[uniffi::export]
pub fn live_is_invite(text: String) -> bool {
    tonearm_core::live::ticket::Ticket::parse(&text).is_ok()
}

/// One track by id (guests resolve the shared queue's ids with their own account).
#[uniffi::export]
pub async fn live_track(id: String) -> Result<crate::Track, FfiError> {
    let t = crate::run(move || tonearm_core::service::get_track(&id)).await?;
    Ok(crate::convert::tracks(std::slice::from_ref(&t)).remove(0))
}
