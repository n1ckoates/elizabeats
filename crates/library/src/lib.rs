//! The local Library Source: indexes `.mp3` and `.wav` files below the
//! Library folder, keeps the index fresh while the folder changes, and
//! resolves Track ids back to files.
//!
//! All slow work (walking folders, reading tags, watching) happens on one
//! background "library" thread. Methods on [`LocalLibrary`] only read the
//! latest published index, so they are cheap and never wait for a scan —
//! except [`LocalLibrary::artwork`], which reads one file.

// Tests may panic on failure; production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod scan;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use elizabeats_core::{DiagnosticEvent, DiagnosticSink, SourceId, Track};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// Quiet period after the last filesystem change before rescanning.
const RESCAN_DEBOUNCE: Duration = Duration::from_millis(250);
/// A steady stream of changes (e.g. a long copy) still rescans this often.
const RESCAN_MAX_DELAY: Duration = Duration::from_secs(3);
/// How often a missing Library folder is checked for again.
const UNAVAILABLE_POLL_INTERVAL: Duration = Duration::from_secs(2);

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
    /// The newest rescan request that has been fully handled; `u64::MAX`
    /// once the library thread is gone, which releases every waiter.
    completed: tokio::sync::watch::Sender<u64>,
    on_changed: Arc<dyn Fn() + Send + Sync>,
    diagnostics: DiagnosticSink,
}

struct State {
    folder: Option<PathBuf>,
    /// The Tracks, or why the folder can't be scanned (then `files` is empty).
    tracks: Result<Arc<Vec<Track>>, String>,
    /// Track id → absolute path. The only way ids are turned into paths.
    files: Arc<HashMap<String, PathBuf>>,
    /// Rescan request counter; the library thread catches up to it.
    requested: u64,
    /// Pending watcher-triggered rescan: (first event, latest event).
    pending_change: Option<(Instant, Instant)>,
    /// The watcher reported an error and should be recreated.
    watcher_failed: bool,
}

impl State {
    fn set_index(&mut self, tracks: Result<Vec<Track>, String>, files: HashMap<String, PathBuf>) {
        self.tracks = tracks.map(Arc::new);
        self.files = Arc::new(files);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic elsewhere must not take the Library down with it.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// When a pending change (first event, latest event) should be rescanned.
fn change_due((first, latest): (Instant, Instant)) -> Instant {
    (latest + RESCAN_DEBOUNCE).min(first + RESCAN_MAX_DELAY)
}

impl LocalLibrary {
    /// `on_changed` runs on the library thread whenever the index (or its
    /// error) changes. `diagnostics` receives scan/watch/resolve events.
    pub fn new(on_changed: Arc<dyn Fn() + Send + Sync>, diagnostics: DiagnosticSink) -> Self {
        let (completed, _) = tokio::sync::watch::channel(0);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                folder: None,
                tracks: Ok(Arc::default()),
                files: Arc::default(),
                requested: 0,
                pending_change: None,
                watcher_failed: false,
            }),
            wake: Condvar::new(),
            folder_generation: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
            completed,
            on_changed,
            diagnostics,
        });
        let worker_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("library".into())
            .spawn(move || run_worker(worker_shared));
        if let Err(error) = spawned {
            shared.completed.send_replace(u64::MAX);
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
            state.set_index(Ok(Vec::new()), HashMap::new());
            state.pending_change = None;
            state.requested += 1;
        }
        self.shared.wake.notify_all();
        (self.shared.on_changed)();
    }

    /// Rescans and waits until a scan that started after this call has
    /// finished. Works with any async executor.
    pub async fn rescan_and_wait(&self) {
        let request = {
            let mut state = lock(&self.shared.state);
            state.requested += 1;
            state.requested
        };
        self.shared.wake.notify_all();
        let mut completed = self.shared.completed.subscribe();
        // An error only means the library thread has stopped.
        let _ = completed.wait_for(|done| *done >= request).await;
    }

    /// The indexed Tracks, sorted by title, artist, then id. Fails when the
    /// Library folder is missing or not a folder.
    fn tracks(&self) -> Result<Arc<Vec<Track>>, String> {
        lock(&self.shared.state).tracks.clone()
    }

    /// Case-insensitive substring search over "title artist album". An empty
    /// query returns every Track.
    pub fn search(&self, query: &str) -> Result<Vec<Track>, String> {
        let tracks = self.tracks()?;
        let query = query.trim().to_lowercase();
        Ok(tracks
            .iter()
            .filter(|track| {
                query.is_empty() || {
                    let album = track.album.as_deref().unwrap_or_default();
                    let haystack = format!("{} {} {album}", track.title, track.artist);
                    haystack.to_lowercase().contains(&query)
                }
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
        lock(&self.shared.state).files.get(id).cloned()
    }

    /// Embedded cover art (encoded image bytes) for Track `id`: the front
    /// cover, else the first picture. Reads the file, so call it off the UI
    /// thread; callers cache the result.
    pub fn artwork(&self, id: &str) -> Option<Vec<u8>> {
        scan::read_artwork(&self.path_of(id)?)
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

    /// Replaces the index unless the folder changed meanwhile. Returns whether
    /// it was published.
    fn publish(
        &self,
        generation: u64,
        tracks: Result<Vec<Track>, String>,
        files: HashMap<String, PathBuf>,
    ) -> bool {
        {
            let mut state = lock(&self.state);
            if self.folder_generation.load(Ordering::SeqCst) != generation {
                return false;
            }
            state.set_index(tracks, files);
        }
        (self.on_changed)();
        true
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
        if state
            .pending_change
            .is_some_and(|change| now >= change_due(change))
        {
            state.pending_change = None;
            state.requested += 1;
        }
        if state.requested > handled {
            return Work::Scan {
                request: state.requested,
            };
        }
        let mut wake_at = state.pending_change.map(change_due);
        if state.tracks.is_err()
            && let Some(folder) = &state.folder
        {
            let next_check = *last_folder_check + UNAVAILABLE_POLL_INTERVAL;
            if now >= next_check {
                *last_folder_check = now;
                return Work::CheckFolder(folder.clone());
            }
            wake_at = Some(wake_at.map_or(next_check, |at| at.min(next_check)));
        }
        state = match wake_at {
            Some(at) => {
                shared
                    .wake
                    .wait_timeout(state, at.saturating_duration_since(now))
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
        state.pending_change = None;
        (
            state.folder.clone(),
            shared.folder_generation.load(Ordering::SeqCst),
            std::mem::take(&mut state.watcher_failed),
        )
    };
    // Dropped outside the lock: the watcher's callback may be waiting for it.
    if watcher_failed {
        *watcher = None;
    }
    let Some(folder) = folder else {
        *watcher = None;
        return;
    };

    let problem = match std::fs::metadata(&folder) {
        Err(_) => Some("Library folder is unavailable"),
        Ok(meta) if !meta.is_dir() => Some("Library folder is not a folder"),
        Ok(_) => None,
    };
    if let Some(message) = problem {
        *watcher = None;
        shared.record("rescan", Some(format!("{message}.")));
        let error = format!("{message}: {}", folder.display());
        shared.publish(generation, Err(error), HashMap::new());
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

    let superseded = || {
        shared.folder_generation.load(Ordering::SeqCst) != generation
            || shared.shutdown.load(Ordering::SeqCst)
    };
    if let Some((tracks, files)) = scan::scan(&folder, &superseded)
        && shared.publish(generation, Ok(tracks), files)
    {
        shared.record("rescan", None);
    }
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
