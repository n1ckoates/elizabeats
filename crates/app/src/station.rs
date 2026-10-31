//! The Station: the app's single source of truth on the UI thread.
//!
//! It owns the [`Session`] and executes the Session's effects against the real
//! services (audio engine, Library, YouTube, persistence). Background work
//! reports back through one event channel that is drained on the UI thread,
//! so all state changes happen in one place, in order.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use elizabeats_audio::AudioEngine;
use elizabeats_core::session::{AudioCommand, AudioEvent, Effect, RequestId, Session};
use elizabeats_core::{
    AppSettings, DiagnosticEvent, DiagnosticSink, DownloadCacheStatus, QueueItem, SourceId, Track,
    TrackKey,
};
use elizabeats_library::LocalLibrary;
use elizabeats_youtube::YoutubeMusic;
use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender};
use gpui_kit::{Context, Task};

use crate::persist::Persistence;

/// Everything background work can tell the UI thread.
pub enum StationEvent {
    Audio(AudioEvent),
    Prepared {
        request: RequestId,
        key: TrackKey,
        result: Result<PathBuf, String>,
    },
    LibraryChanged,
    CacheChanged,
    Error(String),
}

/// Download Cache operations that must apply in order.
enum CacheOp {
    Protect(Vec<String>),
    RecordPlay(String),
    SetMaxBytes(u64),
}

/// Long-lived services shared with background tasks.
pub struct Services {
    pub runtime: tokio::runtime::Runtime,
    pub data_dir: PathBuf,
    pub audio: AudioEngine,
    pub library: Arc<LocalLibrary>,
    pub youtube: Arc<YoutubeMusic>,
    pub diagnostics: elizabeats_store::DiagnosticsStore,
    pub persistence: Persistence,
    pub http: reqwest::Client,
}

pub struct Station {
    session: Session,
    services: Arc<Services>,
    events: UnboundedSender<StationEvent>,
    cache_ops: tokio::sync::mpsc::UnboundedSender<CacheOp>,
    settings: AppSettings,
    cache_status: DownloadCacheStatus,
    cached_video_ids: Arc<HashSet<String>>,
    /// Bumped whenever the local Library index changes (search re-runs on it).
    library_revision: u64,
    shutting_down: bool,
    /// When the audio engine last reported playback progress.
    progress_at: Option<std::time::Instant>,
    _event_loop: Task<()>,
}

/// Inputs for building a [`Station`].
pub struct StationParts {
    pub services: Arc<Services>,
    pub settings: AppSettings,
    pub events: (
        UnboundedSender<StationEvent>,
        UnboundedReceiver<StationEvent>,
    ),
}

impl Station {
    pub fn new(parts: StationParts, cx: &mut Context<Self>) -> Self {
        let StationParts {
            services,
            settings,
            events: (events, mut receiver),
        } = parts;
        let event_loop = cx.spawn(async move |this, cx| {
            while let Some(event) = receiver.next().await {
                if this
                    .update(cx, |this, cx| this.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });

        let cache_ops = spawn_cache_worker(&services);
        let mut session = Session::default();
        session.set_crossfade_seconds(settings.crossfade_duration_seconds);

        // Startup reads are small local files; doing them now means the first
        // frame already knows whether to offer recovery.
        session.recovery_loaded(
            elizabeats_store::read_recovery(&services.data_dir).map_err(|error| error.to_string()),
        );
        let protected = youtube_ids(
            session
                .pending_recovery()
                .into_iter()
                .flat_map(|snapshot| snapshot.queue.iter().filter_map(QueueItem::as_track)),
        );
        session.play_log_loaded(
            elizabeats_store::read_play_log(&services.data_dir).map_err(|error| error.to_string()),
        );

        let youtube = services.youtube.clone();
        let max_bytes = settings.cache_bytes();
        let errors = events.clone();
        services.runtime.spawn(async move {
            if let Err(error) = youtube.initialize(max_bytes, protected).await {
                let _ = errors.unbounded_send(StationEvent::Error(format!(
                    "Download cache could not be loaded: {error}"
                )));
            }
        });

        let mut station = Self {
            session,
            cache_status: services.youtube.cache_status(),
            cached_video_ids: Arc::new(services.youtube.cached_video_ids().into_iter().collect()),
            services,
            events,
            cache_ops,
            settings,
            library_revision: 0,
            shutting_down: false,
            progress_at: None,
            _event_loop: event_loop,
        };
        station.apply_settings_side_effects(None);
        station
    }

    // ---- Read access for views ---------------------------------------------

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn settings(&self) -> &AppSettings {
        &self.settings
    }

    pub fn services(&self) -> &Arc<Services> {
        &self.services
    }

    /// Seconds played since the engine's last progress report (~every 100 ms),
    /// so the UI can move progress smoothly between reports. Capped so a
    /// stalled engine never makes the display run ahead.
    pub fn progress_extrapolation(&self) -> f64 {
        if self.session.queue().is_stopped() {
            return 0.0;
        }
        self.progress_at
            .map_or(0.0, |at| at.elapsed().as_secs_f64().min(0.25))
    }

    pub fn cache_status(&self) -> DownloadCacheStatus {
        self.cache_status
    }

    pub fn cached_video_ids(&self) -> Arc<HashSet<String>> {
        self.cached_video_ids.clone()
    }

    pub fn library_revision(&self) -> u64 {
        self.library_revision
    }

    // ---- Session commands --------------------------------------------------

    /// Runs a Session command and executes its effects.
    pub fn command(&mut self, cx: &mut Context<Self>, run: impl FnOnce(&mut Session)) {
        if self.shutting_down {
            return;
        }
        run(&mut self.session);
        self.execute();
        cx.notify();
    }

    // ---- Settings ----------------------------------------------------------

    /// Saves new settings; resolves with a user-facing error on failure.
    pub fn update_settings(
        &mut self,
        next: AppSettings,
        cx: &mut Context<Self>,
    ) -> futures::channel::oneshot::Receiver<Result<(), String>> {
        let previous = std::mem::replace(&mut self.settings, next.clone());
        self.apply_settings_side_effects(Some(&previous));
        cx.notify();
        self.services.persistence.save_settings(next)
    }

    fn apply_settings_side_effects(&mut self, previous: Option<&AppSettings>) {
        let settings = self.settings.clone();
        self.session
            .set_crossfade_seconds(settings.crossfade_duration_seconds);
        if previous.map(|p| &p.library_folder) != Some(&settings.library_folder) {
            self.services
                .library
                .set_library_folder(settings.library_folder.as_ref().map(PathBuf::from));
        }
        if let Some(previous) = previous
            && previous.download_cache_size_gib != settings.download_cache_size_gib
        {
            let _ = self
                .cache_ops
                .send(CacheOp::SetMaxBytes(settings.cache_bytes()));
        }
    }

    /// Resolves once the Download Cache has been cleared (or failed to).
    pub fn clear_download_cache(
        &self,
    ) -> impl std::future::Future<Output = Result<(), String>> + use<> {
        let youtube = self.services.youtube.clone();
        let handle = self.services.runtime.spawn(async move {
            youtube
                .clear_download_cache()
                .await
                .map_err(|error| error.to_string())
        });
        async move { handle.await.map_err(|error| error.to_string())? }
    }

    pub fn clear_play_log(
        &mut self,
        cx: &mut Context<Self>,
    ) -> futures::channel::oneshot::Receiver<Result<(), String>> {
        self.session.clear_play_log();
        cx.notify();
        self.services.persistence.clear_play_log()
    }

    // ---- Shutdown ----------------------------------------------------------

    pub fn needs_close_confirmation(&self) -> bool {
        !self.shutting_down && !self.session.queue().is_stopped()
    }

    /// Stops playback, saves a clean snapshot, and flushes every writer.
    /// Returns `false` when something could not be confirmed as saved.
    pub fn shutdown(&mut self) -> bool {
        if self.shutting_down {
            return true;
        }
        self.shutting_down = true;
        self.session.shutdown();
        self.execute();
        self.services.audio.stop();
        let saved = self
            .services
            .persistence
            .flush_blocking(Duration::from_secs(5));
        let youtube = self.services.youtube.clone();
        let _ = self.services.runtime.block_on(async move {
            tokio::time::timeout(Duration::from_secs(3), youtube.flush()).await
        });
        self.services.diagnostics.flush();
        saved
    }

    // ---- Event handling ----------------------------------------------------

    fn handle_event(&mut self, event: StationEvent, cx: &mut Context<Self>) {
        if self.shutting_down {
            return;
        }
        match event {
            StationEvent::Audio(event) => {
                let is_progress = matches!(event, AudioEvent::Progress { .. });
                self.session.audio_event(event);
                self.execute();
                if is_progress {
                    self.progress_at = Some(std::time::Instant::now());
                    // Progress only changes numbers in the header and the active row.
                    cx.notify();
                    return;
                }
            }
            StationEvent::Prepared {
                request,
                key,
                result,
            } => {
                self.session.track_prepared(request, &key, result);
                self.execute();
            }
            StationEvent::LibraryChanged => {
                self.library_revision += 1;
                self.session.library_changed();
                self.execute();
            }
            StationEvent::CacheChanged => {
                self.cache_status = self.services.youtube.cache_status();
                self.cached_video_ids = Arc::new(
                    self.services
                        .youtube
                        .cached_video_ids()
                        .into_iter()
                        .collect(),
                );
            }
            StationEvent::Error(message) => self.session.report_error(message),
        }
        cx.notify();
    }

    fn execute(&mut self) {
        for effect in self.session.take_effects() {
            match effect {
                Effect::Audio(command) => self.audio(command),
                Effect::Prepare { request, track } => self.prepare(request, track),
                Effect::SaveRecovery(snapshot) => self.services.persistence.save_recovery(snapshot),
                Effect::DiscardRecovery => self.services.persistence.discard_recovery(),
                Effect::SavePlayLogEntry(entry) => {
                    self.services.persistence.save_play_log_entry(entry)
                }
                Effect::ProtectTracks(tracks) => {
                    let ids = youtube_ids(tracks.iter());
                    let _ = self.cache_ops.send(CacheOp::Protect(ids));
                }
                Effect::RecordYoutubePlay(video_id) => {
                    let _ = self.cache_ops.send(CacheOp::RecordPlay(video_id));
                }
                Effect::Diagnostic(event) => self.services.diagnostics.record(event),
            }
        }
    }

    fn audio(&self, command: AudioCommand) {
        let audio = &self.services.audio;
        match command {
            AudioCommand::Play { id, path } => audio.play(id, path),
            AudioCommand::Crossfade {
                id,
                path,
                duration_seconds,
            } => audio.crossfade(id, path, Duration::from_secs(u64::from(duration_seconds))),
            AudioCommand::PromoteIncoming { incoming } => audio.promote_incoming(incoming),
            AudioCommand::Stop => audio.stop(),
        }
    }

    fn prepare(&self, request: RequestId, track: Track) {
        let key = track.key();
        let events = self.events.clone();
        match track.source {
            SourceId::Local => {
                let result = self.services.library.resolve(&track.id);
                let _ = events.unbounded_send(StationEvent::Prepared {
                    request,
                    key,
                    result,
                });
            }
            SourceId::YoutubeMusic => {
                let youtube = self.services.youtube.clone();
                self.services.runtime.spawn(async move {
                    let result = youtube
                        .resolve(&track.id)
                        .await
                        .map_err(|error| error.to_string());
                    let _ = events.unbounded_send(StationEvent::Prepared {
                        request,
                        key,
                        result,
                    });
                });
            }
        }
    }
}

/// Applies Download Cache operations one at a time, in request order.
fn spawn_cache_worker(services: &Arc<Services>) -> tokio::sync::mpsc::UnboundedSender<CacheOp> {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<CacheOp>();
    let youtube = services.youtube.clone();
    let diagnostics: DiagnosticSink = services.diagnostics.sink();
    services.runtime.spawn(async move {
        while let Some(op) = receiver.recv().await {
            let result = match op {
                CacheOp::Protect(ids) => youtube.set_protected_video_ids(ids).await,
                CacheOp::RecordPlay(video_id) => youtube.record_play(&video_id).await,
                CacheOp::SetMaxBytes(bytes) => youtube.set_max_cache_bytes(bytes).await,
            };
            if let Err(error) = result {
                diagnostics(
                    DiagnosticEvent::new("cache-update")
                        .source(SourceId::YoutubeMusic)
                        .error(error.to_string()),
                );
            }
        }
    });
    sender
}

/// The video ids of the YouTube Tracks among `tracks`.
fn youtube_ids<'a>(tracks: impl Iterator<Item = &'a Track>) -> Vec<String> {
    tracks
        .filter(|track| track.source == SourceId::YoutubeMusic)
        .map(|track| track.id.clone())
        .collect()
}
