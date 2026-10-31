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

use crate::cache::CacheState;
use crate::cache_files::{CacheFiles, PartFile};
use crate::http_download::StreamFetcher;
use crate::innertube::{CLIENT_NAME, Innertube};
use crate::video_id::{is_valid_video_id, parse_video_id};

pub use crate::error::YoutubeError;

const DEFAULT_MAX_CACHE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
/// Pause between two downloads, in milliseconds (picked at random each time).
const DOWNLOAD_GAP_MS: (u64, u64) = (5_000, 10_000);
/// Limit for one whole download, from player request to final rename.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

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
/// Must be created and used within a tokio runtime.
pub struct YoutubeMusic {
    inner: Arc<Inner>,
    /// Feeds the download worker. Kept outside `Inner` (which the worker
    /// holds) so dropping `YoutubeMusic` stops the worker.
    jobs: mpsc::UnboundedSender<Job>,
}

impl YoutubeMusic {
    pub fn new(mut config: YoutubeConfig) -> Result<Self, YoutubeError> {
        let fetcher = Arc::new(StreamFetcher::new(config.botguard.take())?);
        Self::with_fetcher(config, fetcher)
    }

    fn with_fetcher(
        config: YoutubeConfig,
        fetcher: Arc<dyn AudioFetcher>,
    ) -> Result<Self, YoutubeError> {
        let inner = Arc::new(Inner {
            files: CacheFiles::new(config.data_dir.join("download-cache")),
            innertube: Innertube::new()?,
            fetcher,
            diagnostics: config.diagnostics,
            on_cache_change: config.on_cache_change,
            state: Mutex::new(CacheState::new(DEFAULT_MAX_CACHE_BYTES)),
            mutation: tokio::sync::Mutex::new(()),
            loaded: tokio::sync::OnceCell::new(),
            downloads: Mutex::new(HashMap::new()),
        });
        let (jobs, receiver) = mpsc::unbounded_channel();
        tokio::spawn(run_worker(inner.clone(), receiver));
        Ok(Self { inner, jobs })
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
        let _guard = inner.mutate().await;
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
        inner.evict_locked(false).await;
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
        let innertube = &self.inner.innertube;
        let lookup = async {
            match &video_id {
                Some(id) => innertube.track_for_video(id).await.map(|track| vec![track]),
                None => innertube.search_tracks(query).await,
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
        let valid = is_valid_video_id(video_id);
        let result = if valid {
            self.resolve_valid(video_id).await
        } else {
            Err(YoutubeError::InvalidVideoId)
        };
        let id = valid.then_some(video_id);
        self.inner.record("resolve", id, result.as_ref().err());
        result
    }

    async fn resolve_valid(&self, video_id: &str) -> Result<PathBuf, YoutubeError> {
        self.inner.ensure_loaded().await;
        if let Some(path) = self.inner.cached_path(video_id).await {
            return Ok(path);
        }
        let mut done = self.enqueue_download(video_id)?;
        done.wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|result| result.clone())
            .unwrap_or(Err(YoutubeError::DownloadInterrupted))
    }

    /// Joins the in-flight download for `video_id`, or queues a new one.
    fn enqueue_download(
        &self,
        video_id: &str,
    ) -> Result<watch::Receiver<DownloadResult>, YoutubeError> {
        let mut downloads = lock(&self.inner.downloads);
        // A closed channel means its job was dropped; queue a new one.
        if let Some(existing) = downloads.get(video_id)
            && existing.has_changed().is_ok()
        {
            return Ok(existing.clone());
        }
        let (done, receiver) = watch::channel(None);
        let job = Job {
            video_id: video_id.to_owned(),
            done,
        };
        self.jobs
            .send(job)
            .map_err(|_| YoutubeError::DownloadInterrupted)?;
        downloads.insert(video_id.to_owned(), receiver.clone());
        Ok(receiver)
    }

    /// Gets search ready in the background (client version, connection), so
    /// the first search after launch is as fast as later ones. Call within a
    /// tokio runtime; failures are ignored (the first search retries).
    pub fn prewarm(&self) {
        let inner = self.inner.clone();
        tokio::spawn(async move { inner.innertube.warm_up().await });
    }

    pub fn cache_status(&self) -> DownloadCacheStatus {
        lock(&self.inner.state).status()
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
        let _guard = inner.mutate().await;
        for entry in lock(&inner.state).entries.values_mut() {
            if entry.file_name.is_some() {
                entry.clear_requested = true;
            }
        }
        inner.evict_locked(false).await;
        inner.persist_locked().await
    }

    /// Counts a play; frequently played Tracks are evicted last.
    pub async fn record_play(&self, video_id: &str) -> Result<(), YoutubeError> {
        if !is_valid_video_id(video_id) {
            return Err(YoutubeError::InvalidVideoId);
        }
        let inner = &self.inner;
        let _guard = inner.mutate().await;
        {
            let mut state = lock(&inner.state);
            let entry = state.entry(video_id);
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

/// Writes one video's audio into `part`, which the cache then moves into
/// place. Behind a trait so the queue can be tested without the network.
pub(crate) trait AudioFetcher: Send + Sync {
    fn fetch<'a>(
        &'a self,
        video_id: &'a str,
        part: &'a mut PartFile,
    ) -> BoxFuture<'a, Result<(), YoutubeError>>;
}

struct Job {
    video_id: String,
    done: watch::Sender<DownloadResult>,
}

struct Inner {
    files: CacheFiles,
    innertube: Innertube,
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
}

/// Processes download jobs strictly one at a time, in order.
async fn run_worker(inner: Arc<Inner>, mut jobs: mpsc::UnboundedReceiver<Job>) {
    let mut last_finished: Option<Instant> = None;
    while let Some(job) = jobs.recv().await {
        let result = inner.download(&job.video_id, &mut last_finished).await;
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
    async fn download(
        &self,
        video_id: &str,
        last_finished: &mut Option<Instant>,
    ) -> Result<PathBuf, YoutubeError> {
        if let Some(path) = self.cached_path(video_id).await {
            return Ok(path);
        }
        if let Some(last) = *last_finished {
            let gap = random_between(DOWNLOAD_GAP_MS.0, DOWNLOAD_GAP_MS.1);
            tokio::time::sleep_until(last + Duration::from_millis(gap)).await;
        }
        let started = Instant::now();
        let file_name = format!("{video_id}.m4a");
        let mut fetched = self.fetch(video_id, &file_name).await;
        // Out of disk space: make room by evicting cached Tracks nothing needs
        // right now, then try once more.
        if matches!(fetched, Err(YoutubeError::DiskFull)) && self.evict_everything_evictable().await
        {
            tracing::warn!(
                "disk full while downloading {video_id}; freed cache space and retrying"
            );
            fetched = self.fetch(video_id, &file_name).await;
        }
        *last_finished = Some(Instant::now());
        let byte_size = fetched?;
        tracing::info!(
            "downloaded {video_id} ({byte_size} bytes) in {:.1} s",
            started.elapsed().as_secs_f64()
        );
        let path = self.files.path(&file_name).map_err(YoutubeError::cache)?;
        let _guard = self.mutate().await;
        lock(&self.state).set_audio(video_id, file_name, byte_size);
        self.evict_locked(false).await;
        // The audio is safely on disk; a manifest write failure is reported but
        // doesn't fail the download (initialize would adopt the file anyway).
        self.persist_or_record(Some(video_id)).await;
        Ok(path)
    }

    /// Fetches the audio into `file_name` (atomically, through a `.part`
    /// file), returning its size.
    async fn fetch(&self, video_id: &str, file_name: &str) -> Result<u64, YoutubeError> {
        let attempt = async {
            let mut part = self
                .files
                .begin(file_name)
                .await
                .map_err(YoutubeError::cache_write)?;
            self.fetcher.fetch(video_id, &mut part).await?;
            part.finish().await.map_err(|error| {
                if error.kind() == std::io::ErrorKind::InvalidData {
                    YoutubeError::EmptyDownload
                } else {
                    YoutubeError::cache_write(error)
                }
            })
        };
        tokio::time::timeout(DOWNLOAD_TIMEOUT, attempt)
            .await
            .unwrap_or(Err(YoutubeError::DownloadTimedOut))
    }

    /// Deletes every cached file that isn't protected (Playing or in the
    /// Queue) or downloading, regardless of the cache size limit. Returns
    /// whether anything was deleted.
    async fn evict_everything_evictable(&self) -> bool {
        let _guard = self.mutate().await;
        let evicted = self.evict_locked(true).await;
        if evicted {
            self.persist_or_record(None).await;
        }
        evicted
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

    /// Waits for the manifest to be loaded, then takes the mutation lock.
    async fn mutate(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.ensure_loaded().await;
        self.mutation.lock().await
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
        let _guard = self.mutate().await;
        let (file_name, recorded_size) = {
            let state = lock(&self.state);
            let entry = state.entries.get(video_id)?;
            (entry.file_name.clone()?, entry.byte_size)
        };
        let size = self.audio_size(&file_name).await;
        if size != recorded_size {
            lock(&self.state).set_size(video_id, size);
            self.persist_or_record(Some(video_id)).await;
        }
        size?;
        self.files.path(&file_name).ok()
    }

    /// The size of a cached audio file that is on disk, not empty and
    /// playable. Anything else is deleted (Opus/WebM from the Electron app
    /// can't be played by our audio engine, which only plays AAC in MP4).
    async fn audio_size(&self, file_name: &str) -> Option<u64> {
        let size = if file_name.ends_with(".m4a") {
            self.files.size(file_name).await.filter(|size| *size > 0)
        } else {
            None
        };
        if size.is_none() {
            let _ = self.files.remove(file_name).await;
        }
        size
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
        let mut known = HashSet::new();
        for (video_id, file_name) in with_files {
            let size = self.audio_size(&file_name).await;
            lock(&self.state).set_size(&video_id, size);
            if size.is_some() {
                known.insert(file_name);
            }
        }
        for (file_name, size) in self.files.list_audio_files().await {
            if known.contains(&file_name) {
                continue;
            }
            let adoptable = file_name
                .strip_suffix(".m4a")
                .filter(|id| size > 0 && is_valid_video_id(id));
            match adoptable {
                Some(video_id) => lock(&self.state).set_audio(video_id, file_name.clone(), size),
                None => {
                    let _ = self.files.remove(&file_name).await;
                }
            }
        }
    }

    async fn trim_and_persist(&self) -> Result<(), YoutubeError> {
        let _guard = self.mutate().await;
        self.evict_locked(false).await;
        self.persist_locked().await
    }

    /// Deletes cached files (call with the mutation lock held): clear-requested
    /// ones, then others in eviction order until the cache fits, or with
    /// `everything` all of them. Protected and in-flight videos are never
    /// evicted. Returns whether anything was deleted.
    async fn evict_locked(&self, everything: bool) -> bool {
        let in_flight = self.in_flight();
        let (candidates, mut used, max) = {
            let state = lock(&self.state);
            (
                state.eviction_candidates(&in_flight),
                state.used_bytes(),
                state.max_bytes,
            )
        };
        let mut evicted = false;
        for candidate in candidates {
            if !everything && !candidate.clear_requested && used <= max {
                continue;
            }
            let Some(file_name) = candidate.file_name.as_deref() else {
                continue;
            };
            match self.files.remove(file_name).await {
                Ok(()) => {
                    evicted = true;
                    used = used.saturating_sub(candidate.byte_size.unwrap_or(0));
                    lock(&self.state).set_size(&candidate.video_id, None);
                }
                Err(error) => {
                    let error = YoutubeError::cache(error);
                    self.record("cache-remove", Some(&candidate.video_id), Some(&error));
                }
            }
        }
        evicted
    }

    /// Writes the manifest and notifies the app (call with the mutation lock held).
    async fn persist_locked(&self) -> Result<(), YoutubeError> {
        let json = lock(&self.state).manifest_json();
        let result = match json {
            Ok(json) => self.files.write_manifest(&json).await,
            Err(error) => Err(error.into()),
        };
        (self.on_cache_change)();
        result.map_err(YoutubeError::cache)
    }

    /// [`Self::persist_locked`], recording a failure in diagnostics.
    async fn persist_or_record(&self, video_id: Option<&str>) {
        if let Err(error) = self.persist_locked().await {
            self.record("cache-write", video_id, Some(&error));
        }
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
