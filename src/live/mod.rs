//! Listen Together on the desktop: glue between the shared core
//! (`tonearm_core::live`, iroh transport) and the `GStreamer` player.
//!
//! The host publishes its queue and playback anchors; guests load the host's
//! track through their own TIDAL session and follow the anchors with the
//! core's drift policy (ignore < 30 ms, ±2 % rate up to 300 ms, seek above).
//! Only metadata travels.

pub mod link;
pub mod state;

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gtk::gio;
use gtk::gio::prelude::*;
use tonearm_core::live::drift::{Correction, SEEK_MS};
use tonearm_core::live::net::{LiveEvent, LiveGuest, LiveHost, LiveListener, NetConfig};
use tonearm_core::live::queue::QueueItem;
use tonearm_core::live::MAX_QUEUE;

use crate::i18n::tr;
use crate::player::{self, PlaybackStatus};
use crate::signals::{State, CONTINUE};
use crate::{global_signal, settings};
pub use state::{Ended, Phase};

global_signal!(pub fn phase() -> State<Phase> = State::new(Phase::Idle));

enum Session {
    Host(Arc<LiveHost>),
    Guest(Arc<LiveGuest>),
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    /// Bumped for every new or ended session; stale events are dropped.
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    /// Host: entry id of the current track in the shared queue.
    static CURRENT_ITEM: RefCell<Option<String>> = const { RefCell::new(None) };
    /// Host: last published (status, loading).
    static LAST_STATUS: Cell<Option<(PlaybackStatus, bool)>> = const { Cell::new(None) };
    /// Guest: track loads started to follow the host.
    static LOADS: RefCell<LoadGate> = RefCell::new(LoadGate::default());
    static HOOKED: Cell<bool> = const { Cell::new(false) };
    static OUTPUT_DELAY: Cell<i32> = const { Cell::new(0) };
}

const OUTPUT_DELAY_KEY: &str = "live-output-delay";
const GUEST_TICK: Duration = Duration::from_millis(250);
const HOST_REFRESH: Duration = Duration::from_secs(5);

fn current_generation() -> u64 {
    GENERATION.with(Cell::get)
}

fn next_generation() -> u64 {
    GENERATION.with(|g| {
        g.set(g.get() + 1);
        g.get()
    })
}

fn host() -> Option<Arc<LiveHost>> {
    SESSION.with(|s| match &*s.borrow() {
        Some(Session::Host(h)) => Some(Arc::clone(h)),
        _ => None,
    })
}

fn guest() -> Option<Arc<LiveGuest>> {
    SESSION.with(|s| match &*s.borrow() {
        Some(Session::Guest(g)) => Some(Arc::clone(g)),
        _ => None,
    })
}

// ---------------------------------------------------------------- settings

fn has_delay_key() -> bool {
    settings::player().settings_schema().is_some_and(|s| s.has_key(OUTPUT_DELAY_KEY))
}

/// Extra delay (ms) a guest adds for its audio output (Bluetooth etc.).
/// Falls back to an in-memory value when an older schema is installed.
pub fn output_delay_ms() -> i32 {
    if has_delay_key() { settings::player().int(OUTPUT_DELAY_KEY) } else { OUTPUT_DELAY.with(Cell::get) }
}

pub fn set_output_delay_ms(ms: i32) {
    if has_delay_key() {
        if let Err(e) = settings::player().set_int(OUTPUT_DELAY_KEY, ms) {
            log::warn!("failed to store the Listen Together output delay error={e}");
        }
    } else {
        OUTPUT_DELAY.with(|d| d.set(ms));
    }
}

// ---------------------------------------------------------------- events

#[derive(Debug)]
struct Listener {
    generation: u64,
    tx: async_channel::Sender<(u64, LiveEvent)>,
}

impl LiveListener for Listener {
    fn on_event(&self, ev: LiveEvent) {
        // Called from the transport's threads; hand over to the main loop.
        drop(self.tx.try_send((self.generation, ev)));
    }
}

fn listener(generation: u64) -> Arc<dyn LiveListener> {
    let (tx, rx) = async_channel::unbounded();
    glib::spawn_future_local(async move {
        while let Ok((g, ev)) = rx.recv().await {
            if g == current_generation() {
                on_event(&ev);
            }
        }
    });
    Arc::new(Listener { generation, tx })
}

fn on_event(ev: &LiveEvent) {
    let current = phase().get();
    if let Some(next) = state::apply_event(&current, ev) {
        if let Phase::Ended(e) = &next {
            player::toasts().notify(&state::ended_message(e));
            teardown();
        } else if next == Phase::Idle {
            teardown();
        }
        phase().set(next);
    }
    if guest().is_some() {
        guest_tick();
    }
}

fn display_name() -> String {
    let real = glib::real_name().to_string_lossy().trim().to_string();
    if real.is_empty() || real == "Unknown" { glib::user_name().to_string_lossy().to_string() } else { real }
}

// ---------------------------------------------------------------- lifecycle

/// Host: opens a session and starts publishing the local player.
pub fn start_host() {
    if phase().get().is_active() {
        return;
    }
    let generation = next_generation();
    phase().set(Phase::Starting);
    let l = listener(generation);
    let name = display_name();
    glib::spawn_future_local(async move {
        let r = gio::spawn_blocking(move || LiveHost::start(name, &NetConfig::default(), l)).await;
        if generation != current_generation() {
            if let Ok(Ok(h)) = r {
                drop(gio::spawn_blocking(move || h.end()));
            }
            return;
        }
        match r {
            Ok(Ok(h)) => {
                let h = Arc::new(h);
                let url = h.ticket_url();
                SESSION.with(|s| *s.borrow_mut() = Some(Session::Host(h)));
                install_hooks();
                phase().set(Phase::Hosting { url, guests: Vec::new() });
                publish_queue();
                start_host_refresh(generation);
            }
            Ok(Err(e)) => fail(&format!("{e}")),
            Err(_) => fail("host start panicked"),
        }
    });
}

/// Guest: joins from an `sdat://join/<ticket>` link.
pub fn join(url: &str) {
    match phase().get() {
        p if p.is_host() => {
            player::toasts().notify(&tr("End your session before joining another one"));
            return;
        }
        p if p.is_active() => end(),
        _ => {}
    }
    let generation = next_generation();
    phase().set(Phase::Joining);
    match LiveGuest::join(url, display_name(), &NetConfig::default(), listener(generation)) {
        Ok(g) => {
            SESSION.with(|s| *s.borrow_mut() = Some(Session::Guest(Arc::new(g))));
            install_hooks();
            start_guest_tick(generation);
        }
        Err(e) => fail(&format!("{e}")),
    }
}

fn fail(err: &str) {
    log::error!("Listen Together failed error={err}");
    teardown();
    let e = Ended::Failed;
    player::toasts().notify(&state::ended_message(&e));
    phase().set(Phase::Ended(e));
}

/// Ends (host) or leaves (guest) the current session.
pub fn end() {
    match SESSION.with(|s| s.borrow_mut().take()) {
        Some(Session::Host(h)) => drop(gio::spawn_blocking(move || h.end())),
        Some(Session::Guest(g)) => g.leave(),
        None => {}
    }
    teardown();
    phase().set(Phase::Idle);
}

/// Clears an "ended" message.
pub fn dismiss() {
    if matches!(phase().get(), Phase::Ended(_)) {
        phase().set(Phase::Idle);
    }
}

/// Stops everything tied to the current session; bumping the generation
/// also stops its timers and drops its late events.
fn teardown() {
    next_generation();
    let was_guest = matches!(SESSION.with(|s| s.borrow_mut().take()), Some(Session::Guest(_))) || phase().get().is_guest();
    CURRENT_ITEM.with(|c| c.borrow_mut().take());
    drop(LOADS.with(RefCell::take));
    LAST_STATUS.with(|l| l.set(None));
    if was_guest {
        player::set_rate(1.0);
    }
}

// ---------------------------------------------------------------- host

/// Track ids shared with guests: current track first, then the user queue
/// and the rest of the playing list. Ids outside the protocol's rule
/// (≤ 40 bytes, `[A-Za-z0-9-_:.]`) are left out, and the list is capped.
pub fn shared_track_ids(current: Option<&str>, user: &[String], base: &[String]) -> Vec<String> {
    current
        .into_iter()
        .map(str::to_string)
        .chain(user.iter().cloned())
        .chain(base.iter().cloned())
        .filter(|id| QueueItem { item_id: "x".into(), track_id: id.clone() }.is_valid())
        .take(MAX_QUEUE)
        .collect()
}

fn ids(tracks: &[crate::model::Track]) -> Vec<String> {
    tracks.iter().map(|t| t.id().to_string()).collect()
}

fn publish_queue() {
    let Some(h) = host() else { return };
    let current = player::current_track_id();
    let user = player::user_queue().entries().with(|t| ids(t));
    let base = player::base_queue().entries().with(|t| ids(t));
    let shared = shared_track_ids(current.as_deref(), &user, &base);
    let current_valid = current.as_ref().is_some_and(|c| shared.first() == Some(c));
    match h.replace_queue(shared) {
        Ok(item_ids) => CURRENT_ITEM.with(|c| *c.borrow_mut() = if current_valid { item_ids.into_iter().next() } else { None }),
        Err(e) => log::warn!("failed to share the queue error={e}"),
    }
    publish_playback();
}

fn publish_playback() {
    let Some(h) = host() else { return };
    let ps = player::playback_state_changed().get();
    LAST_STATUS.with(|l| l.set(Some((ps.status, ps.loading))));
    let pos = player::position().unwrap_or(ps.position);
    let playing = ps.status == PlaybackStatus::Playing && !ps.loading;
    let item = CURRENT_ITEM.with(|c| c.borrow().clone());
    let track = if item.is_some() { player::current_track_id() } else { None };
    h.set_playback(item, track, i64::try_from(pos.as_millis()).unwrap_or(i64::MAX), playing);
}

fn start_host_refresh(generation: u64) {
    glib::timeout_add_local(HOST_REFRESH, move || {
        if generation != current_generation() {
            return glib::ControlFlow::Break;
        }
        publish_playback();
        glib::ControlFlow::Continue
    });
}

/// Player subscriptions, installed once; they only act while hosting.
fn install_hooks() {
    if HOOKED.with(|h| h.replace(true)) {
        return;
    }
    player::track_changed().on(|_| {
        publish_queue();
        CONTINUE
    });
    player::user_queue().entries().on(|_| {
        publish_queue();
        CONTINUE
    });
    player::base_queue().entries().on(|_| {
        publish_queue();
        CONTINUE
    });
    player::playback_state_changed().on(|ps| {
        // Position updates fire often; only status changes are anchors.
        if host().is_some() && LAST_STATUS.with(Cell::get) != Some((ps.status, ps.loading)) {
            publish_playback();
        }
        CONTINUE
    });
    player::seeked().on(|_| {
        publish_playback();
        CONTINUE
    });
}

// ---------------------------------------------------------------- guest

fn start_guest_tick(generation: u64) {
    glib::timeout_add_local(GUEST_TICK, move || {
        if generation != current_generation() {
            return glib::ControlFlow::Break;
        }
        guest_tick();
        glib::ControlFlow::Continue
    });
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// While paused, guests only seek when they are clearly elsewhere.
pub fn paused_needs_seek(target_ms: i64, actual_ms: i64) -> bool {
    (target_ms - actual_ms).abs() > SEEK_MS
}

/// How long a track the guest's account could not play is left alone.
const RETRY_FAILED: Duration = Duration::from_secs(15);

/// Guest: which host tracks are being loaded or failed recently, so a tick
/// never starts a second load of the same track and an unavailable track is
/// not retried (and toasted) every tick.
#[derive(Debug, Default)]
pub struct LoadGate {
    pending: Option<String>,
    failed: Option<(String, Instant)>,
}

impl LoadGate {
    /// Whether a load of `track` should start now.
    pub fn begin(&mut self, track: &str, now: Instant) -> bool {
        if self.pending.as_deref() == Some(track) {
            return false;
        }
        if let Some((t, at)) = &self.failed
            && t == track
            && now.duration_since(*at) < RETRY_FAILED
        {
            return false;
        }
        self.pending = Some(track.to_string());
        true
    }
    /// The player now has the host's track.
    pub fn loaded(&mut self) {
        self.pending = None;
        self.failed = None;
    }
    /// A load ended without playing (failed or superseded).
    /// Returns true on the first failure of this track (worth a toast).
    pub fn failed(&mut self, track: &str, now: Instant) -> bool {
        if self.pending.as_deref() == Some(track) {
            self.pending = None;
        }
        let first = self.failed.as_ref().is_none_or(|(t, _)| t != track);
        self.failed = Some((track.to_string(), now));
        first
    }
    /// A load was dropped because the host moved on.
    pub fn abandon(&mut self, track: &str) {
        if self.pending.as_deref() == Some(track) {
            self.pending = None;
        }
    }
}

/// Fetches the host's track with the guest's own TIDAL session, then plays it
/// only if the same session still wants it, honouring the host's pause.
fn load_host_track(track: String) {
    let generation = current_generation();
    glib::spawn_future_local(async move {
        let id = track.clone();
        let r = crate::api::run(move || crate::service::get_track(&id)).await;
        if generation != current_generation() {
            return; // session ended or changed meanwhile; teardown reset the gate
        }
        let latency = i64::from(output_delay_ms());
        let want = guest().and_then(|g| g.target(latency));
        if want.as_ref().and_then(|t| t.track_id.as_deref()) != Some(track.as_str()) {
            LOADS.with(|l| l.borrow_mut().abandon(&track));
            return;
        }
        match r {
            Ok(t) => {
                player::play_track(t);
                if want.is_some_and(|t| !t.playing) {
                    player::pause();
                }
            }
            Err(e) => {
                log::error!("failed to load the host's track error={e:#}");
                if LOADS.with(|l| l.borrow_mut().failed(&track, Instant::now())) {
                    player::toasts().notify(&tr("Could not play the host's track with your account"));
                }
            }
        }
    });
}

/// Moves the local player towards the host's anchor.
fn guest_tick() {
    let Some(g) = guest() else { return };
    let latency = i64::from(output_delay_ms());
    let Some(t) = g.target(latency) else { return };
    let ps = player::playback_state_changed().get();
    let local_playing = ps.status == PlaybackStatus::Playing;
    let Some(track) = t.track_id else {
        if local_playing {
            player::pause();
        }
        return;
    };
    if player::current_track_id().as_deref() != Some(track.as_str()) {
        if LOADS.with(|l| l.borrow_mut().begin(&track, Instant::now())) {
            load_host_track(track);
        }
        return;
    }
    LOADS.with(|l| l.borrow_mut().loaded());
    if ps.loading {
        return;
    }
    if t.playing != local_playing {
        if t.playing { player::play() } else { player::pause() }
        return;
    }
    let Some(actual) = player::position().map(ms) else { return };
    if !t.playing {
        if paused_needs_seek(t.position_ms, actual) {
            player::seek_to_position(Duration::from_millis(u64::try_from(t.position_ms.max(0)).unwrap_or(0)), true);
        }
        return;
    }
    match g.correction(latency, actual) {
        Correction::None => {}
        Correction::Rate(r) => {
            if !player::set_rate(r) {
                log::warn!("player refused a rate change, falling back to seeks");
                g.set_seek_only(true);
            }
        }
        Correction::Seek(to) => player::seek_to_position(Duration::from_millis(u64::try_from(to.max(0)).unwrap_or(0)), true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(ids: &[&str]) -> Vec<String> {
        ids.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn current_track_goes_first() {
        assert_eq!(shared_track_ids(Some("1"), &v(&["2"]), &v(&["3", "4"])), v(&["1", "2", "3", "4"]));
        assert_eq!(shared_track_ids(None, &[], &v(&["3"])), v(&["3"]));
    }

    #[test]
    fn invalid_ids_are_skipped() {
        let long = "9".repeat(41);
        assert_eq!(shared_track_ids(Some("a b"), &v(&[&long, "ok-1"]), &v(&["é"])), v(&["ok-1"]));
    }

    #[test]
    fn queue_is_capped() {
        let base: Vec<String> = (0..MAX_QUEUE + 10).map(|i| i.to_string()).collect();
        assert_eq!(shared_track_ids(Some("x"), &[], &base).len(), MAX_QUEUE);
    }

    #[test]
    fn load_gate_starts_each_track_once() {
        let now = Instant::now();
        let mut g = LoadGate::default();
        assert!(g.begin("1", now));
        assert!(!g.begin("1", now));
        assert!(g.begin("2", now), "a new host track starts its own load");
        g.loaded();
        assert!(g.begin("2", now));
    }

    #[test]
    fn load_gate_backs_off_failed_tracks() {
        let now = Instant::now();
        let mut g = LoadGate::default();
        assert!(g.begin("1", now));
        assert!(g.failed("1", now), "first failure is reported");
        assert!(!g.begin("1", now + Duration::from_secs(1)));
        assert!(g.begin("1", now + RETRY_FAILED));
        assert!(!g.failed("1", now + RETRY_FAILED), "repeated failure is not reported again");
        assert!(g.begin("2", now), "other tracks are not blocked");
    }

    #[test]
    fn abandoned_load_can_restart() {
        let now = Instant::now();
        let mut g = LoadGate::default();
        assert!(g.begin("1", now));
        g.abandon("1");
        assert!(g.begin("1", now));
    }

    #[test]
    fn paused_seek_threshold() {
        assert!(!paused_needs_seek(10_000, 10_000 - SEEK_MS));
        assert!(paused_needs_seek(10_000, 10_000 - SEEK_MS - 1));
        assert!(paused_needs_seek(0, 5_000));
    }
}
