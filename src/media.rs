//! Now-playing info from Windows' media controls (the same thing behind the volume
//! flyout and media keys). Only the app named in APP_FILTER is followed, so a video in
//! the browser doesn't take over the music widget.
//!
//! Two background threads:
//! - the **player thread** reads the song, play state and position, and sends button
//!   presses. It never waits on the network, so it keeps up even with rapid clicking.
//!   Updates come from Windows' change notifications (instant) plus a safety-net check
//!   (every second; every ~80 ms for a couple of seconds after a click).
//! - the **cover thread** downloads, decodes and scales album art. Covers are cached by
//!   song, only the newest requested cover is fetched during a burst of skips, and a cover
//!   identical to the previous song's (Windows sometimes still serves the old one right
//!   after a change) is treated as "not ready yet" and fetched again.

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Foundation::TypedEventHandler;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session,
    GlobalSystemMediaTransportControlsSessionManager as SessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};
use windows::Storage::Streams::{DataReader, IRandomAccessStreamReference};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};
use windows::core::AgileReference;

/// Only follow media sessions whose app id contains this (case-insensitive).
pub const APP_FILTER: &str = "spotifast";

/// Posted to the music window when the track, play state, position or album art changes.
pub const WM_MUSIC_UPDATED: u32 = WM_APP + 2;

/// How long the app must be missing before we treat it as closed (one failed read isn't enough).
const GONE_AFTER: Duration = Duration::from_secs(3);
/// After a single skip, how long to hold the new song back waiting for its cover (a late
/// cover crossfades in anyway; during rapid skipping there's no wait at all).
const ART_WAIT: Duration = Duration::from_millis(300);
/// After a button press, check quickly for this long.
const FAST_POLL_FOR: Duration = Duration::from_secs(2);
/// A cover identical to the previous song's is accepted after this long (same album).
const SAME_COVER_AFTER: Duration = Duration::from_millis(1500);
/// Stop retrying a song's cover after this long.
const COVER_GIVE_UP: Duration = Duration::from_secs(8);
/// How many covers to remember (going back to a recent song shows its cover instantly).
const COVER_CACHE: usize = 16;

#[derive(Clone)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub playing: bool,
    /// Position and length in seconds; `position` was true at `sampled`.
    pub position: f64,
    pub duration: f64,
    pub sampled: Instant,
    pub art: Option<Arc<image::RgbaImage>>,
    /// False once the app has gone away: we keep showing the last song, dimmed.
    pub present: bool,
}

impl Track {
    /// Current position, moving forward on its own while playing.
    pub fn position_now(&self) -> f64 {
        let p = if self.playing { self.position + self.sampled.elapsed().as_secs_f64() } else { self.position };
        if self.duration > 0.0 { p.clamp(0.0, self.duration) } else { p.max(0.0) }
    }
}

pub enum Command {
    PlayPause,
    Next,
    Previous,
    /// Jump to this many seconds into the song.
    Seek(f64),
    /// Something changed (a Windows notification, or a cover finished loading): read again now.
    Refresh,
}

type SongKey = (String, String);

static NOW: Mutex<Option<Track>> = Mutex::new(None);
static COMMANDS: OnceLock<Sender<Command>> = OnceLock::new();
/// The size (in pixels) the widget shows album art at; covers are scaled to it on the cover
/// thread, so the widget never has to shrink a big image mid-animation.
static ART_SIZE: AtomicU32 = AtomicU32::new(0);

pub fn set_art_size(pixels: u32) {
    ART_SIZE.store(pixels, Ordering::Relaxed);
}

/// The latest track (or the last one seen, if the app has closed).
pub fn current() -> Option<Track> {
    NOW.lock().unwrap().clone()
}

pub fn send(cmd: Command) {
    if let Some(tx) = COMMANDS.get() {
        let _ = tx.send(cmd);
    }
}

/// Starts the background threads that follow the player and notify `hwnd` on changes.
pub fn start(hwnd: HWND) {
    let (tx, rx) = channel();
    let _ = COMMANDS.set(tx.clone());
    let (cover_tx, cover_rx) = channel();
    let tx_for_covers = tx.clone();
    std::thread::spawn(move || cover_thread(cover_rx, tx_for_covers));
    let hwnd_raw = hwnd.0 as usize; // HWND isn't Send; pass the raw value
    std::thread::spawn(move || player_thread(rx, tx, cover_tx, hwnd_raw));
}

// ---------- covers ----------

struct Cover {
    image: Arc<image::RgbaImage>,
    fingerprint: u64,
}

/// Recently loaded covers, by song.
#[derive(Default)]
struct CoverCache {
    covers: HashMap<SongKey, Cover>,
    order: VecDeque<SongKey>,
}

impl CoverCache {
    fn insert(&mut self, key: SongKey, cover: Cover) {
        if self.covers.insert(key.clone(), cover).is_none() {
            self.order.push_back(key);
            while self.order.len() > COVER_CACHE {
                if let Some(old) = self.order.pop_front() {
                    self.covers.remove(&old);
                }
            }
        }
    }
}

static COVERS: Mutex<Option<CoverCache>> = Mutex::new(None);

fn cached_cover(key: &SongKey) -> Option<(Arc<image::RgbaImage>, u64)> {
    COVERS.lock().unwrap().as_ref()?.covers.get(key).map(|c| (c.image.clone(), c.fingerprint))
}

struct CoverRequest {
    key: SongKey,
    /// The cover's stream, wrapped so it can be handed to the cover thread.
    thumbnail: AgileReference<IRandomAccessStreamReference>,
    /// Fingerprint of the previous song's cover: an identical download means "not updated yet".
    previous_fingerprint: u64,
    /// Accept a cover identical to the previous song's (enough time has passed: same album).
    accept_same: bool,
}

/// Loads covers off the player thread. Only the newest request is handled; anything older
/// was for a song that has already been skipped past.
fn cover_thread(rx: Receiver<CoverRequest>, player: Sender<Command>) {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
    }
    while let Ok(mut request) = rx.recv() {
        while let Ok(newer) = rx.try_recv() {
            request = newer;
        }
        if cached_cover(&request.key).is_some() {
            continue;
        }
        let Ok(thumbnail) = request.thumbnail.resolve() else { continue };
        let Some(bytes) = read_bytes(&thumbnail) else { continue };
        let fingerprint = fingerprint(&bytes);
        if fingerprint == request.previous_fingerprint && !request.accept_same {
            continue; // still the old song's cover; the player thread will ask again shortly
        }
        let Some(image) = prepare_cover(&bytes) else { continue };
        COVERS
            .lock()
            .unwrap()
            .get_or_insert_with(CoverCache::default)
            .insert(request.key, Cover { image: Arc::new(image), fingerprint });
        let _ = player.send(Command::Refresh);
    }
}

fn read_bytes(thumbnail: &IRandomAccessStreamReference) -> Option<Vec<u8>> {
    let stream = thumbnail.OpenReadAsync().ok()?.join().ok()?;
    let size = stream.Size().ok()? as u32;
    if size == 0 {
        return None;
    }
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0).ok()?).ok()?;
    reader.LoadAsync(size).ok()?.join().ok()?;
    let mut bytes = vec![0u8; size as usize];
    reader.ReadBytes(&mut bytes).ok()?;
    Some(bytes)
}

fn fingerprint(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h = (h ^ *b as u64).wrapping_mul(0x100000001b3);
    }
    h
}

/// Centre-crops a cover to a square and scales it to the widget's art size.
fn prepare_cover(bytes: &[u8]) -> Option<image::RgbaImage> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let side = img.width().min(img.height());
    let square =
        image::imageops::crop_imm(&img, (img.width() - side) / 2, (img.height() - side) / 2, side, side).to_image();
    let target = ART_SIZE.load(Ordering::Relaxed);
    if target == 0 || target >= side {
        return Some(square);
    }
    Some(image::imageops::resize(&square, target, target, image::imageops::FilterType::Triangle))
}

// ---------- player ----------

/// Change notifications registered on one session, so they can be removed when it goes away.
struct Subscription {
    session: Session,
    playback: i64,
    properties: i64,
    timeline: i64,
}

impl Subscription {
    fn new(session: &Session, tx: &Sender<Command>) -> Option<Subscription> {
        let (t1, t2, t3) = (tx.clone(), tx.clone(), tx.clone());
        Some(Subscription {
            session: session.clone(),
            playback: session
                .PlaybackInfoChanged(&TypedEventHandler::new(move |_, _| {
                    let _ = t1.send(Command::Refresh);
                    Ok(())
                }))
                .ok()?,
            properties: session
                .MediaPropertiesChanged(&TypedEventHandler::new(move |_, _| {
                    let _ = t2.send(Command::Refresh);
                    Ok(())
                }))
                .ok()?,
            timeline: session
                .TimelinePropertiesChanged(&TypedEventHandler::new(move |_, _| {
                    let _ = t3.send(Command::Refresh);
                    Ok(())
                }))
                .ok()?,
        })
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.session.RemovePlaybackInfoChanged(self.playback);
        let _ = self.session.RemoveMediaPropertiesChanged(self.properties);
        let _ = self.session.RemoveTimelinePropertiesChanged(self.timeline);
    }
}

/// What the player thread remembers about the song it's currently seeing.
#[derive(Default)]
struct SongState {
    key: SongKey,
    /// When this song was first seen.
    since: Option<Instant>,
    /// Fingerprint of the previous song's cover.
    previous_fingerprint: u64,
    /// Fingerprint of this song's cover once loaded.
    fingerprint: u64,
    last_request: Option<Instant>,
    /// When recent song changes happened (to spot rapid skipping).
    changes: Vec<Instant>,
}

fn player_thread(rx: Receiver<Command>, tx: Sender<Command>, covers: Sender<CoverRequest>, hwnd_raw: usize) {
    let manager = loop {
        match SessionManager::RequestAsync().and_then(|op| op.join()) {
            Ok(m) => break m,
            Err(_) => std::thread::sleep(Duration::from_secs(10)),
        }
    };
    // Apps opening or closing their media session.
    let tx_sessions = tx.clone();
    let _sessions_token = manager.SessionsChanged(&TypedEventHandler::new(move |_, _| {
        let _ = tx_sessions.send(Command::Refresh);
        Ok(())
    }));

    let notify = || unsafe {
        let _ = PostMessageW(Some(HWND(hwnd_raw as *mut c_void)), WM_MUSIC_UPDATED, WPARAM(0), LPARAM(0));
    };
    let mut subscription: Option<Subscription> = None;
    let mut song = SongState::default();
    let mut last_seen = Instant::now();
    let mut fast_until = Instant::now();

    loop {
        let session = find_session(&manager);
        let same = match (&session, &subscription) {
            (Some(s), Some(sub)) => sub.session == *s,
            (None, None) => true,
            _ => false,
        };
        if !same {
            subscription = session.as_ref().and_then(|s| Subscription::new(s, &tx));
        }

        let previous = current();
        let mut holding = false;
        let next = match &session {
            Some(s) => read(s, previous.as_ref(), &mut song, &covers, &mut holding),
            None => None,
        };
        let changed = {
            let mut now = NOW.lock().unwrap();
            match next {
                Some(t) => {
                    last_seen = Instant::now();
                    let changed = match &*now {
                        Some(a) => {
                            a.title != t.title
                                || a.artist != t.artist
                                || a.playing != t.playing
                                || !a.present
                                || a.art.as_ref().map(Arc::as_ptr) != t.art.as_ref().map(Arc::as_ptr)
                                || (a.position_now() - t.position).abs() > 1.5 // seeked or restarted
                        }
                        None => true,
                    };
                    *now = Some(t);
                    changed
                }
                // A failed read or a briefly missing session keeps the last state; only after
                // GONE_AFTER do we treat the app as closed (last song stays, dimmed).
                None => match now.as_mut() {
                    Some(t) if t.present && last_seen.elapsed() >= GONE_AFTER => {
                        t.position = t.position_now();
                        t.sampled = Instant::now();
                        t.playing = false;
                        t.present = false;
                        true
                    }
                    _ => false,
                },
            }
        };
        if changed {
            notify();
        }

        let wait = if holding {
            Duration::from_millis(40)
        } else if Instant::now() < fast_until {
            Duration::from_millis(80)
        } else {
            Duration::from_millis(1000)
        };
        let mut pending = Vec::new();
        match rx.recv_timeout(wait) {
            Ok(cmd) => pending.push(cmd),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        // Take everything that's queued: rapid clicks are sent back-to-back, then read once.
        while let Ok(cmd) = rx.try_recv() {
            pending.push(cmd);
        }
        for cmd in pending {
            let Some(s) = &session else { continue };
            let op = match cmd {
                Command::PlayPause => s.TryTogglePlayPauseAsync(),
                Command::Next => s.TrySkipNextAsync(),
                Command::Previous => s.TrySkipPreviousAsync(),
                // Positions are in 100 ns ticks.
                Command::Seek(secs) => s.TryChangePlaybackPositionAsync((secs.max(0.0) * 10_000_000.0) as i64),
                Command::Refresh => continue,
            };
            let _ = op.and_then(|op| op.join());
            fast_until = Instant::now() + FAST_POLL_FOR;
        }
    }
}

fn find_session(manager: &SessionManager) -> Option<Session> {
    let filter = APP_FILTER.to_lowercase();
    let sessions = manager.GetSessions().ok()?;
    sessions.into_iter().find(|s| {
        s.SourceAppUserModelId().map(|id| id.to_string().to_lowercase().contains(&filter)).unwrap_or(false)
    })
}

/// Reads the session (never waits on the network). Returns None if the read failed, in which
/// case the caller keeps the last state. Sets `holding` while a new song is held back for its cover.
fn read(
    session: &Session,
    previous: Option<&Track>,
    song: &mut SongState,
    covers: &Sender<CoverRequest>,
    holding: &mut bool,
) -> Option<Track> {
    let props = session.TryGetMediaPropertiesAsync().ok()?.join().ok()?;
    let title = props.Title().ok()?.to_string();
    let artist = props.Artist().ok()?.to_string();
    let playing = session.GetPlaybackInfo().ok()?.PlaybackStatus().ok()? == PlaybackStatus::Playing;

    let timeline = session.GetTimelineProperties().ok()?;
    let secs = |ticks: i64| ticks as f64 / 10_000_000.0;
    let mut position = secs(timeline.Position().ok()?.Duration);
    let duration = secs(timeline.EndTime().ok()?.Duration - timeline.StartTime().ok()?.Duration);
    // Players only report position now and then; add the time since their last report.
    if playing {
        if let Ok(updated) = timeline.LastUpdatedTime() {
            let now = chrono::Utc::now();
            let now_ticks =
                now.timestamp() * 10_000_000 + now.timestamp_subsec_nanos() as i64 / 100 + 116_444_736_000_000_000;
            let lag = secs(now_ticks - updated.UniversalTime);
            if (0.0..86_400.0).contains(&lag) {
                position += lag;
            }
        }
    }

    // A new song: remember the old cover so we can recognise when the new one arrives.
    let key: SongKey = (title.clone(), artist.clone());
    if song.key != key {
        let previous_fingerprint = song.fingerprint;
        song.changes.retain(|t| t.elapsed() < Duration::from_millis(1500));
        song.changes.push(Instant::now());
        *song = SongState {
            key: key.clone(),
            since: Some(Instant::now()),
            previous_fingerprint,
            fingerprint: 0,
            last_request: None,
            changes: std::mem::take(&mut song.changes),
        };
    }
    let age = song.since.map(|t| t.elapsed()).unwrap_or_default();
    let rapid = song.changes.len() >= 2;

    // The cover: from the cache if loaded, otherwise ask the cover thread (and keep asking for
    // a while, since players often update the cover a moment after the title).
    let art = match cached_cover(&key) {
        Some((image, fp)) => {
            song.fingerprint = fp;
            Some(image)
        }
        None => {
            let due = song.last_request.is_none_or(|t| t.elapsed() >= Duration::from_millis(150));
            if due && age < COVER_GIVE_UP {
                if let Some(thumbnail) = props.Thumbnail().ok().and_then(|t| AgileReference::new(&t).ok()) {
                    let _ = covers.send(CoverRequest {
                        key: key.clone(),
                        thumbnail,
                        previous_fingerprint: song.previous_fingerprint,
                        accept_same: age >= SAME_COVER_AFTER,
                    });
                    song.last_request = Some(Instant::now());
                }
            }
            None
        }
    };

    // After a single skip, hold the new song back very briefly so it can arrive with its cover.
    let new_song = previous.is_none_or(|p| p.title != title || p.artist != artist);
    if new_song && art.is_none() && !rapid && previous.is_some_and(|p| p.present) && age < ART_WAIT {
        *holding = true;
        return previous.cloned();
    }

    Some(Track { title, artist, playing, position, duration, sampled: Instant::now(), art, present: true })
}
