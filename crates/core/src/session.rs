//! The playout Session: a pure state machine over the Queue.
//!
//! The Session never performs I/O. DJ commands and engine events go in; it
//! updates its state and queues [`Effect`]s (play this file, save recovery,
//! prepare a Track, ...) that the application collects with
//! [`Session::take_effects`] and executes. Results come back as more events. Every audio load gets a fresh [`LoadId`], so late events from
//! audio that has already been replaced are ignored instead of corrupting
//! the Queue.
//!
//! Safety rules (vocabulary in CONTEXT.md):
//! - only DJ-authored items play; an empty Queue never starts by itself;
//! - a Break blocks automatic advancement until it is skipped;
//! - crossfades happen only between adjacent Tracks, never across a Break;
//! - if the next Track is not ready in time, playback stops and says so;
//! - the Play Log records only items that actually began Playing.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};

use crate::domain::{
    DiagnosticEvent, PlayLogEntry, PlayLogReason, PlaybackProgress, QueueItem, RecoverySnapshot,
    RecoveryStatus, SourceId, Track, TrackKey,
};
pub use crate::queue::LoadId;
use crate::queue::{Direction, Playback, PlayingItem, Queue, QueueItemRef};

/// Identifies one Track preparation request.
pub type RequestId = u64;

#[derive(Clone, Debug, PartialEq)]
pub enum AudioCommand {
    /// Stop everything and play `path` at full volume.
    Play {
        id: LoadId,
        path: PathBuf,
    },
    /// Start `path` on the idle deck and crossfade into it.
    Crossfade {
        id: LoadId,
        path: PathBuf,
        duration_seconds: u32,
    },
    /// Stop the outgoing deck now if `incoming` is still the incoming deck.
    PromoteIncoming {
        incoming: LoadId,
    },
    Stop,
}

/// Notifications from the audio engine.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioEvent {
    Progress {
        id: LoadId,
        position_secs: f64,
        duration_secs: Option<f64>,
    },
    Ended {
        id: LoadId,
    },
    Failed {
        id: LoadId,
        message: String,
    },
    CrossfadeComplete {
        outgoing: LoadId,
        incoming: LoadId,
    },
}

/// Work the application performs on the Session's behalf.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Audio(AudioCommand),
    /// Resolve a Track to a playable local file; answer with [`Session::track_prepared`].
    Prepare {
        request: RequestId,
        track: Track,
    },
    SaveRecovery(RecoverySnapshot),
    DiscardRecovery,
    /// Insert or replace (by entry id) one Play Log entry.
    SavePlayLogEntry(PlayLogEntry),
    ClearPlayLog,
    /// Tracks in the Queue must not be evicted from the Download Cache.
    ProtectTracks(Vec<Track>),
    RecordYoutubePlay(String),
    Diagnostic(DiagnosticEvent),
}

/// Whether an Upcoming item is ready to play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    Preparing,
    Ready,
    Unavailable,
}

/// The audio file behind a Track, per Track (shared by repeated Queue items).
#[derive(Clone, Debug, PartialEq)]
enum Preparation {
    Pending(RequestId),
    Ready(PathBuf),
    Failed,
}

type Clock = Box<dyn Fn() -> DateTime<Utc> + Send>;
type IdSource = Box<dyn FnMut() -> String + Send>;

pub struct Session {
    queue: Queue,
    preparations: HashMap<TrackKey, Preparation>,
    play_log: Vec<PlayLogEntry>,
    playback_error: Option<String>,
    /// An interrupted Queue the DJ has not yet recovered or discarded.
    recovery: Option<RecoverySnapshot>,
    session_started_at: DateTime<Utc>,
    crossfade_seconds: u32,
    /// Source of load and request ids.
    next_id: u64,
    clock: Clock,
    ids: IdSource,
    effects: Vec<Effect>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new(
            Box::new(Utc::now),
            Box::new(|| uuid::Uuid::new_v4().to_string()),
        )
    }
}

fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Millis, true)
}

impl Session {
    pub fn new(clock: Clock, ids: IdSource) -> Self {
        let now = clock();
        Self {
            queue: Queue::default(),
            preparations: HashMap::new(),
            play_log: Vec::new(),
            playback_error: None,
            recovery: None,
            session_started_at: now,
            crossfade_seconds: 3,
            next_id: 1,
            clock,
            ids,
            effects: Vec::new(),
        }
    }

    /// The effects queued by the commands since the last call, in order.
    #[must_use]
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    // ---- Read access -------------------------------------------------------

    pub fn queue(&self) -> &Queue {
        &self.queue
    }

    /// Newest first.
    pub fn play_log(&self) -> &[PlayLogEntry] {
        &self.play_log
    }

    pub fn playback_error(&self) -> Option<&str> {
        self.playback_error.as_deref()
    }

    pub fn pending_recovery(&self) -> Option<&RecoverySnapshot> {
        self.recovery.as_ref()
    }

    pub fn session_started_at(&self) -> DateTime<Utc> {
        self.session_started_at
    }

    pub fn crossfade_seconds(&self) -> u32 {
        self.crossfade_seconds
    }

    /// Breaks are always ready; Playing items are ready by definition.
    pub fn readiness(&self, item: QueueItemRef<'_>) -> Readiness {
        let (QueueItemRef::Upcoming(_), Some(track)) = (item, item.track()) else {
            return Readiness::Ready;
        };
        match self.preparations.get(&track.key()) {
            Some(Preparation::Ready(_)) => Readiness::Ready,
            Some(Preparation::Failed) => Readiness::Unavailable,
            Some(Preparation::Pending(_)) | None => Readiness::Preparing,
        }
    }

    /// The Play Log entry of the item currently Playing as the Active Item.
    pub fn active_log_entry(&self) -> Option<&PlayLogEntry> {
        let active = self.queue.active()?;
        self.play_log
            .iter()
            .find(|entry| entry.item.id == active.item.id && entry.ended_at.is_none())
    }

    // ---- Queue editing -----------------------------------------------------

    pub fn add_track_to_end(&mut self, track: Track) {
        self.insert_track_at(track, usize::MAX);
    }

    pub fn add_track_next(&mut self, track: Track) {
        self.insert_track_at(track, 0);
    }

    pub fn insert_track_at(&mut self, track: Track, at: usize) {
        let item = QueueItem::track((self.ids)(), track);
        self.edit(|queue| queue.insert(at, item));
    }

    pub fn insert_break_at(&mut self, at: usize) {
        let item = QueueItem::new_break((self.ids)());
        self.edit(|queue| queue.insert(at, item));
    }

    pub fn insert_break_after(&mut self, after_id: &str) {
        if let Some(index) = self.queue.index_of(after_id) {
            self.insert_break_at(index + 1);
        }
    }

    /// "Add Break": right above `selected` (the selected item), else at the
    /// end of the Queue.
    pub fn insert_break(&mut self, selected: Option<&str>) {
        let at = selected.and_then(|id| self.queue.index_of(id));
        self.insert_break_at(at.unwrap_or(usize::MAX));
    }

    pub fn remove_items(&mut self, item_ids: &[String]) {
        self.edit(|queue| queue.remove(item_ids));
    }

    /// Moves these items together, in Queue order, to display slot `slot`.
    pub fn move_items(&mut self, item_ids: &[String], slot: usize) {
        self.edit(|queue| queue.move_to(item_ids, slot));
    }

    /// Moves each of these items one place, or none if any can't move.
    pub fn step_items(&mut self, item_ids: &[String], direction: Direction) {
        self.edit(|queue| queue.step(item_ids, direction));
    }

    // ---- Transport ---------------------------------------------------------

    /// "Play now" for an Upcoming item.
    pub fn play_item(&mut self, item_id: &str) {
        let Some(item) = self.queue.upcoming.iter().find(|item| item.id == item_id) else {
            return;
        };
        let readiness = self.readiness(QueueItemRef::Upcoming(item));
        if readiness == Readiness::Preparing {
            return;
        }
        let key = item.as_track().map(Track::key);
        self.playback_error = None;
        if let Some(key) = &key {
            self.retry_if_failed(key);
        }
        if key.is_some() && self.active_track_playing() {
            self.crossfade_to(item_id);
        } else {
            self.restart_from(item_id);
        }
    }

    /// Skip the Active Item (crossfading into the next Track when adjacent).
    pub fn skip(&mut self) {
        match &self.queue.playback {
            Playback::Stopped { .. } => {}
            Playback::Crossfading { .. } => self.promote_incoming(PlayLogReason::Skipped, true),
            Playback::Playing { .. } => {
                let next = self.queue.upcoming.first();
                let next_track =
                    next.and_then(|next| Some((next.id.clone(), next.as_track()?.key())));
                match next_track {
                    Some((next_id, key)) if self.active_track_playing() => {
                        self.retry_if_failed(&key);
                        self.playback_error = None;
                        self.crossfade_to(&next_id);
                    }
                    _ => {
                        self.audio(AudioCommand::Stop);
                        self.advance(PlayLogReason::Skipped);
                    }
                }
            }
        }
    }

    // ---- Engine and service results ----------------------------------------

    pub fn audio_event(&mut self, event: AudioEvent) {
        match event {
            AudioEvent::Progress {
                id,
                position_secs,
                duration_secs,
            } => self.on_progress(id, position_secs, duration_secs),
            AudioEvent::Ended { id } => match self.playing_with_load(id) {
                // The engine already promoted its incoming deck, if any.
                Some(QueueItemRef::Active(_)) => {
                    self.promote_incoming(PlayLogReason::Completed, false);
                }
                Some(QueueItemRef::Incoming(_)) => self.incoming_ended(),
                _ => {}
            },
            AudioEvent::Failed { id, message } => {
                if let Some(item) = self.playing_with_load(id) {
                    let item_id = item.id().to_owned();
                    self.fail_playback(message, Some(&item_id));
                }
            }
            AudioEvent::CrossfadeComplete { outgoing, incoming } => {
                if let Playback::Crossfading {
                    active,
                    incoming: playing,
                    manual: true,
                } = &self.queue.playback
                    && active.load == Some(outgoing)
                    && playing.load == Some(incoming)
                {
                    self.promote_incoming(PlayLogReason::Skipped, true);
                }
            }
        }
    }

    /// Answer to [`Effect::Prepare`].
    pub fn track_prepared(
        &mut self,
        request: RequestId,
        key: &TrackKey,
        result: Result<PathBuf, String>,
    ) {
        if self.preparations.get(key) != Some(&Preparation::Pending(request)) {
            return;
        }
        match result {
            Ok(path) => {
                self.preparations
                    .insert(key.clone(), Preparation::Ready(path));
                self.resume_pending();
            }
            Err(message) => {
                self.preparations.insert(key.clone(), Preparation::Failed);
                let pending = self.pending_item().filter(|id| {
                    self.queue
                        .item(id)
                        .and_then(QueueItemRef::track)
                        .is_some_and(|track| &track.key() == key)
                });
                if let Some(item_id) = pending.map(str::to_owned) {
                    self.fail_playback(message, Some(&item_id));
                }
            }
        }
    }

    /// The local Library changed: local Tracks must be resolved again.
    pub fn library_changed(&mut self) {
        self.preparations
            .retain(|key, _| key.source != SourceId::Local);
        self.prepare_upcoming();
    }

    /// Persistence or another service failed; show it to the DJ.
    pub fn report_error(&mut self, message: impl Into<String>) {
        self.playback_error = Some(message.into());
    }

    pub fn set_crossfade_seconds(&mut self, seconds: u32) {
        self.crossfade_seconds = seconds;
    }

    // ---- Recovery and Play Log ---------------------------------------------

    /// Startup: the result of reading the recovery file.
    pub fn recovery_loaded(&mut self, result: Result<Option<RecoverySnapshot>, String>) {
        match result {
            Ok(snapshot) => {
                self.recovery = snapshot.filter(|snapshot| {
                    snapshot.status == RecoveryStatus::Interrupted && !snapshot.queue.is_empty()
                });
            }
            Err(message) => {
                self.playback_error = Some(format!("Queue recovery could not be read: {message}"));
            }
        }
    }

    pub fn restore_recovery(&mut self) {
        let Some(snapshot) = self.recovery.take() else {
            return;
        };
        if let Some(started) = snapshot
            .session_started_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        {
            self.session_started_at = started.with_timezone(&Utc);
        }
        self.queue = Queue::stopped(snapshot.queue);
        self.queue_changed();
    }

    /// "Start Empty": a new Session with an empty Queue.
    pub fn discard_recovery(&mut self) {
        self.recovery = None;
        self.effects.push(Effect::DiscardRecovery);
        self.effects.push(Effect::ProtectTracks(Vec::new()));
        self.queue = Queue::default();
        self.session_started_at = (self.clock)();
    }

    /// Startup: the stored Play Log, newest first.
    pub fn play_log_loaded(&mut self, result: Result<Vec<PlayLogEntry>, String>) {
        match result {
            Ok(entries) => self.play_log = entries,
            Err(message) => {
                self.playback_error = Some(format!("Play log could not be read: {message}"));
            }
        }
    }

    pub fn clear_play_log(&mut self) {
        self.play_log.clear();
        self.effects.push(Effect::ClearPlayLog);
    }

    /// Stops audio, closes Playing log entries, and saves a clean snapshot.
    pub fn shutdown(&mut self) {
        self.stop_and_requeue(PlayLogReason::Skipped);
        let snapshot = self.snapshot(RecoveryStatus::Clean);
        self.effects.push(Effect::SaveRecovery(snapshot));
    }

    // ---- Internals ---------------------------------------------------------

    fn audio(&mut self, command: AudioCommand) {
        self.effects.push(Effect::Audio(command));
    }

    fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn now(&self) -> String {
        timestamp((self.clock)())
    }

    fn snapshot(&self, status: RecoveryStatus) -> RecoverySnapshot {
        RecoverySnapshot {
            version: 1,
            status,
            queue: self.queue.stored_items(),
            session_started_at: Some(timestamp(self.session_started_at)),
            updated_at: self.now(),
        }
    }

    fn edit(&mut self, edit: impl FnOnce(&mut Queue)) {
        let before = self.queue.upcoming.clone();
        edit(&mut self.queue);
        if self.queue.upcoming != before {
            self.queue_changed();
        }
    }

    /// Every structural Queue change ends here: protect, save, prepare.
    fn queue_changed(&mut self) {
        let tracks: Vec<Track> = self.queue.tracks().cloned().collect();
        let keep: HashSet<TrackKey> = tracks.iter().map(Track::key).collect();
        self.preparations.retain(|key, _| keep.contains(key));
        self.effects.push(Effect::ProtectTracks(tracks));
        let snapshot = self.snapshot(RecoveryStatus::Interrupted);
        self.effects.push(Effect::SaveRecovery(snapshot));
        self.prepare_upcoming();
    }

    /// Requests every Upcoming Track that has no preparation yet.
    fn prepare_upcoming(&mut self) {
        for track in self.queue.upcoming.iter().filter_map(QueueItem::as_track) {
            let key = track.key();
            if self.preparations.contains_key(&key) {
                continue;
            }
            let request = self.next_id;
            self.next_id += 1;
            self.preparations.insert(key, Preparation::Pending(request));
            self.effects.push(Effect::Prepare {
                request,
                track: track.clone(),
            });
        }
    }

    /// Re-resolves a Track whose preparation failed, rather than giving up.
    fn retry_if_failed(&mut self, key: &TrackKey) {
        if self.preparations.get(key) == Some(&Preparation::Failed) {
            self.preparations.remove(key);
            self.prepare_upcoming();
        }
    }

    fn ready_path(&self, item_id: &str) -> Option<PathBuf> {
        let track = self.queue.item(item_id)?.track()?;
        match self.preparations.get(&track.key()) {
            Some(Preparation::Ready(path)) => Some(path.clone()),
            _ => None,
        }
    }

    fn active_track_playing(&self) -> bool {
        matches!(&self.queue.playback, Playback::Playing { active, .. } if active.load.is_some())
    }

    fn playing_with_load(&self, load: LoadId) -> Option<QueueItemRef<'_>> {
        self.queue.items().find(|item| match item {
            QueueItemRef::Active(playing) | QueueItemRef::Incoming(playing) => {
                playing.load == Some(load)
            }
            QueueItemRef::Upcoming(_) => false,
        })
    }

    /// The Upcoming item a start or manual crossfade is waiting for.
    fn pending_item(&self) -> Option<&str> {
        match &self.queue.playback {
            Playback::Stopped { starting: pending }
            | Playback::Playing {
                crossfade_to: pending,
                ..
            } => pending.as_deref(),
            Playback::Crossfading { .. } => None,
        }
    }

    /// Ends playback: closes the Playing items' log entries and stops the
    /// engine. Returns the items that were Playing.
    fn stop(&mut self, reason: PlayLogReason) -> Vec<QueueItem> {
        let playing = match std::mem::take(&mut self.queue.playback) {
            Playback::Stopped { .. } => vec![],
            Playback::Playing { active, .. } => vec![active],
            Playback::Crossfading {
                active, incoming, ..
            } => vec![active, incoming],
        };
        for item in &playing {
            self.finish_log(&item.item.id, reason);
        }
        self.audio(AudioCommand::Stop);
        playing.into_iter().map(|playing| playing.item).collect()
    }

    /// Stops, putting the Playing items back in front of the Upcoming ones.
    fn stop_and_requeue(&mut self, reason: PlayLogReason) {
        let items = self.stop(reason);
        self.queue.upcoming.splice(0..0, items);
    }

    /// Stops everything and starts `item_id`, dropping the items before it.
    fn restart_from(&mut self, item_id: &str) {
        let Some(index) = self
            .queue
            .upcoming
            .iter()
            .position(|item| item.id == item_id)
        else {
            return;
        };
        self.stop(PlayLogReason::Skipped);
        self.queue.upcoming.drain(..index);
        self.queue_changed();
        self.start_item(item_id);
    }

    /// Starts an Upcoming item as the Active Item (stopped Queue assumed),
    /// or waits for its Track to be prepared.
    fn start_item(&mut self, item_id: &str) {
        let Some(item) = self.queue.upcoming.iter().find(|item| item.id == item_id) else {
            return;
        };
        let Some(key) = item.as_track().map(Track::key) else {
            self.audio(AudioCommand::Stop);
            self.become_active(item_id, None);
            return;
        };
        match self.ready_path(item_id) {
            Some(path) => {
                let load = self.new_id();
                self.audio(AudioCommand::Play { id: load, path });
                self.become_active(item_id, Some(load));
            }
            None => {
                self.retry_if_failed(&key);
                self.queue.playback = Playback::Stopped {
                    starting: Some(item_id.to_owned()),
                };
            }
        }
    }

    fn become_active(&mut self, item_id: &str, load: Option<LoadId>) {
        let Some(position) = self
            .queue
            .upcoming
            .iter()
            .position(|item| item.id == item_id)
        else {
            return;
        };
        let item = self.queue.upcoming.remove(position);
        self.queue.playback = Playback::playing(PlayingItem::new(item, load));
        self.start_log(item_id);
    }

    /// Manual crossfade from the active Track into `item_id`, which moves to
    /// the front and starts once it is prepared.
    fn crossfade_to(&mut self, item_id: &str) {
        let Some(position) = self
            .queue
            .upcoming
            .iter()
            .position(|item| item.id == item_id)
        else {
            return;
        };
        let Playback::Playing { crossfade_to, .. } = &mut self.queue.playback else {
            return;
        };
        *crossfade_to = Some(item_id.to_owned());
        let target = self.queue.upcoming.remove(position);
        self.queue.upcoming.insert(0, target);
        self.queue_changed();
        self.resume_pending();
    }

    /// Starts the pending start or crossfade once its Track is ready, and
    /// forgets it if its item was removed.
    fn resume_pending(&mut self) {
        let Some(item_id) = self.pending_item().map(str::to_owned) else {
            return;
        };
        match self.ready_path(&item_id) {
            Some(path) if self.queue.is_stopped() => {
                let load = self.new_id();
                self.audio(AudioCommand::Play { id: load, path });
                self.become_active(&item_id, Some(load));
            }
            Some(path) => self.begin_crossfade(&item_id, path, true),
            None if self.queue.item(&item_id).is_none() => {
                if let Playback::Stopped { starting: pending }
                | Playback::Playing {
                    crossfade_to: pending,
                    ..
                } = &mut self.queue.playback
                {
                    *pending = None;
                }
            }
            None => {}
        }
    }

    fn begin_crossfade(&mut self, item_id: &str, path: PathBuf, manual: bool) {
        let Some(position) = self
            .queue
            .upcoming
            .iter()
            .position(|item| item.id == item_id)
        else {
            return;
        };
        let Playback::Playing { active, .. } = &self.queue.playback else {
            return;
        };
        let active = active.clone();
        let incoming = self.queue.upcoming.remove(position);
        let load = self.new_id();
        self.audio(AudioCommand::Crossfade {
            id: load,
            path,
            duration_seconds: self.crossfade_seconds,
        });
        self.queue.playback = Playback::Crossfading {
            active,
            incoming: PlayingItem::new(incoming, Some(load)),
            manual,
        };
        self.start_log(item_id);
    }

    fn on_progress(&mut self, load: LoadId, position: f64, duration: Option<f64>) {
        let progress = PlaybackProgress {
            current_time: position,
            duration: duration
                .filter(|value| value.is_finite() && *value > 0.0)
                .unwrap_or(0.0),
        };
        let crossfade = f64::from(self.crossfade_seconds);
        let near_end = crossfade > 0.0
            && progress.duration > 0.0
            && position > 0.0
            && progress.duration - position <= crossfade;
        let mut try_auto_crossfade = false;
        match &mut self.queue.playback {
            Playback::Stopped { .. } => {}
            Playback::Playing {
                active,
                crossfade_to,
                auto_crossfade_tried,
            } => {
                if active.load == Some(load) {
                    active.progress = progress;
                    if near_end && !*auto_crossfade_tried {
                        *auto_crossfade_tried = true;
                        try_auto_crossfade = crossfade_to.is_none();
                    }
                }
            }
            Playback::Crossfading {
                active, incoming, ..
            } => {
                for item in [active, incoming] {
                    if item.load == Some(load) {
                        item.progress = progress;
                    }
                }
            }
        }
        if try_auto_crossfade {
            self.auto_crossfade();
        }
    }

    /// Crossfades into the next item if it is an adjacent, ready Track.
    fn auto_crossfade(&mut self) {
        let Some(next) = self.queue.upcoming.first() else {
            return;
        };
        let next_id = next.id.clone();
        if let Some(path) = self.ready_path(&next_id) {
            self.begin_crossfade(&next_id, path, false);
        }
    }

    /// The outgoing Track leaves; the Incoming Track becomes the Active Item.
    fn promote_incoming(&mut self, reason: PlayLogReason, tell_engine: bool) {
        let Playback::Crossfading {
            active, incoming, ..
        } = &self.queue.playback
        else {
            self.advance(reason);
            return;
        };
        let (active_id, incoming) = (active.item.id.clone(), incoming.clone());
        if tell_engine && let Some(load) = incoming.load {
            self.audio(AudioCommand::PromoteIncoming { incoming: load });
        }
        self.finish_log(&active_id, reason);
        self.queue.playback = Playback::playing(incoming);
        self.queue_changed();
    }

    /// The Incoming Track ended before the outgoing one (a very short Track).
    fn incoming_ended(&mut self) {
        let Playback::Crossfading {
            active, incoming, ..
        } = &self.queue.playback
        else {
            return;
        };
        let (active, incoming_id) = (active.clone(), incoming.item.id.clone());
        self.finish_log(&incoming_id, PlayLogReason::Completed);
        self.queue.playback = Playback::playing(active);
        self.queue_changed();
    }

    /// The Active Item left: start the next item, stopping at a Break or an unready Track.
    fn advance(&mut self, reason: PlayLogReason) {
        if let Some(active) = self.queue.active() {
            let active_id = active.item.id.clone();
            self.finish_log(&active_id, reason);
        }
        if let Playback::Crossfading { incoming, .. } = std::mem::take(&mut self.queue.playback) {
            self.queue.upcoming.insert(0, incoming.item);
        }
        self.queue_changed();
        let Some(next) = self.queue.upcoming.first() else {
            return;
        };
        let next_id = next.id.clone();
        if let Some(track) = next.as_track()
            && self.ready_path(&next_id).is_none()
        {
            self.playback_error = Some(format!(
                "{} was not ready when the previous track ended.",
                track.title
            ));
            return;
        }
        self.start_item(&next_id);
    }

    /// Stops all audio and reports `message`; the affected Track is marked unavailable.
    fn fail_playback(&mut self, message: String, affected_item: Option<&str>) {
        self.stop_and_requeue(PlayLogReason::Error);
        let track = affected_item
            .and_then(|id| self.queue.item(id))
            .and_then(QueueItemRef::track)
            .cloned();
        if let Some(track) = track {
            self.preparations.insert(track.key(), Preparation::Failed);
            let mut event = DiagnosticEvent::new("playback")
                .source(track.source)
                .error(&message);
            if track.source == SourceId::YoutubeMusic {
                event = event.video_id(&track.id);
            }
            self.effects.push(Effect::Diagnostic(event));
        }
        self.playback_error = Some(message);
    }

    fn start_log(&mut self, item_id: &str) {
        let Some(item) = self.queue.item(item_id) else {
            return;
        };
        let item = item.item().clone();
        if let Some(track) = item.as_track()
            && track.source == SourceId::YoutubeMusic
        {
            self.effects
                .push(Effect::RecordYoutubePlay(track.id.clone()));
        }
        let entry = PlayLogEntry {
            id: (self.ids)(),
            item,
            started_at: self.now(),
            ended_at: None,
            reason: None,
        };
        self.effects.push(Effect::SavePlayLogEntry(entry.clone()));
        self.play_log.insert(0, entry);
    }

    fn finish_log(&mut self, item_id: &str, reason: PlayLogReason) {
        let now = self.now();
        let Some(entry) = self
            .play_log
            .iter_mut()
            .find(|entry| entry.item.id == item_id)
        else {
            return;
        };
        if entry.ended_at.is_some() {
            return;
        }
        entry.ended_at = Some(now);
        entry.reason = Some(reason);
        let finished = entry.clone();
        self.effects.push(Effect::SavePlayLogEntry(finished));
    }
}

#[cfg(test)]
mod tests;
