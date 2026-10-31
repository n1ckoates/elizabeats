//! The local Library Source: indexes `.mp3` and `.wav` files below the
//! Library folder, keeps the index fresh while the folder changes, and
//! resolves Track ids back to files.
//!
//! All slow work (walking folders, reading tags, watching) happens on one
//! background "library" thread. Methods on [`LocalLibrary`] only read the
//! latest published index, so they are cheap and never wait for a scan —
//! except [`LocalLibrary::artwork`], which may read one file.

// Tests may panic on failure; production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod artwork;
mod scan;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use elizabeats_core::{DiagnosticEvent, DiagnosticSink, SourceId, Track};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

pub use artwork::Artwork;

use artwork::{ArtworkCache, read_artwork};

/// Quiet period after the last filesystem change before rescanning.
const RESCAN_DEBOUNCE: Duration = Duration::from_millis(250);
/// A steady stream of changes (e.g. a long copy) still rescans this often.
const RESCAN_MAX_DELAY: Duration = Duration::from_secs(3);
/// How often a missing Library folder is checked for again.
const UNAVAILABLE_POLL_INTERVAL: Duration = Duration::from_secs(2);
const ARTWORK_CACHE_ENTRIES: usize = 200;

const TRACK_UNAVAILABLE: &str = "This local track is no longer available.";

/// The local Library. Share it with `Arc<LocalLibrary>`.
pub struct LocalLibrary {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    /// Wakes the library thread when there is new work.
    wake: Condvar,
    /// Bumped whenever the Library folder changes, so older scans can tell
    /// they have been superseded without taking the lock.
    folder_generation: AtomicU64,
    shutdown: AtomicBool,
    /// The newest rescan request that has been fully handled.
    completed: tokio::sync::watch::Sender<u64>,
    artwork: Mutex<ArtworkState>,
    on_changed: Arc<dyn Fn() + Send + Sync>,
    diagnostics: DiagnosticSink,
}

struct State {
    folder: Option<PathBuf>,
    index: Index,
    /// Rescan request counter; the library thread catches up to it.
    requested: u64,
    /// Pending watcher-triggered rescan: (first event, latest event).
    pending_change: Option<(Instant, Instant)>,
    /// The watcher reported an error and should be recreated.
    watcher_failed: bool,
    /// The last scan found the folder missing (or not a folder).
    unavailable: bool,
    scanning: bool,
    worker_running: bool,
}

#[derive(Clone)]
struct Index {
    tracks: Result<Arc<Vec<Track>>, String>,
    /// Track id → absolute path. The only way ids are turned into paths.
    files: Arc<HashMap<String, PathBuf>>,
}

impl Index {
    fn empty() -> Self {
        Self {
            tracks: Ok(Arc::new(Vec::new())),
            files: Arc::new(HashMap::new()),
        }
    }

    fn unavailable(error: String) -> Self {
        Self {
            tracks: Err(error),
            files: Arc::new(HashMap::new()),
        }
    }
}

struct ArtworkState {
    cache: ArtworkCache,
    /// Bumped when the index changes, so a slow read cannot cache stale art.
    generation: u64,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere must not take the Library down with it.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl LocalLibrary {
    /// `on_changed` runs on the library thread whenever the index (or its
    /// error) changes. `diagnostics` receives scan/watch/resolve events.
    pub fn new(on_changed: Arc<dyn Fn() + Send + Sync>, diagnostics: DiagnosticSink) -> Self {
        let (completed, _) = tokio::sync::watch::channel(0);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                folder: None,
                index: Index::empty(),
                requested: 0,
                pending_change: None,
                watcher_failed: false,
                unavailable: false,
                scanning: false,
                worker_running: true,
            }),
            wake: Condvar::new(),
            folder_generation: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            completed,
            artwork: Mutex::new(ArtworkState {
                cache: ArtworkCache::new(ARTWORK_CACHE_ENTRIES),
                generation: 0,
            }),
            on_changed,
            diagnostics,
        });
        let worker_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("library".into())
            .spawn(move || run_worker(worker_shared));
        if let Err(error) = spawned {
            lock(&shared.state).worker_running = false;
            shared.record(
                "start",
                Some(format!("Could not start the library thread: {error}")),
            );
        }
        Self { shared }
    }

    /// Switches to `folder` (or no Library when `None`), clears the index,
    /// and starts a background scan plus a watcher.
    pub fn set_library_folder(&self, folder: Option<PathBuf>) {
        let folder = folder
            .filter(|folder| !folder.as_os_str().is_empty())
            .map(|folder| std::path::absolute(&folder).unwrap_or(folder));
        {
            let mut state = lock(&self.shared.state);
            self.shared.folder_generation.fetch_add(1, Ordering::SeqCst);
            state.folder = folder;
            state.index = Index::empty();
            state.pending_change = None;
            state.unavailable = false;
            state.requested += 1;
        }
        self.shared.clear_artwork();
        self.shared.wake.notify_all();
        (self.shared.on_changed)();
    }

    /// The current Library folder, if any.
    pub fn library_folder(&self) -> Option<PathBuf> {
        lock(&self.shared.state).folder.clone()
    }

    /// Starts a background rescan; `on_changed` fires when it finishes.
    pub fn rescan(&self) {
        self.request_rescan();
    }

    /// Rescans and waits until a scan that started after this call has
    /// finished. Works with any async executor.
    pub async fn rescan_and_wait(&self) {
        let Some(request) = self.request_rescan() else {
            return;
        };
        let mut completed = self.shared.completed.subscribe();
        // An error only means the library thread has stopped.
        let _ = completed.wait_for(|done| *done >= request).await;
    }

    /// True while a scan is in progress.
    pub fn is_scanning(&self) -> bool {
        lock(&self.shared.state).scanning
    }

    /// The indexed Tracks, sorted by title, artist, then id. Fails when the
    /// Library folder is missing or not a folder.
    pub fn tracks(&self) -> Result<Arc<Vec<Track>>, String> {
        lock(&self.shared.state).index.tracks.clone()
    }

    /// Case-insensitive substring search over "title artist album". An empty
    /// query returns every Track.
    pub fn search(&self, query: &str) -> Result<Vec<Track>, String> {
        let tracks = self.tracks()?;
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return Ok(tracks.as_ref().clone());
        }
        Ok(tracks
            .iter()
            .filter(|track| {
                let haystack = format!(
                    "{} {} {}",
                    track.title,
                    track.artist,
                    track.album.as_deref().unwrap_or_default()
                );
                haystack.to_lowercase().contains(&query)
            })
            .cloned()
            .collect())
    }

    /// The file to play for Track `id`. It must be in the index and still be
    /// a regular file (not a symlink).
    pub fn resolve(&self, id: &str) -> Result<PathBuf, String> {
        let path = self
            .path_of(id)
            .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file()));
        path.ok_or_else(|| {
            self.shared
                .record("resolve", Some(TRACK_UNAVAILABLE.to_owned()));
            TRACK_UNAVAILABLE.to_owned()
        })
    }

    /// Where Track `id` lives on disk (for "Reveal in Finder").
    pub fn path_of(&self, id: &str) -> Option<PathBuf> {
        lock(&self.shared.state).index.files.get(id).cloned()
    }

    /// Embedded cover art for Track `id`. May read the file on a cache miss,
    /// so call it off the UI thread.
    pub fn artwork(&self, id: &str) -> Option<Arc<Artwork>> {
        let generation = {
            let mut artwork = lock(&self.shared.artwork);
            if let Some(cached) = artwork.cache.get(id) {
                return cached;
            }
            artwork.generation
        };
        let path = self.path_of(id)?;
        let art = read_artwork(&path).map(Arc::new);
        let mut artwork = lock(&self.shared.artwork);
        if artwork.generation == generation {
            artwork.cache.insert(id.to_owned(), art.clone());
        }
        art
    }

    fn request_rescan(&self) -> Option<u64> {
        let request = {
            let mut state = lock(&self.shared.state);
            if !state.worker_running {
                return None;
            }
            state.requested += 1;
            state.requested
        };
        self.shared.wake.notify_all();
        Some(request)
    }
}

impl Drop for LocalLibrary {
    fn drop(&mut self) {
        self.shared.shutdown.store(true, Ordering::SeqCst);
        // Take the lock so the library thread cannot miss the wake-up.
        drop(lock(&self.shared.state));
        self.shared.wake.notify_all();
    }
}

impl Shared {
    fn record(&self, operation: &str, error: Option<String>) {
        let mut event = DiagnosticEvent::new(operation).source(SourceId::Local);
        event.error = error;
        (self.diagnostics)(event);
    }

    fn clear_artwork(&self) {
        let mut artwork = lock(&self.artwork);
        artwork.generation += 1;
        artwork.cache.clear();
    }

    /// Called from the watcher thread for every filesystem change.
    fn note_change(&self, generation: u64) {
        if self.folder_generation.load(Ordering::SeqCst) != generation {
            return;
        }
        {
            let mut state = lock(&self.state);
            let now = Instant::now();
            let first = state.pending_change.map_or(now, |(first, _)| first);
            state.pending_change = Some((first, now));
        }
        self.wake.notify_all();
    }
}

/// What the library thread should do next.
enum Work {
    Scan { request: u64 },
    CheckFolder(PathBuf),
    Stop,
}

/// Releases `rescan_and_wait` callers if the library thread ever exits.
struct WorkerExit<'a>(&'a Shared);

impl Drop for WorkerExit<'_> {
    fn drop(&mut self) {
        lock(&self.0.state).worker_running = false;
        self.0.completed.send_replace(u64::MAX);
    }
}

/// The watcher for the current folder, tagged with the folder generation it
/// belongs to.
struct ActiveWatcher {
    generation: u64,
    _watcher: RecommendedWatcher,
}

fn run_worker(shared: Arc<Shared>) {
    let _exit = WorkerExit(&shared);
    let mut handled = 0u64;
    let mut watcher: Option<ActiveWatcher> = None;
    let mut last_folder_check = Instant::now();

    loop {
        match next_work(&shared, handled, &mut last_folder_check) {
            Work::Stop => return,
            Work::CheckFolder(folder) => {
                // Checked without the lock: a hung network volume must not
                // block the UI.
                if folder.is_dir() {
                    lock(&shared.state).requested += 1;
                }
            }
            Work::Scan { request } => {
                run_scan(&shared, &mut watcher);
                handled = request;
                shared.completed.send_if_modified(|done| {
                    let advanced = request > *done;
                    if advanced {
                        *done = request;
                    }
                    advanced
                });
            }
        }
    }
}

/// Sleeps until a rescan is requested, a debounced change is due, or the
/// missing folder should be checked again.
fn next_work(shared: &Shared, handled: u64, last_folder_check: &mut Instant) -> Work {
    let mut state = lock(&shared.state);
    loop {
        if shared.shutdown.load(Ordering::SeqCst) {
            return Work::Stop;
        }
        let now = Instant::now();
        if let Some((first, latest)) = state.pending_change {
            let due = (latest + RESCAN_DEBOUNCE).min(first + RESCAN_MAX_DELAY);
            if now >= due {
                state.pending_change = None;
                state.requested += 1;
            }
        }
        if state.requested > handled {
            return Work::Scan {
                request: state.requested,
            };
        }
        let mut timeout: Option<Duration> = state.pending_change.map(|(first, latest)| {
            (latest + RESCAN_DEBOUNCE)
                .min(first + RESCAN_MAX_DELAY)
                .saturating_duration_since(now)
        });
        if state.unavailable
            && let Some(folder) = &state.folder
        {
            let next_check = *last_folder_check + UNAVAILABLE_POLL_INTERVAL;
            if now >= next_check {
                *last_folder_check = now;
                return Work::CheckFolder(folder.clone());
            }
            let until_check = next_check.saturating_duration_since(now);
            timeout = Some(timeout.map_or(until_check, |t| t.min(until_check)));
        }
        state = match timeout {
            Some(timeout) => {
                shared
                    .wake
                    .wait_timeout(state, timeout)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
            None => shared
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner),
        };
    }
}

fn run_scan(shared: &Arc<Shared>, watcher: &mut Option<ActiveWatcher>) {
    let (folder, generation, watcher_failed) = {
        let mut state = lock(&shared.state);
        state.scanning = true;
        state.pending_change = None;
        let failed = std::mem::take(&mut state.watcher_failed);
        (
            state.folder.clone(),
            shared.folder_generation.load(Ordering::SeqCst),
            failed,
        )
    };
    if watcher_failed {
        *watcher = None;
    }
    let superseded = || {
        shared.folder_generation.load(Ordering::SeqCst) != generation
            || shared.shutdown.load(Ordering::SeqCst)
    };

    let Some(folder) = folder else {
        *watcher = None;
        lock(&shared.state).scanning = false;
        return;
    };

    let problem = match std::fs::metadata(&folder) {
        Err(_) => Some((
            format!("Library folder is unavailable: {}", folder.display()),
            "Library folder is unavailable.",
        )),
        Ok(meta) if !meta.is_dir() => Some((
            format!("Library folder is not a folder: {}", folder.display()),
            "Library folder is not a folder.",
        )),
        Ok(_) => None,
    };
    if let Some((error, diagnostic)) = problem {
        *watcher = None;
        shared.record("rescan", Some(diagnostic.to_owned()));
        publish(shared, generation, Index::unavailable(error), true);
        return;
    }

    // Watch before scanning so changes made during the scan trigger another.
    if watcher
        .as_ref()
        .is_none_or(|active| active.generation != generation)
    {
        *watcher = None;
        match start_watcher(shared, &folder, generation) {
            Ok(started) => *watcher = Some(started),
            Err(error) => shared.record("watch", Some(error.to_string())),
        }
    }

    let Some(result) = scan::scan(&folder, &superseded) else {
        lock(&shared.state).scanning = false;
        return;
    };
    let index = Index {
        tracks: Ok(Arc::new(result.tracks)),
        files: Arc::new(result.files),
    };
    if publish(shared, generation, index, false) {
        shared.record("rescan", None);
    }
}

/// Replaces the index unless the folder changed meanwhile. Returns whether
/// it was published.
fn publish(shared: &Shared, generation: u64, index: Index, unavailable: bool) -> bool {
    {
        let mut state = lock(&shared.state);
        state.scanning = false;
        if shared.folder_generation.load(Ordering::SeqCst) != generation {
            return false;
        }
        state.index = index;
        state.unavailable = unavailable;
    }
    shared.clear_artwork();
    (shared.on_changed)();
    true
}

fn start_watcher(
    shared: &Arc<Shared>,
    folder: &Path,
    generation: u64,
) -> notify::Result<ActiveWatcher> {
    let weak: Weak<Shared> = Arc::downgrade(shared);
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let Some(shared) = weak.upgrade() else {
            return;
        };
        match result {
            Ok(event) if is_relevant(&event) => shared.note_change(generation),
            Ok(_) => {}
            Err(error) => {
                shared.record("watch", Some(error.to_string()));
                lock(&shared.state).watcher_failed = true;
                shared.note_change(generation);
            }
        }
    })?;
    watcher.watch(folder, RecursiveMode::Recursive)?;
    Ok(ActiveWatcher {
        generation,
        _watcher: watcher,
    })
}

/// Ignores reads and Finder's `.DS_Store` bookkeeping, which would otherwise
/// rescan whenever someone browses the folder.
fn is_relevant(event: &notify::Event) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.paths.is_empty()
        || !event
            .paths
            .iter()
            .all(|path| path.file_name().is_some_and(|name| name == ".DS_Store"))
}

#[cfg(test)]
mod tests;
