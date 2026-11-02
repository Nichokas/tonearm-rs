//! Port of `internal/player`: playbin3 wrapper, queues, controls, gapless
//! playback, `ReplayGain`, stream quality and MPRIS.


pub mod mpris;
pub use tonearm_core::playqueue;
pub mod quality;
pub mod replaygain;
pub mod statemachine;

use anyhow::{anyhow, Result};
use gst::prelude::*;
use std::cell::OnceCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use crate::model::{PlaybackSource, Track};
use crate::signals::{Signal, State, CONTINUE};
use crate::{api, global_signal, i18n::tr, service, settings};
use playqueue::{DurableQueue, HistoryQueue, Queue};
pub use quality::audio_stream_quality;
use statemachine::{Hooks, Machine, Trigger};
pub use statemachine::{
    playback_quality_changed, playback_state_changed, track_changed, PlaybackState, PlaybackStatus,
};

// ---------------------------------------------------------------- signals

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepeatMode {
    #[default]
    None,
    Queue,
    Track,
}

global_signal!(pub fn repeat_mode_changed() -> State<RepeatMode> = State::new(RepeatMode::None));
global_signal!(pub fn shuffle_state_changed() -> State<bool> = State::new(false));
global_signal!(pub fn seeked() -> Signal<Duration> = Signal::default());
global_signal!(pub fn source_changed() -> State<Option<PlaybackSource>> = State::new(None));
global_signal!(pub fn volume_changed() -> State<f64> = State::new(settings::volume()));
// Toast requests from the player (notifications.OnToast).
global_signal!(pub fn toasts() -> Signal<String> = Signal::default());

global_signal!(pub fn base_queue() -> DurableQueue = DurableQueue::default());
global_signal!(pub fn user_queue() -> Queue = Queue::default());
global_signal!(pub fn history() -> HistoryQueue = HistoryQueue::default());

// ---------------------------------------------------------------- player

struct Player {
    playbin: gst::Element,
    machine: Rc<Machine>,
    _bus_watch: gst::bus::BusWatchGuard,
}

thread_local! {
    static PLAYER: OnceCell<Player> = const { OnceCell::new() };
}

fn with<R>(f: impl FnOnce(&Player) -> R) -> Option<R> {
    PLAYER.with(|p| p.get().map(f))
}

fn fire(t: Trigger) {
    if with(|p| p.machine.fire(t)).is_none() {
        log::warn!("player not initialised, dropping trigger");
    }
}

fn playbin() -> Option<gst::Element> {
    with(|p| p.playbin.clone())
}

/// Initialises `GStreamer`, playbin3, the state machine and MPRIS.
/// Must be called once on the GTK main thread.
pub fn init() -> Result<()> {
    if with(|_| ()).is_some() {
        return Ok(());
    }
    gst::init()?;
    let playbin = gst::ElementFactory::make("playbin3").build().map_err(|e| anyhow!("playbin3 unavailable: {e}"))?;

    let pb: Rc<dyn statemachine::Playbin> = Rc::new(playbin.clone());
    let machine = Machine::new(
        pb,
        Hooks {
            next_track,
            on_eos_stopped: || Box::pin(on_eos_stopped()),
            clear_source: || source_changed().set(None),
        },
    );

    let bus = playbin.bus().ok_or_else(|| anyhow!("playbin has no bus"))?;
    let bus_watch = bus.add_watch_local(|_, msg| {
        on_bus_message(msg);
        glib::ControlFlow::Continue
    })?;

    playbin.connect_notify(Some("volume"), |pb, _| {
        let v: f64 = pb.property("volume");
        glib::MainContext::default().invoke(move || on_volume_change(v));
    });

    playbin.connect("about-to-finish", false, |_| {
        let (tx, rx) = mpsc::channel();
        let ctx = glib::MainContext::default();
        if ctx.is_owner() {
            fire(Trigger::GaplessHint(None));
            return None;
        }
        ctx.invoke(move || fire(Trigger::GaplessHint(Some(tx))));
        // Block the streaming thread until the next URI is set (gapless).
        if rx.recv_timeout(Duration::from_secs(15)).is_err() {
            log::warn!("timed out waiting for the next gapless track");
        }
        None
    });

    if let Some(bin) = playbin.downcast_ref::<gst::Bin>() {
        bin.connect_deep_element_added(|_, _, element| quality::on_deep_element_added(element));
    }

    replaygain::init(&playbin)?;
    playbin.set_property("volume", settings::volume());

    PLAYER.with(|p| {
        let _ = p.set(Player { playbin, machine, _bus_watch: bus_watch });
    });

    track_changed().on(|ev| {
        if let Some(ev) = ev
            && !ev.skip_history {
                history().prepend(ev.track.clone());
            }
        CONTINUE
    });
    source_changed().on(|_| {
        replaygain::update_album_mode();
        CONTINUE
    });
    enqueued_track_hook();
    mpris::init();
    Ok(())
}

fn enqueued_track_hook() {
    statemachine::enqueued_track().on(|m| {
        replaygain::set_enqueued(m.as_ref());
        CONTINUE
    });
}

fn on_bus_message(msg: &gst::Message) {
    use gst::MessageView;
    match msg.view() {
        MessageView::Error(err) => {
            let e = err.error();
            if e.matches(gst::ResourceError::NotAuthorized) || e.matches(gst::ResourceError::NotFound) {
                log::warn!("playback token expired, restarting playback session");
                fire(Trigger::TokenExpired);
                return;
            }
            log::error!("playback failed: {} (debug: {:?})", e, err.debug());
        }
        MessageView::Tag(t) => quality::on_tags_received(&t.tags()),
        MessageView::StreamStart(_) => fire(Trigger::StreamStart),
        MessageView::Eos(_) => fire(Trigger::Eos),
        _ => {}
    }
}

fn on_volume_change(v: f64) {
    #[expect(clippy::float_cmp, reason = "exact echo of a value we published ourselves")]
    let unchanged = v == volume_changed().get();
    if unchanged {
        return;
    }
    settings::set_volume(v);
    volume_changed().set(v);
}

async fn on_eos_stopped() {
    if list_pending() {
        // The loaded part ran out before the next page arrived: wait for it
        // instead of stopping, starting a radio or repeating.
        log::info!("queue ran dry while the list is still loading; waiting for the next page");
        WAITING.with(|w| w.set(true));
        feed(true);
        return;
    }
    let stop = || {
        source_changed().set(None);
        statemachine::on_stopped_stop();
    };
    if !settings::allow_autoplay() {
        stop();
        return;
    }
    let Some(id) = current_track_id() else {
        stop();
        return;
    };
    if let Err(e) = play_track_radio(&id, true).await {
        log::info!("failed to play track radio: {e:#}");
        stop();
    }
}

pub fn current_track_id() -> Option<String> {
    track_changed().with(|t| t.as_ref().map(|e| e.track.id().to_string()))
}

// ---------------------------------------------------------------- queue.go

fn clear_queues() {
    user_queue().clear();
    base_queue().clear();
}

fn get_next_track_from_queue(peek: bool) -> Option<Track> {
    let t = if peek { user_queue().peek() } else { user_queue().pop() };
    if t.is_some() {
        return t;
    }
    let t = if peek { base_queue().peek() } else { base_queue().pop() };
    if t.is_some() {
        return t;
    }
    if list_pending() {
        // More of the list is coming: don't wrap around (repeat) yet.
        return None;
    }
    if repeat_mode_changed().get() == RepeatMode::Queue {
        log::debug!("queue repeat mode is enabled, replaying base queue");
        base_queue().restore("");
        if shuffle_state_changed().get() {
            base_queue().shuffle("");
        }
        return base_queue().peek();
    }
    None
}

/// Returns the next streamable track and whether it is a track repeat.
fn next_track(peek: bool) -> (Option<Track>, bool) {
    let repeat = repeat_mode_changed().get();
    if repeat == RepeatMode::Track {
        return (track_changed().with(|t| t.as_ref().map(|e| e.track.clone())), true);
    }
    let mut next = get_next_track_from_queue(true);
    while matches!(&next, Some(t) if !t.info.streamable) {
        get_next_track_from_queue(false);
        next = get_next_track_from_queue(true);
    }
    if !peek {
        get_next_track_from_queue(false);
    }
    (next, false)
}

fn previous_track(peek: bool) -> Option<Track> {
    if repeat_mode_changed().get() == RepeatMode::Track
        && let Some(t) = track_changed().with(|t| t.as_ref().map(|e| e.track.clone())) {
            return Some(t);
        }
    if peek { history().peek() } else { history().pop() }
}

pub async fn add_track_to_user_queue(track_id: &str) -> Result<()> {
    let id = track_id.to_string();
    let track = api::run(move || service::get_track(&id)).await?;
    user_queue().append(track);
    if playback_state_changed().get().status == PlaybackStatus::Stopped {
        log::info!("no track is currently playing, immediately playing track {track_id}");
        next();
    }
    Ok(())
}

pub fn add_tracklist_to_user_queue(tracks: Vec<Track>) {
    let any = !tracks.is_empty();
    for t in tracks {
        user_queue().append(t);
    }
    if any && playback_state_changed().get().status == PlaybackStatus::Stopped {
        next();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueKind {
    User,
    Base,
}

/// Skips `to` entries of a queue (adding them to the history) and plays the next.
pub fn skip_through_queue(kind: QueueKind, to: usize) {
    let res = match kind {
        QueueKind::User => user_queue().skip(to),
        QueueKind::Base => base_queue().skip(to),
    };
    match res {
        Ok(skipped) => {
            for t in skipped {
                history().prepend(t);
            }
            let next = match kind {
                QueueKind::User => user_queue().pop(),
                QueueKind::Base => base_queue().pop(),
            };
            match next {
                Some(t) => fire(Trigger::PlayTrack(t, false)),
                None => log::error!("failed to start playback after skipping through queue"),
            }
        }
        Err(e) => log::error!("failed to skip through queue: {e}"),
    }
}

// ---------------------------------------------------------------- controls.go

pub fn cycle_repeat_mode() {
    set_repeat_mode(match repeat_mode_changed().get() {
        RepeatMode::None => RepeatMode::Queue,
        RepeatMode::Queue => RepeatMode::Track,
        RepeatMode::Track => RepeatMode::None,
    });
}

pub fn next() {
    match next_track(false).0 {
        Some(t) => fire(Trigger::PlayTrack(t, false)),
        None => log::error!("failed to get next track: queue is empty"),
    }
}

pub fn pause() {
    fire(Trigger::Pause);
}

pub fn play() {
    fire(Trigger::Resume);
}

pub fn play_pause() {
    match playback_state_changed().get().status {
        PlaybackStatus::Playing => pause(),
        PlaybackStatus::Paused | PlaybackStatus::Stopped => play(),
    }
}

pub fn previous() {
    let pos = playbin().and_then(|p| statemachine::Playbin::query_position(&p));
    if pos.is_some_and(|p| p > Duration::from_secs(5)) || history().peek().is_none() {
        seek_to_position(Duration::ZERO, false);
        return;
    }
    let Some(prev) = previous_track(false) else {
        log::error!("failed to get previous track");
        return;
    };
    if let Some(cur) = track_changed().with(|t| t.as_ref().map(|e| e.track.clone())) {
        user_queue().prepend(cur);
    }
    fire(Trigger::PlayTrack(prev, false));
}

pub fn seek_to_percent(percent: f64) {
    let percent = percent.clamp(0.0, 100.0);
    let d = playback_state_changed().get().duration;
    seek_to_position(d.mul_f64(percent / 100.0), false);
}

pub fn seek_to_position(position: Duration, precise: bool) {
    if let Some(p) = playbin() {
        let flags = if precise {
            gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE
        } else {
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT
        };
        statemachine::Playbin::seek(&p, position, flags);
    }
    seeked().notify(&position);
}

pub fn set_volume(volume: f64) {
    let volume = if volume < 0.0 { 0.0 } else { volume };
    if volume > 1.0 {
        log::warn!("Volume is higher than 1. This will cause overdrive to the speakers.");
    }
    if let Some(p) = playbin() {
        p.set_property("volume", volume);
    }
}

pub fn stop() {
    source_changed().set(None);
    fire(Trigger::Stop);
}

pub fn set_repeat_mode(m: RepeatMode) {
    if m != repeat_mode_changed().get() {
        repeat_mode_changed().set(m);
    }
}

pub fn set_shuffle(shuffle: bool, current_track_id: &str) {
    if shuffle {
        base_queue().shuffle(current_track_id);
    } else {
        base_queue().restore(current_track_id);
    }
    shuffle_state_changed().set(shuffle);
}

pub fn toggle_shuffle() {
    let cur = current_track_id().unwrap_or_default();
    set_shuffle(!base_queue().is_shuffled(), &cur);
}

// ---------------------------------------------------------------- playback.go

pub async fn play_track_id(track_id: &str) -> Result<()> {
    let id = track_id.to_string();
    let t = api::run(move || service::get_track(&id)).await?;
    play_track(t);
    Ok(())
}

pub fn play_track(track: Track) {
    bump_list_generation();
    clear_queues();
    stop();
    source_changed().set(Some(PlaybackSource::Track(Box::new(track.clone()))));
    fire(Trigger::PlayTrack(track, false));
}

#[expect(clippy::unused_async, reason = "callers await it like the other playback entry points; mirrors Go's PlayTracklist")]
pub async fn play_tracklist(source: PlaybackSource, tracks: Vec<Track>, shuffle: bool, position: usize) -> Result<()> {
    play_tracks(tracks, shuffle, position)?;
    source_changed().set(Some(source));
    Ok(())
}

pub async fn play_album(album_id: &str, shuffle: bool, position: usize) -> Result<()> {
    let id = album_id.to_string();
    let i = id.clone();
    let info = api::run(move || service::get_album_info(&i)).await?;
    let tracks = crate::cache::all_tracks(crate::cache::Job::Album(id)).await?;
    clear_queues();
    stop();
    play_tracks(tracks, shuffle, position)?;
    source_changed().set(Some(PlaybackSource::Album(info)));
    Ok(())
}

pub async fn play_artist_top_songs(artist_id: &str, shuffle: bool, position: usize) -> Result<()> {
    let id = artist_id.to_string();
    let (info, tracks) = api::run(move || {
        let info = service::get_artist_info(&id)?;
        let page = api::artist_page(&id)?;
        let mut tracks = vec![];
        if let Some(module) = page.items.iter().find(|i| i.module_id == "ARTIST_TOP_TRACKS") {
            for item in &module.items {
                if let api::legacy::ItemData::Track(t) = &item.data {
                    match service::get_track(&t.id.to_string()) {
                        Ok(track) => tracks.push(track),
                        Err(e) => log::error!("error while resolving Top Track item {}: {e}", t.id),
                    }
                }
            }
        }
        Ok::<_, anyhow::Error>((info, tracks))
    })
    .await?;
    clear_queues();
    stop();
    play_tracks(tracks, shuffle, position)?;
    source_changed().set(Some(PlaybackSource::Artist(info)));
    Ok(())
}

pub async fn play_playlist(playlist_id: &str, shuffle: bool, position: usize) -> Result<()> {
    let id = playlist_id.to_string();
    let i = id.clone();
    let info = api::run(move || service::get_playlist_info(&i)).await?;
    let tracks = crate::cache::all_tracks(crate::cache::Job::Playlist(id)).await?;
    clear_queues();
    stop();
    play_tracks(tracks, shuffle, position)?;
    source_changed().set(Some(PlaybackSource::Playlist(info)));
    Ok(())
}

pub async fn play_track_radio(track_id: &str, skip_self: bool) -> Result<()> {
    let id = track_id.to_string();
    let radio = api::run(move || service::get_track_radio(&id)).await.inspect_err(|e| log::error!("failed to retrieve mix: {e}"))?;
    let Some(mix) = radio else {
        toasts().notify(&tr("No mix found for the current track"));
        anyhow::bail!("no mix found for the current track");
    };
    play_playlist(&mix.id, false, usize::from(skip_self)).await
}

thread_local! {
    /// Bumped whenever the base queue is replaced, so background fetches for
    /// an older list can tell they are stale.
    static LIST_GENERATION: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Plays `tracks` as the queue starting at the clicked row; earlier rows go to
/// the history so "previous" walks back through the list. Returns the queue
/// generation for [`extend_list`], or `None` if nothing could be played.
pub fn play_list_from(source: PlaybackSource, tracks: Vec<Track>, clicked: &Track, pos: usize) -> Option<u64> {
    let start = crate::model::queue_start(&tracks, clicked.id(), pos);
    match play_tracks(tracks, false, start) {
        Ok(_) => {
            source_changed().set(Some(source));
            Some(LIST_GENERATION.with(std::cell::Cell::get))
        }
        Err(e) => {
            toasts().notify(&tr("An error occurred while playing the track"));
            log::error!("playing the track list failed error={e:#}");
            None
        }
    }
}

/// Fetches the next page of the playing list; gets a callback to call with
/// `true` when more may follow, `false` at the end or on error.
pub type ListFeeder = Rc<dyn Fn(Rc<dyn Fn(bool)>)>;

/// Upcoming tracks left in the queue at which the next page is requested.
pub const FEED_THRESHOLD: usize = 10;

thread_local! {
    static FEEDER: std::cell::RefCell<Option<(u64, ListFeeder)>> = const { std::cell::RefCell::new(None) };
    static FEEDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Playback reached the end of the loaded tracks and waits for a page.
    static WAITING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

// Tracks of the playing list that are not loaded yet (known from the list's
// total size); the queue view shows them as placeholder rows.
global_signal!(pub fn list_unloaded() -> State<usize> = State::new(0));

thread_local! {
    static LIST_TOTAL: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// How many of `total` list tracks are still to load once `loaded` are known.
pub fn unloaded_count(total: Option<usize>, loaded: usize, pending: bool) -> usize {
    match total {
        Some(t) if pending => t.saturating_sub(loaded),
        _ => 0,
    }
}

fn update_unloaded() {
    let n = unloaded_count(LIST_TOTAL.with(std::cell::Cell::get), base_queue().source_len(), list_pending());
    if list_unloaded().get() != n {
        list_unloaded().set(n);
    }
}

/// Records the full size of the list started at `generation` (e.g. the
/// number of favourite tracks) so the queue can show all of it.
pub fn set_list_total(generation: u64, total: usize) {
    if LIST_GENERATION.with(std::cell::Cell::get) == generation {
        LIST_TOTAL.with(|t| t.set(Some(total)));
        update_unloaded();
    }
}

/// Whether the playing list has pages that are not loaded yet.
fn list_pending() -> bool {
    let generation = LIST_GENERATION.with(std::cell::Cell::get);
    FEEDER.with(|f| f.borrow().as_ref().is_some_and(|(g, _)| *g == generation))
}

/// Whether to ask for another page: only when few tracks remain upcoming.
pub fn needs_more(upcoming: usize) -> bool {
    upcoming <= FEED_THRESHOLD
}

/// Lets the list started at `generation` grow lazily: `feeder` is called
/// (one request at a time) whenever the queue runs low.
pub fn set_list_feeder(generation: u64, feeder: ListFeeder) {
    static HOOKED: std::sync::Once = std::sync::Once::new();
    HOOKED.call_once(|| {
        base_queue().entries().on(|v| {
            if needs_more(v.len()) {
                glib::idle_add_local_once(maybe_feed);
            }
            CONTINUE
        });
    });
    if LIST_GENERATION.with(std::cell::Cell::get) != generation {
        return;
    }
    FEEDER.with(|f| *f.borrow_mut() = Some((generation, feeder)));
    maybe_feed();
}

fn maybe_feed() {
    feed(false);
}

/// Requests the next page (one at a time); `force` ignores the threshold
/// (playback is waiting, or the queue view shows unloaded rows).
pub fn feed(force: bool) {
    let generation = LIST_GENERATION.with(std::cell::Cell::get);
    let Some(feeder) = FEEDER.with(|f| f.borrow().as_ref().filter(|(g, _)| *g == generation).map(|(_, f)| Rc::clone(f))) else { return };
    if FEEDING.with(std::cell::Cell::get) || !(force || needs_more(base_queue().entries().with(Vec::len))) {
        return;
    }
    FEEDING.with(|f| f.set(true));
    feeder(Rc::new(move |more| {
        FEEDING.with(|f| f.set(false));
        if !more {
            FEEDER.with(|f| {
                if f.borrow().as_ref().is_some_and(|(g, _)| *g == generation) {
                    *f.borrow_mut() = None;
                }
            });
        }
        update_unloaded();
        if LIST_GENERATION.with(std::cell::Cell::get) != generation || !WAITING.with(std::cell::Cell::get) {
            return;
        }
        if !base_queue().is_empty() {
            WAITING.with(|w| w.set(false));
            next();
        } else if more {
            feed(true); // page was empty or all duplicates: keep going
        } else {
            // The list ended (or failed) while waiting: finish like a normal end.
            WAITING.with(|w| w.set(false));
            glib::spawn_future_local(on_eos_stopped());
        }
    }));
}

/// Appends late-loaded tracks to the list started by [`play_list_from`].
/// Returns `false` (and does nothing) once another list replaced it.
pub fn extend_list(generation: u64, tracks: &[Track]) -> bool {
    if LIST_GENERATION.with(std::cell::Cell::get) != generation {
        return false;
    }
    base_queue().extend(tracks);
    update_unloaded();
    true
}

fn bump_list_generation() {
    LIST_GENERATION.with(|g| g.set(g.get() + 1));
    FEEDER.with(|f| *f.borrow_mut() = None);
    FEEDING.with(|f| f.set(false));
    WAITING.with(|w| w.set(false));
    LIST_TOTAL.with(|t| t.set(None));
    update_unloaded();
}

fn play_tracks(tracks: Vec<Track>, shuffle: bool, starting_position: usize) -> Result<Track> {
    bump_list_generation();
    clear_queues();
    stop();
    base_queue().set(tracks);
    set_shuffle(shuffle, "");
    if starting_position > 0 {
        match base_queue().skip(starting_position) {
            Ok(skipped) => skipped.into_iter().for_each(|t| history().prepend(t)),
            Err(e) => log::warn!("could not skip to position {starting_position}: {e:#}"),
        }
    }
    let Some(t) = next_track(false).0 else {
        anyhow::bail!("tracklist produced no valid tracks to be played");
    };
    fire(Trigger::PlayTrack(t.clone(), false));
    Ok(t)
}

// ---------------------------------------------------------------- listen together

/// Current playback position as reported by `GStreamer`.
pub fn position() -> Option<Duration> {
    playbin().and_then(|p| statemachine::Playbin::query_position(&p))
}

/// Changes the playback rate without moving the position (Listen Together
/// drift nudges). Tries an instant rate change first and falls back to a
/// flushing seek; returns false when the pipeline refuses both.
pub fn set_rate(rate: f64) -> bool {
    let Some(p) = playbin() else { return false };
    let instant = p.seek(rate, gst::SeekFlags::INSTANT_RATE_CHANGE, gst::SeekType::None, gst::ClockTime::NONE, gst::SeekType::None, gst::ClockTime::NONE);
    if instant.is_ok() {
        return true;
    }
    let Some(pos) = statemachine::Playbin::query_position(&p) else { return false };
    let pos = gst::ClockTime::from_nseconds(u64::try_from(pos.as_nanos()).unwrap_or(u64::MAX));
    p.seek(rate, gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, gst::SeekType::Set, pos, gst::SeekType::None, gst::ClockTime::NONE).is_ok()
}

#[cfg(test)]
mod feed_tests {
    #[test]
    fn unloaded_rows_cover_the_rest_of_the_list() {
        assert_eq!(super::unloaded_count(Some(3000), 50, true), 2950);
        assert_eq!(super::unloaded_count(Some(10), 50, true), 0);
        // fully loaded or unknown size: no placeholders
        assert_eq!(super::unloaded_count(Some(3000), 50, false), 0);
        assert_eq!(super::unloaded_count(None, 50, true), 0);
    }

    #[test]
    fn asks_for_more_only_near_the_end_of_the_window() {
        assert!(super::needs_more(0));
        assert!(super::needs_more(super::FEED_THRESHOLD));
        assert!(!super::needs_more(super::FEED_THRESHOLD + 1));
    }
}

