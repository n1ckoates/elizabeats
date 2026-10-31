//! YouTube Music Source: search, Track metadata, and the Download Cache.
//!
//! Tracks are always fully downloaded into the Download Cache before they are
//! played (see `CONTEXT.md`). Downloads run one at a time, in request order,
//! with a random 5–10 s pause between them so we look like a person, not a
//! bot. Several callers asking for the same video share one download.
//!
//! Layout under `data_dir`:
//! - `download-cache/` – audio files plus `manifest.json`

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod cache;
mod cache_files;
mod error;
mod http_download;
mod innertube;
mod source;
mod video_id;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use elizabeats_botguard::BotguardService;
use elizabeats_core::{DiagnosticEvent, DiagnosticSink, DownloadCacheStatus, SourceId, Track};
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;

use crate::cache::{CacheEntry, CacheState};
use crate::cache_files::CacheFiles;
use crate::source::{CLIENT_NAME, YoutubeSource};
use crate::video_id::is_valid_video_id;

pub use crate::error::YoutubeError;
pub use crate::video_id::parse_video_id;

const DEFAULT_MAX_CACHE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);

pub struct YoutubeConfig {
    /// `download-cache/` lives under here.
    pub data_dir: PathBuf,
    /// Mints PO tokens and resolves stream URLs. Downloads fail without it;
    /// search and link lookup don't need it.
    pub botguard: Option<BotguardService>,
    pub diagnostics: DiagnosticSink,
    /// Called after every Download Cache change (from any thread).
    pub on_cache_change: Arc<dyn Fn() + Send + Sync>,
}

/// The YouTube Music Source. Share it via `Arc`; every method takes `&self`.
/// Must be used from within a tokio runtime.
pub struct YoutubeMusic {
    inner: Arc<Inner>,
    /// Sender for the download worker, started on the first download.
    /// Kept outside `Inner` so dropping `YoutubeMusic` stops the worker.
    jobs: Mutex<Option<mpsc::UnboundedSender<Job>>>,
}

impl YoutubeMusic {
    pub fn new(config: YoutubeConfig) -> Result<Self, YoutubeError> {
        let source = Arc::new(YoutubeSource::new(config.botguard.clone())?);
        Ok(Self::with_parts(
            config,
            source.clone(),
            source,
            Timing::default(),
        ))
    }

    fn with_parts(
        config: YoutubeConfig,
        source: Arc<YoutubeSource>,
        fetcher: Arc<dyn AudioFetcher>,
        timing: Timing,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                files: CacheFiles::new(config.data_dir.join("download-cache")),
                source,
                fetcher,
                diagnostics: config.diagnostics,
                on_cache_change: config.on_cache_change,
                state: Mutex::new(CacheState::new(DEFAULT_MAX_CACHE_BYTES)),
                mutation: tokio::sync::Mutex::new(()),
                loaded: tokio::sync::OnceCell::new(),
                downloads: Mutex::new(HashMap::new()),
                timing,
            }),
            jobs: Mutex::new(None),
        }
    }

    /// Loads the manifest, reconciles it with files on disk, trims to the limit.
    pub async fn initialize(
        &self,
        max_cache_bytes: u64,
        protected_video_ids: Vec<String>,
    ) -> Result<(), YoutubeError> {
        {
            let mut state = lock(&self.inner.state);
            state.max_bytes = max_cache_bytes;
            state.protected = protected_video_ids.into_iter().collect();
        }
        let inner = &self.inner;
        inner.ensure_loaded().await;
        let _guard = inner.mutation.lock().await;
        inner
            .files
            .ensure_dir()
            .await
            .map_err(YoutubeError::cache)?;
        inner
            .files
            .remove_stale_temp_files(&inner.in_flight())
            .await;
        inner.reconcile_locked().await;
        inner.trim_locked().await;
        inner.persist_locked().await
    }

    /// Song search (≤ 20 Tracks), or a single Track when `query` is a
    /// YouTube/YouTube Music URL. Empty query → no Tracks. Times out after
    /// 15 s. Callers cancel by dropping the future.
    pub async fn search(&self, query: &str) -> Result<Vec<Track>, YoutubeError> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let video_id = parse_video_id(query);
        let source = &self.inner.source;
        let lookup = async {
            match &video_id {
                Some(id) => source.track_for_video(id).await.map(|track| vec![track]),
                None => source.search_tracks(query).await,
            }
        };
        let result = tokio::time::timeout(SEARCH_TIMEOUT, lookup)
            .await
            .unwrap_or(Err(YoutubeError::SearchTimedOut));
        self.inner
            .record("search", video_id.as_deref(), result.as_ref().err());
        result
    }

    /// Returns the path of a fully downloaded, cached audio file for
    /// `video_id`, downloading it first if needed. Concurrent calls for the
    /// same id share one download. Downloads run one at a time with a random
    /// 5–10 s gap after the previous one finished. Dropping the future does
    /// not cancel the download; it still lands in the cache.
    pub async fn resolve(&self, video_id: &str) -> Result<PathBuf, YoutubeError> {
        let result = self.resolve_inner(video_id).await;
        let id = is_valid_video_id(video_id).then_some(video_id);
        self.inner.record("resolve", id, result.as_ref().err());
        result
    }

    async fn resolve_inner(&self, video_id: &str) -> Result<PathBuf, YoutubeError> {
        if !is_valid_video_id(video_id) {
            return Err(YoutubeError::InvalidVideoId);
        }
        self.inner.ensure_loaded().await;
        if let Some(path) = self.inner.cached_path(video_id).await {
            return Ok(path);
        }
        let mut done = self.enqueue_download(video_id)?;
        match done.wait_for(Option::is_some).await {
            Ok(result) => result
                .clone()
                .unwrap_or(Err(YoutubeError::DownloadInterrupted)),
            Err(_) => Err(YoutubeError::DownloadInterrupted),
        }
    }

    /// Joins the in-flight download for `video_id`, or queues a new one.
    fn enqueue_download(
        &self,
        video_id: &str,
    ) -> Result<watch::Receiver<DownloadResult>, YoutubeError> {
        let mut downloads = lock(&self.inner.downloads);
        if let Some(existing) = downloads.get(video_id) {
            // A closed channel means its worker is gone; start over below.
            if existing.has_changed().is_ok() {
                return Ok(existing.clone());
            }
        }
        let (done_tx, done_rx) = watch::channel(None);
        let mut job = Job {
            video_id: video_id.to_owned(),
            done: done_tx,
        };
        let mut jobs = lock(&self.jobs);
        if let Some(sender) = jobs.as_ref() {
            match sender.send(job) {
                Ok(()) => {
                    downloads.insert(video_id.to_owned(), done_rx.clone());
                    return Ok(done_rx);
                }
                Err(mpsc::error::SendError(returned)) => job = returned,
            }
        }
        // First download (or the worker stopped): start a worker.
        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(run_worker(self.inner.clone(), receiver));
        sender
            .send(job)
            .map_err(|_| YoutubeError::DownloadInterrupted)?;
        *jobs = Some(sender);
        downloads.insert(video_id.to_owned(), done_rx.clone());
        Ok(done_rx)
    }

    /// Gets search ready in the background (client version, connection), so
    /// the first search after launch is as fast as later ones. Call within a
    /// tokio runtime; failures are ignored (the first search retries).
    pub fn prewarm(&self) {
        let source = self.inner.source.clone();
        tokio::spawn(async move { source.warm_up().await });
    }

    pub fn cache_status(&self) -> DownloadCacheStatus {
        let state = lock(&self.inner.state);
        DownloadCacheStatus {
            used_bytes: state.used_bytes(),
            max_bytes: state.max_bytes,
            pending_clear_count: state.pending_clear_count(),
        }
    }

    /// Video IDs with audio in the Download Cache.
    pub fn cached_video_ids(&self) -> Vec<String> {
        lock(&self.inner.state).cached_video_ids()
    }

    /// Videos that must not be evicted (e.g. everything in the Queue).
    pub async fn set_protected_video_ids(&self, ids: Vec<String>) -> Result<(), YoutubeError> {
        lock(&self.inner.state).protected = ids.into_iter().collect();
        self.inner.trim_and_persist().await
    }

    pub async fn set_max_cache_bytes(&self, bytes: u64) -> Result<(), YoutubeError> {
        lock(&self.inner.state).max_bytes = bytes;
        self.inner.trim_and_persist().await
    }

    /// Removes all cached audio now, except protected or downloading
    /// videos, which are removed as soon as they stop being protected.
    pub async fn clear_download_cache(&self) -> Result<(), YoutubeError> {
        let inner = &self.inner;
        inner.ensure_loaded().await;
        let _guard = inner.mutation.lock().await;
        for entry in lock(&inner.state).entries.values_mut() {
            if entry.file_name.is_some() {
                entry.clear_requested = true;
            }
        }
        inner.trim_locked().await;
        inner.persist_locked().await
    }

    /// Counts a play; frequently played Tracks are evicted last.
    pub async fn record_play(&self, video_id: &str) -> Result<(), YoutubeError> {
        if !is_valid_video_id(video_id) {
            return Err(YoutubeError::InvalidVideoId);
        }
        let inner = &self.inner;
        inner.ensure_loaded().await;
        let _guard = inner.mutation.lock().await;
        {
            let mut state = lock(&inner.state);
            let entry = state
                .entries
                .entry(video_id.to_owned())
                .or_insert_with(|| CacheEntry::new(video_id));
            entry.play_count = entry.play_count.saturating_add(1);
        }
        inner.persist_locked().await
    }

    /// Waits for pending manifest writes.
    pub async fn flush(&self) {
        drop(self.inner.mutation.lock().await);
    }
}

type DownloadResult = Option<Result<PathBuf, YoutubeError>>;
pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Audio that a fetcher has written (atomically) into the cache directory.
pub(crate) struct FetchedAudio {
    pub file_name: String,
    pub byte_size: u64,
    /// How the audio was fetched (for logs).
    pub client_type: String,
}

/// Downloads one video's audio into the cache directory. Behind a trait so
/// the queue can be tested without the network.
pub(crate) trait AudioFetcher: Send + Sync {
    fn fetch<'a>(
        &'a self,
        video_id: &'a str,
        files: &'a CacheFiles,
    ) -> BoxFuture<'a, Result<FetchedAudio, YoutubeError>>;
}

#[derive(Clone, Debug)]
struct Timing {
    min_gap: Duration,
    max_gap: Duration,
    /// Limit for one whole download, from player request to final rename.
    download_timeout: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            min_gap: Duration::from_secs(5),
            max_gap: Duration::from_secs(10),
            download_timeout: Duration::from_secs(120),
        }
    }
}

struct Job {
    video_id: String,
    done: watch::Sender<DownloadResult>,
}

struct Inner {
    files: CacheFiles,
    source: Arc<YoutubeSource>,
    fetcher: Arc<dyn AudioFetcher>,
    diagnostics: DiagnosticSink,
    on_cache_change: Arc<dyn Fn() + Send + Sync>,
    /// Current cache state; locked only briefly, never across an `.await`.
    state: Mutex<CacheState>,
    /// Held for the whole of each cache mutation (including file I/O and the
    /// manifest write) so mutations run one after another.
    mutation: tokio::sync::Mutex<()>,
    loaded: tokio::sync::OnceCell<()>,
    /// Downloads queued or running, keyed by video ID.
    downloads: Mutex<HashMap<String, watch::Receiver<DownloadResult>>>,
    timing: Timing,
}

/// Processes download jobs strictly one at a time, in order.
async fn run_worker(inner: Arc<Inner>, mut jobs: mpsc::UnboundedReceiver<Job>) {
    let mut last_finished: Option<Instant> = None;
    while let Some(job) = jobs.recv().await {
        let result = inner.run_job(&job.video_id, &mut last_finished).await;
        lock(&inner.downloads).remove(&job.video_id);
        // Evictions skipped while this video was downloading can happen now.
        if let Err(error) = inner.trim_and_persist().await {
            inner.record("cache-write", None, Some(&error));
        }
        // Only now, so waiters see the cache (and its manifest) settled.
        let _ = job.done.send(Some(result));
    }
}

impl Inner {
    async fn run_job(
        &self,
        video_id: &str,
        last_finished: &mut Option<Instant>,
    ) -> Result<PathBuf, YoutubeError> {
        if let Some(path) = self.cached_path(video_id).await {
            return Ok(path);
        }
        if let Some(last) = *last_finished {
            let gap_ms = random_between(
                u64::try_from(self.timing.min_gap.as_millis()).unwrap_or(u64::MAX),
                u64::try_from(self.timing.max_gap.as_millis()).unwrap_or(u64::MAX),
            );
            tokio::time::sleep_until(last + Duration::from_millis(gap_ms)).await;
        }
        let started = Instant::now();
        let mut fetched = self.fetch_with_timeout(video_id).await;
        // Out of disk space: make room by evicting cached Tracks nothing needs
        // right now, then try once more.
        if matches!(fetched, Err(YoutubeError::DiskFull)) && self.evict_everything_evictable().await
        {
            tracing::warn!(
                "disk full while downloading {video_id}; freed cache space and retrying"
            );
            fetched = self.fetch_with_timeout(video_id).await;
        }
        *last_finished = Some(Instant::now());
        let fetched = fetched?;
        tracing::info!(
            "downloaded {video_id} ({} bytes, {} client) in {:.1} s",
            fetched.byte_size,
            fetched.client_type,
            started.elapsed().as_secs_f64()
        );
        self.register_download(video_id, &fetched).await
    }

    async fn fetch_with_timeout(&self, video_id: &str) -> Result<FetchedAudio, YoutubeError> {
        tokio::time::timeout(
            self.timing.download_timeout,
            self.fetcher.fetch(video_id, &self.files),
        )
        .await
        .unwrap_or(Err(YoutubeError::DownloadTimedOut))
    }

    /// Deletes every cached file that isn't protected (Playing or in the
    /// Queue) or downloading, regardless of the cache size limit. Returns
    /// whether anything was deleted.
    async fn evict_everything_evictable(&self) -> bool {
        self.ensure_loaded().await;
        let _guard = self.mutation.lock().await;
        let candidates = lock(&self.state).eviction_candidates(&self.in_flight());
        let mut evicted = false;
        for candidate in candidates {
            let Some(file_name) = candidate.file_name.as_deref() else {
                continue;
            };
            match self.files.remove(file_name).await {
                Ok(()) => {
                    evicted = true;
                    if let Some(entry) = lock(&self.state).entries.get_mut(&candidate.video_id) {
                        entry.remove_audio();
                    }
                }
                Err(error) => {
                    let error = YoutubeError::cache(error);
                    self.record("cache-remove", Some(&candidate.video_id), Some(&error));
                }
            }
        }
        if evicted && let Err(error) = self.persist_locked().await {
            self.record("cache-write", None, Some(&error));
        }
        evicted
    }

    async fn register_download(
        &self,
        video_id: &str,
        fetched: &FetchedAudio,
    ) -> Result<PathBuf, YoutubeError> {
        let path = self
            .files
            .path(&fetched.file_name)
            .map_err(YoutubeError::cache)?;
        let _guard = self.mutation.lock().await;
        {
            let mut state = lock(&self.state);
            let entry = state
                .entries
                .entry(video_id.to_owned())
                .or_insert_with(|| CacheEntry::new(video_id));
            entry.file_name = Some(fetched.file_name.clone());
            entry.byte_size = Some(fetched.byte_size);
            entry.downloaded_at = Some(cache::now_timestamp());
            entry.clear_requested = false;
        }
        self.trim_locked().await;
        // The audio is safely on disk; a manifest write failure is reported but
        // doesn't fail the download (initialize would adopt the file anyway).
        if let Err(error) = self.persist_locked().await {
            self.record("cache-write", Some(video_id), Some(&error));
        }
        Ok(path)
    }

    /// Loads the manifest once. Every cache operation waits for this, so a
    /// download can never overwrite a manifest that hasn't been read yet.
    async fn ensure_loaded(&self) {
        self.loaded
            .get_or_init(|| async {
                let entries = self
                    .files
                    .read_manifest()
                    .await
                    .map(|value| cache::parse_manifest(&value))
                    .unwrap_or_default();
                lock(&self.state).entries = entries;
            })
            .await;
    }

    /// The cached file for `video_id` if it is really on disk. Fixes up the
    /// manifest when the file vanished or changed size.
    async fn cached_path(&self, video_id: &str) -> Option<PathBuf> {
        let known = lock(&self.state)
            .entries
            .get(video_id)
            .is_some_and(|entry| entry.file_name.is_some());
        if !known {
            return None;
        }
        let _guard = self.mutation.lock().await;
        let (file_name, recorded_size) = {
            let state = lock(&self.state);
            let entry = state.entries.get(video_id)?;
            (entry.file_name.clone()?, entry.byte_size)
        };
        let size = if is_playable_file(&file_name) {
            self.files.size(&file_name).await.filter(|size| *size > 0)
        } else {
            // Opus/WebM from the Electron app: our audio engine can't play it.
            let _ = self.files.remove(&file_name).await;
            None
        };
        if size != recorded_size {
            if let Some(entry) = lock(&self.state).entries.get_mut(video_id) {
                match size {
                    Some(size) => entry.byte_size = Some(size),
                    None => entry.remove_audio(),
                }
            }
            if let Err(error) = self.persist_locked().await {
                self.record("cache-write", Some(video_id), Some(&error));
            }
        }
        size?;
        self.files.path(&file_name).ok()
    }

    /// Makes the manifest match the disk (call with the mutation lock held):
    /// entries whose file is missing, empty or unplayable lose their audio,
    /// and complete `<videoId>.m4a` files missing from the manifest (e.g.
    /// after a crash) are adopted.
    async fn reconcile_locked(&self) {
        let with_files: Vec<(String, String)> = lock(&self.state)
            .entries
            .values()
            .filter_map(|entry| Some((entry.video_id.clone(), entry.file_name.clone()?)))
            .collect();
        for (video_id, file_name) in with_files {
            let size = if is_playable_file(&file_name) {
                self.files.size(&file_name).await.filter(|size| *size > 0)
            } else {
                None
            };
            if size.is_none() {
                // Missing, empty or unplayable: make sure nothing is left behind.
                let _ = self.files.remove(&file_name).await;
            }
            if let Some(entry) = lock(&self.state).entries.get_mut(&video_id) {
                match size {
                    Some(size) => entry.byte_size = Some(size),
                    None => entry.remove_audio(),
                }
            }
        }

        let known: HashSet<String> = lock(&self.state)
            .entries
            .values()
            .filter_map(|entry| entry.file_name.clone())
            .collect();
        for (file_name, size) in self.files.list_audio_files().await {
            if known.contains(&file_name) {
                continue;
            }
            let adoptable = file_name
                .strip_suffix(".m4a")
                .filter(|id| is_valid_video_id(id));
            match adoptable.filter(|_| size > 0) {
                Some(video_id) => {
                    let mut state = lock(&self.state);
                    let entry = state
                        .entries
                        .entry(video_id.to_owned())
                        .or_insert_with(|| CacheEntry::new(video_id));
                    entry.file_name = Some(file_name.clone());
                    entry.byte_size = Some(size);
                    entry.downloaded_at = Some(cache::now_timestamp());
                    entry.clear_requested = false;
                }
                None => {
                    let _ = self.files.remove(&file_name).await;
                }
            }
        }
    }

    async fn trim_and_persist(&self) -> Result<(), YoutubeError> {
        self.ensure_loaded().await;
        let _guard = self.mutation.lock().await;
        self.trim_locked().await;
        self.persist_locked().await
    }

    /// Evicts files until the cache fits (call with the mutation lock held).
    /// Clear-requested files go regardless of size. Protected and in-flight
    /// videos are never evicted.
    async fn trim_locked(&self) {
        let in_flight = self.in_flight();
        let (candidates, mut used, max) = {
            let state = lock(&self.state);
            (
                state.eviction_candidates(&in_flight),
                state.used_bytes(),
                state.max_bytes,
            )
        };
        for candidate in candidates {
            if !candidate.clear_requested && used <= max {
                continue;
            }
            let Some(file_name) = candidate.file_name.as_deref() else {
                continue;
            };
            match self.files.remove(file_name).await {
                Ok(()) => {
                    used = used.saturating_sub(candidate.byte_size.unwrap_or(0));
                    if let Some(entry) = lock(&self.state).entries.get_mut(&candidate.video_id) {
                        entry.remove_audio();
                    }
                }
                Err(error) => {
                    let error = YoutubeError::cache(error);
                    self.record("cache-remove", Some(&candidate.video_id), Some(&error));
                }
            }
        }
    }

    /// Writes the manifest and notifies the app (call with the mutation lock held).
    async fn persist_locked(&self) -> Result<(), YoutubeError> {
        let json = lock(&self.state)
            .manifest_json()
            .map_err(YoutubeError::cache);
        let result = match json {
            Ok(json) => self
                .files
                .write_manifest(&json)
                .await
                .map_err(YoutubeError::cache),
            Err(error) => Err(error),
        };
        (self.on_cache_change)();
        result
    }

    fn in_flight(&self) -> HashSet<String> {
        lock(&self.downloads).keys().cloned().collect()
    }

    fn record(&self, operation: &str, video_id: Option<&str>, error: Option<&YoutubeError>) {
        let mut event = DiagnosticEvent::new(operation)
            .source(SourceId::YoutubeMusic)
            .client_name(CLIENT_NAME);
        if let Some(video_id) = video_id {
            event = event.video_id(video_id);
        }
        if let Some(error) = error {
            event.status_code = error.status_code();
            event = event.error(error.diagnostic_message());
        }
        (self.diagnostics)(event);
    }
}

/// Only AAC in MP4 is playable by our audio engine.
fn is_playable_file(file_name: &str) -> bool {
    file_name.ends_with(".m4a")
}

/// Locks a std mutex, recovering the data if another thread panicked while holding it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A random number in `min..=max`, using the OS random source behind UUID v4.
pub(crate) fn random_between(min: u64, max: u64) -> u64 {
    if max <= min {
        return min;
    }
    let span = u128::from(max - min) + 1;
    let offset = uuid::Uuid::new_v4().as_u128() % span;
    min + u64::try_from(offset).unwrap_or(0)
}

#[cfg(test)]
mod tests;
