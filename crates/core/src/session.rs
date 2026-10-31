//! The playout Session: a pure state machine over the Queue.
//!
//! The Session never performs I/O. DJ commands and engine events go in; it
//! updates its state and returns [`Effect`]s (play this file, save recovery,
//! prepare a Track, ...) that the application executes. Results come back as
//! more events. Every audio load gets a fresh [`LoadId`], so late events from
//! audio that has already been replaced are ignored instead of corrupting
//! the Queue.
//!
//! Safety rules (see README "Queue and playback invariants"):
//! - only DJ-authored items play; an empty Queue never starts by itself;
//! - a Break blocks automatic advancement until it is skipped;
//! - crossfades happen only between adjacent Tracks, never across a Break;
//! - if the next Track is not ready in time, playback stops and says so;
//! - the Play Log records only items that actually began Playing.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, SecondsFormat, Utc};

use crate::domain::{
    DiagnosticEvent, PlayLogEntry, PlayLogReason, PlaybackProgress, RecoverySnapshot,
    RecoveryStatus, SourceId, StoredQueueItem, Track, TrackKey,
};
use crate::queue::{
    Direction, ItemKind, PlayingItem, Queue, QueueCommand, QueueItemRef, Readiness, UpcomingItem,
    edit_queue,
};

/// Identifies one file loaded into the audio engine.
pub type LoadId = u64;

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

/// Engine notifications, mirrored from the audio crate.
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

#[derive(Clone, Debug, PartialEq)]
pub enum RecoveryState {
    Checking,
    None,
    Pending(RecoverySnapshot),
}

#[derive(Clone, Debug, PartialEq)]
enum Preparation {
    Pending(RequestId),
    Ready(PathBuf),
}

/// A start that is waiting for a Track to finish preparing.
#[derive(Clone, Debug, PartialEq)]
enum PendingStart {
    /// Start `item_id` from a stopped Queue.
    Play { item_id: String },
    /// Crossfade from `active_id` into `item_id` (a manual skip / play now).
    Crossfade { active_id: String, item_id: String },
}

impl PendingStart {
    fn item_id(&self) -> &str {
        match self {
            Self::Play { item_id } | Self::Crossfade { item_id, .. } => item_id,
        }
    }
}

type Clock = Box<dyn Fn() -> DateTime<Utc> + Send>;
type IdSource = Box<dyn FnMut() -> String + Send>;

pub struct Session {
    queue: Queue,
    play_log: Vec<PlayLogEntry>,
    playback_error: Option<String>,
    recovery: RecoveryState,
    session_started_at: DateTime<Utc>,
    crossfade_seconds: u32,

    preparations: HashMap<TrackKey, Preparation>,
    pending: Option<PendingStart>,
    active_load: Option<LoadId>,
    incoming_load: Option<LoadId>,
    /// The active Track has already asked for its automatic crossfade.
    crossfade_requested: bool,
    /// The running crossfade was manual: promote as soon as the fade completes.
    complete_after_fade: bool,

    next_load: LoadId,
    next_request: RequestId,
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

fn requeue(item: QueueItemRef<'_>) -> UpcomingItem {
    match item {
        QueueItemRef::Upcoming(item) => item.clone(),
        QueueItemRef::Active(item) | QueueItemRef::Incoming(item) => UpcomingItem {
            id: item.id.clone(),
            kind: item.kind.clone(),
            readiness: Readiness::Ready,
        },
    }
}

fn playing(item: UpcomingItem) -> PlayingItem {
    let duration = match &item.kind {
        ItemKind::Track(track) => track.duration_seconds.unwrap_or(0.0),
        ItemKind::Break => 0.0,
    };
    PlayingItem {
        id: item.id,
        kind: item.kind,
        progress: PlaybackProgress {
            current_time: 0.0,
            duration,
        },
    }
}

impl Session {
    pub fn new(clock: Clock, ids: IdSource) -> Self {
        let now = clock();
        Self {
            queue: Queue::default(),
            play_log: Vec::new(),
            playback_error: None,
            recovery: RecoveryState::Checking,
            session_started_at: now,
            crossfade_seconds: 3,
            preparations: HashMap::new(),
            pending: None,
            active_load: None,
            incoming_load: None,
            crossfade_requested: false,
            complete_after_fade: false,
            next_load: 1,
            next_request: 1,
            clock,
            ids,
            effects: Vec::new(),
        }
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

    pub fn recovery(&self) -> &RecoveryState {
        &self.recovery
    }

    pub fn session_started_at(&self) -> DateTime<Utc> {
        self.session_started_at
    }

    pub fn crossfade_seconds(&self) -> u32 {
        self.crossfade_seconds
    }

    /// Closing while audio plays needs the DJ's confirmation.
    pub fn needs_close_confirmation(&self) -> bool {
        !self.queue.is_stopped()
    }

    /// The Play Log entry of the item currently Playing as the Active Item.
    pub fn active_log_entry(&self) -> Option<&PlayLogEntry> {
        let active = self.queue.active()?;
        self.play_log
            .iter()
            .find(|entry| entry.item.id() == active.id && entry.ended_at.is_none())
    }

    // ---- Queue editing -----------------------------------------------------

    #[must_use]
    pub fn add_track_to_end(&mut self, track: Track) -> Vec<Effect> {
        let at = self.queue.len();
        self.insert_track_at(track, at, None)
    }

    #[must_use]
    pub fn add_track_next(&mut self, track: Track) -> Vec<Effect> {
        let at = self.queue.first_mutable_index();
        self.insert_track_at(track, at, None)
    }

    /// `item_id` lets the UI know the new item's id ahead of time (for drop animations).
    #[must_use]
    pub fn insert_track_at(
        &mut self,
        track: Track,
        at: usize,
        item_id: Option<String>,
    ) -> Vec<Effect> {
        let item_id = item_id.unwrap_or_else(|| (self.ids)());
        self.update_queue(QueueCommand::InsertTrack { at, item_id, track });
        self.take_effects()
    }

    #[must_use]
    pub fn insert_break_at(&mut self, at: usize) -> Vec<Effect> {
        let item_id = (self.ids)();
        self.update_queue(QueueCommand::InsertBreak { at, item_id });
        self.take_effects()
    }

    #[must_use]
    pub fn insert_break_after(&mut self, after_id: &str) -> Vec<Effect> {
        let item_id = (self.ids)();
        self.update_queue(QueueCommand::InsertBreakAfter {
            after_id: after_id.to_owned(),
            item_id,
        });
        self.take_effects()
    }

    /// "Add Break": right above `selected` (the selected item), else at the
    /// end of the Queue.
    #[must_use]
    pub fn insert_break(&mut self, selected: Option<&str>) -> Vec<Effect> {
        let at = selected
            .and_then(|id| self.queue.items().position(|item| item.id() == id))
            .unwrap_or(self.queue.len());
        self.insert_break_at(at)
    }

    #[must_use]
    pub fn remove_items(&mut self, item_ids: &[String]) -> Vec<Effect> {
        self.update_queue(QueueCommand::Remove {
            item_ids: item_ids.to_vec(),
        });
        self.take_effects()
    }

    /// Moves these items together, in Queue order, to display slot `slot`.
    #[must_use]
    pub fn move_items(&mut self, item_ids: &[String], slot: usize) -> Vec<Effect> {
        self.update_queue(QueueCommand::Move {
            item_ids: item_ids.to_vec(),
            slot,
        });
        self.take_effects()
    }

    /// Moves each of these items one place, or none if any can't move.
    #[must_use]
    pub fn step_items(&mut self, item_ids: &[String], direction: Direction) -> Vec<Effect> {
        self.update_queue(QueueCommand::Step {
            item_ids: item_ids.to_vec(),
            direction,
        });
        self.take_effects()
    }

    // ---- Transport ---------------------------------------------------------

    /// "Play now" for an Upcoming item.
    #[must_use]
    pub fn play_item(&mut self, item_id: &str) -> Vec<Effect> {
        let Some(QueueItemRef::Upcoming(item)) = self.queue.item(item_id) else {
            return self.take_effects();
        };
        if item.readiness == Readiness::Preparing {
            return self.take_effects();
        }
        let is_track = matches!(item.kind, ItemKind::Track(_));
        let retry = item.readiness == Readiness::Unavailable;
        self.playback_error = None;
        if retry {
            self.set_readiness(item_id, Readiness::Preparing);
        }
        let active_is_track = self
            .queue
            .active()
            .is_some_and(|active| matches!(active.kind, ItemKind::Track(_)));
        if is_track && active_is_track && !self.queue.is_crossfading() {
            self.crossfade_to(item_id);
        } else {
            self.restart_from(item_id);
        }
        self.take_effects()
    }

    /// Skip the Active Item (crossfading into the next Track when adjacent).
    #[must_use]
    pub fn skip(&mut self) -> Vec<Effect> {
        let Some(active) = self.queue.active() else {
            return self.take_effects();
        };
        if self.queue.is_crossfading() {
            self.promote_incoming(PlayLogReason::Skipped, false);
            return self.take_effects();
        }
        let active_is_track = matches!(active.kind, ItemKind::Track(_));
        let next_track_id = self
            .queue
            .queued()
            .first()
            .filter(|next| matches!(next.kind, ItemKind::Track(_)))
            .map(|next| next.id.clone());
        match next_track_id {
            Some(next_id) if active_is_track => {
                if self
                    .queue
                    .queued()
                    .first()
                    .is_some_and(|next| next.readiness == Readiness::Unavailable)
                {
                    self.set_readiness(&next_id, Readiness::Preparing);
                }
                self.playback_error = None;
                self.crossfade_to(&next_id);
            }
            _ => {
                self.stop_audio();
                self.advance(PlayLogReason::Skipped);
            }
        }
        self.take_effects()
    }

    // ---- Engine and service results ----------------------------------------

    #[must_use]
    pub fn audio_event(&mut self, event: AudioEvent) -> Vec<Effect> {
        match event {
            AudioEvent::Progress {
                id,
                position_secs,
                duration_secs,
            } => self.on_progress(id, position_secs, duration_secs),
            AudioEvent::Ended { id } => {
                if Some(id) == self.active_load {
                    // The engine already promoted its incoming deck, if any.
                    self.promote_incoming(PlayLogReason::Completed, true);
                } else if Some(id) == self.incoming_load {
                    self.incoming_ended();
                }
            }
            AudioEvent::Failed { id, message } => {
                if Some(id) == self.active_load {
                    let affected = self.queue.active().map(|item| item.id.clone());
                    self.fail_playback(message, affected);
                } else if Some(id) == self.incoming_load {
                    let affected = self.queue.incoming().map(|item| item.id.clone());
                    self.fail_playback(message, affected);
                }
            }
            AudioEvent::CrossfadeComplete { outgoing, incoming } => {
                if self.complete_after_fade
                    && Some(outgoing) == self.active_load
                    && Some(incoming) == self.incoming_load
                {
                    self.promote_incoming(PlayLogReason::Skipped, false);
                }
            }
        }
        self.take_effects()
    }

    /// Answer to [`Effect::Prepare`].
    #[must_use]
    pub fn track_prepared(
        &mut self,
        request: RequestId,
        key: &TrackKey,
        result: Result<PathBuf, String>,
    ) -> Vec<Effect> {
        if self.preparations.get(key) != Some(&Preparation::Pending(request)) {
            return self.take_effects();
        }
        match result {
            Ok(path) => {
                self.preparations
                    .insert(key.clone(), Preparation::Ready(path));
                self.set_key_readiness(key, Readiness::Ready);
                self.resume_pending();
            }
            Err(message) => {
                self.preparations.remove(key);
                self.set_key_readiness(key, Readiness::Unavailable);
                let pending_failed = self.pending.as_ref().is_some_and(|pending| {
                    self.queue
                        .item(pending.item_id())
                        .and_then(QueueItemRef::track)
                        .is_some_and(|track| &track.key() == key)
                });
                if pending_failed {
                    let affected = self
                        .pending
                        .as_ref()
                        .map(|pending| pending.item_id().to_owned());
                    self.fail_playback(message, affected);
                }
            }
        }
        self.take_effects()
    }

    /// The local Library changed: local Tracks must be resolved again.
    #[must_use]
    pub fn library_changed(&mut self) -> Vec<Effect> {
        self.preparations
            .retain(|key, _| key.source != SourceId::Local);
        for item in self.queue.queued_mut() {
            if let ItemKind::Track(track) = &item.kind
                && track.source == SourceId::Local
            {
                item.readiness = Readiness::Preparing;
            }
        }
        self.prepare_queued_tracks();
        self.take_effects()
    }

    /// Persistence or another service failed; show it to the DJ.
    pub fn report_error(&mut self, message: impl Into<String>) {
        self.playback_error = Some(message.into());
    }

    pub fn clear_error(&mut self) {
        self.playback_error = None;
    }

    pub fn set_crossfade_seconds(&mut self, seconds: u32) {
        self.crossfade_seconds = seconds;
    }

    // ---- Recovery and Play Log ---------------------------------------------

    /// Startup: the result of reading the recovery file.
    pub fn recovery_loaded(&mut self, result: Result<Option<RecoverySnapshot>, String>) {
        match result {
            Ok(Some(snapshot))
                if snapshot.status == RecoveryStatus::Interrupted && !snapshot.queue.is_empty() =>
            {
                self.recovery = RecoveryState::Pending(snapshot);
            }
            Ok(_) => self.recovery = RecoveryState::None,
            Err(message) => {
                self.recovery = RecoveryState::None;
                self.playback_error = Some(format!("Queue recovery could not be read: {message}"));
            }
        }
    }

    #[must_use]
    pub fn restore_recovery(&mut self) -> Vec<Effect> {
        let RecoveryState::Pending(snapshot) =
            std::mem::replace(&mut self.recovery, RecoveryState::None)
        else {
            return self.take_effects();
        };
        if let Some(started) = snapshot
            .session_started_at
            .as_deref()
            .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        {
            self.session_started_at = started.with_timezone(&Utc);
        }
        let queued = snapshot
            .queue
            .into_iter()
            .map(|item| match item {
                StoredQueueItem::Track { id, track } => UpcomingItem::track(id, track),
                StoredQueueItem::Break { id } => UpcomingItem::new_break(id),
            })
            .collect();
        self.replace_queue(Queue::Stopped { queued });
        self.take_effects()
    }

    /// "Start Empty": a new Session with an empty Queue.
    #[must_use]
    pub fn discard_recovery(&mut self) -> Vec<Effect> {
        self.recovery = RecoveryState::None;
        self.effects.push(Effect::DiscardRecovery);
        self.effects.push(Effect::ProtectTracks(Vec::new()));
        self.queue = Queue::default();
        self.session_started_at = (self.clock)();
        self.take_effects()
    }

    /// Startup: merge stored entries with any recorded since launch.
    pub fn play_log_loaded(&mut self, result: Result<Vec<PlayLogEntry>, String>) {
        match result {
            Ok(entries) => {
                let mut merged: HashMap<String, PlayLogEntry> = entries
                    .into_iter()
                    .map(|entry| (entry.id.clone(), entry))
                    .collect();
                for entry in self.play_log.drain(..) {
                    merged.insert(entry.id.clone(), entry);
                }
                let mut entries: Vec<_> = merged.into_values().collect();
                entries.sort_by(|a, b| b.started_at.cmp(&a.started_at));
                self.play_log = entries;
            }
            Err(message) => {
                self.playback_error = Some(format!("Play log could not be read: {message}"));
            }
        }
    }

    #[must_use]
    pub fn clear_play_log(&mut self) -> Vec<Effect> {
        self.play_log.clear();
        self.effects.push(Effect::ClearPlayLog);
        self.take_effects()
    }

    /// Stops audio, closes Playing log entries, and saves a clean snapshot.
    #[must_use]
    pub fn shutdown(&mut self) -> Vec<Effect> {
        self.finish_participants(PlayLogReason::Skipped);
        self.stop_audio();
        let queued = self.queue.items().map(requeue).collect();
        self.queue = Queue::Stopped { queued };
        let snapshot = self.snapshot(RecoveryStatus::Clean);
        self.effects.push(Effect::SaveRecovery(snapshot));
        self.take_effects()
    }

    // ---- Internals ---------------------------------------------------------

    fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
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

    fn update_queue(&mut self, command: QueueCommand) {
        let next = edit_queue(&self.queue, command);
        if next != self.queue {
            self.replace_queue(next);
        }
    }

    /// Every structural Queue change goes through here: protect, save, prepare.
    fn replace_queue(&mut self, queue: Queue) {
        self.queue = queue;
        let tracks: Vec<Track> = self.queue.tracks().cloned().collect();
        let keep: std::collections::HashSet<TrackKey> = tracks.iter().map(Track::key).collect();
        self.preparations.retain(|key, _| keep.contains(key));
        self.effects.push(Effect::ProtectTracks(tracks));
        let snapshot = self.snapshot(RecoveryStatus::Interrupted);
        self.effects.push(Effect::SaveRecovery(snapshot));
        self.prepare_queued_tracks();
    }

    fn prepare_queued_tracks(&mut self) {
        let mut requests = Vec::new();
        for item in self.queue.queued_mut() {
            let ItemKind::Track(track) = &item.kind else {
                continue;
            };
            if item.readiness != Readiness::Preparing {
                continue;
            }
            let key = track.key();
            match self.preparations.get(&key) {
                Some(Preparation::Ready(_)) => item.readiness = Readiness::Ready,
                Some(Preparation::Pending(_)) => {}
                None => {
                    let request = self.next_request;
                    self.next_request += 1;
                    self.preparations.insert(key, Preparation::Pending(request));
                    requests.push(Effect::Prepare {
                        request,
                        track: track.clone(),
                    });
                }
            }
        }
        self.effects.extend(requests);
    }

    fn set_readiness(&mut self, item_id: &str, readiness: Readiness) {
        if let Some(item) = self
            .queue
            .queued_mut()
            .iter_mut()
            .find(|item| item.id == item_id)
        {
            item.readiness = readiness;
        }
        if readiness == Readiness::Preparing {
            self.prepare_queued_tracks();
        }
    }

    fn set_key_readiness(&mut self, key: &TrackKey, readiness: Readiness) {
        for item in self.queue.queued_mut() {
            if let ItemKind::Track(track) = &item.kind
                && &track.key() == key
                && item.readiness == Readiness::Preparing
            {
                item.readiness = readiness;
            }
        }
    }

    fn ready_path(&self, item_id: &str) -> Option<PathBuf> {
        let track = self.queue.item(item_id)?.track()?;
        match self.preparations.get(&track.key()) {
            Some(Preparation::Ready(path)) => Some(path.clone()),
            _ => None,
        }
    }

    fn new_load(&mut self) -> LoadId {
        let id = self.next_load;
        self.next_load += 1;
        id
    }

    fn stop_audio(&mut self) {
        self.effects.push(Effect::Audio(AudioCommand::Stop));
        self.active_load = None;
        self.incoming_load = None;
        self.pending = None;
        self.crossfade_requested = false;
        self.complete_after_fade = false;
    }

    /// Stops everything and starts `item_id`, dropping the items before it.
    fn restart_from(&mut self, item_id: &str) {
        let Some(index) = self.queue.index_of(item_id) else {
            return;
        };
        let queued: Vec<UpcomingItem> = self.queue.items().skip(index).map(requeue).collect();
        self.finish_participants(PlayLogReason::Skipped);
        self.stop_audio();
        self.replace_queue(Queue::Stopped { queued });
        self.start_item(item_id);
    }

    /// Starts an Upcoming item as the Active Item (stopped Queue assumed).
    fn start_item(&mut self, item_id: &str) {
        let Some(QueueItemRef::Upcoming(item)) = self.queue.item(item_id) else {
            return;
        };
        match item.kind {
            ItemKind::Break => {
                self.stop_audio();
                self.become_active(item_id, None);
            }
            ItemKind::Track(_) => match self.ready_path(item_id) {
                Some(path) => {
                    let load = self.new_load();
                    self.effects
                        .push(Effect::Audio(AudioCommand::Play { id: load, path }));
                    self.become_active(item_id, Some(load));
                }
                None => {
                    self.set_readiness(item_id, Readiness::Preparing);
                    self.pending = Some(PendingStart::Play {
                        item_id: item_id.to_owned(),
                    });
                }
            },
        }
    }

    fn become_active(&mut self, item_id: &str, load: Option<LoadId>) {
        let mut queued = self.queue.queued().to_vec();
        let Some(position) = queued.iter().position(|item| item.id == item_id) else {
            return;
        };
        let item = queued.remove(position);
        // Starting from a stopped Queue keeps everything before it already removed.
        self.queue = Queue::Playing {
            active: playing(item),
            queued,
        };
        self.active_load = load;
        self.incoming_load = None;
        self.crossfade_requested = false;
        self.complete_after_fade = false;
        self.pending = None;
        self.start_log(item_id);
    }

    /// Manual crossfade from the active Track into `item_id`, which moves to the front.
    fn crossfade_to(&mut self, item_id: &str) {
        let Queue::Playing { active, queued } = &self.queue else {
            return;
        };
        if !matches!(active.kind, ItemKind::Track(_)) {
            return;
        }
        let mut queued = queued.clone();
        let Some(position) = queued.iter().position(|item| item.id == item_id) else {
            return;
        };
        let target = queued.remove(position);
        queued.insert(0, target);
        let active_id = active.id.clone();
        let active = active.clone();
        self.replace_queue(Queue::Playing { active, queued });
        self.pending = Some(PendingStart::Crossfade {
            active_id,
            item_id: item_id.to_owned(),
        });
        self.resume_pending();
    }

    /// Starts the pending start or crossfade once its Track is ready.
    fn resume_pending(&mut self) {
        let Some(pending) = self.pending.clone() else {
            return;
        };
        let Some(path) = self.ready_path(pending.item_id()) else {
            if self.queue.item(pending.item_id()).is_none() {
                self.pending = None;
            }
            return;
        };
        match pending {
            PendingStart::Play { item_id } => {
                if !self.queue.is_stopped() {
                    self.pending = None;
                    return;
                }
                let load = self.new_load();
                self.effects
                    .push(Effect::Audio(AudioCommand::Play { id: load, path }));
                self.become_active(&item_id, Some(load));
            }
            PendingStart::Crossfade { active_id, item_id } => {
                self.pending = None;
                if self.queue.active().map(|active| active.id.as_str()) != Some(active_id.as_str())
                    || self.queue.is_crossfading()
                {
                    return;
                }
                self.begin_crossfade(&item_id, path, true);
            }
        }
    }

    fn begin_crossfade(&mut self, item_id: &str, path: PathBuf, manual: bool) {
        let Queue::Playing { active, queued } = &self.queue else {
            return;
        };
        let (active, mut queued) = (active.clone(), queued.clone());
        let Some(position) = queued.iter().position(|item| item.id == item_id) else {
            return;
        };
        let incoming = queued.remove(position);
        let load = self.new_load();
        self.effects.push(Effect::Audio(AudioCommand::Crossfade {
            id: load,
            path,
            duration_seconds: self.crossfade_seconds,
        }));
        self.queue = Queue::Crossfading {
            active,
            incoming: playing(incoming),
            queued,
        };
        self.incoming_load = Some(load);
        self.complete_after_fade = manual;
        self.start_log(item_id);
    }

    fn on_progress(&mut self, id: LoadId, position: f64, duration: Option<f64>) {
        let progress = PlaybackProgress {
            current_time: position,
            duration: duration
                .filter(|value| value.is_finite() && *value > 0.0)
                .unwrap_or(0.0),
        };
        if Some(id) == self.incoming_load {
            if let Queue::Crossfading { incoming, .. } = &mut self.queue {
                incoming.progress = progress;
            }
            return;
        }
        if Some(id) != self.active_load {
            return;
        }
        match &mut self.queue {
            Queue::Playing { active, .. } | Queue::Crossfading { active, .. } => {
                active.progress = progress
            }
            Queue::Stopped { .. } => return,
        }
        let crossfade = f64::from(self.crossfade_seconds);
        if !self.crossfade_requested
            && crossfade > 0.0
            && progress.duration > 0.0
            && position > 0.0
            && progress.duration - position <= crossfade
        {
            self.crossfade_requested = true;
            self.on_approaching_end();
        }
    }

    /// Automatic crossfade into an adjacent, ready Track.
    fn on_approaching_end(&mut self) {
        if self.queue.is_crossfading() || self.pending.is_some() {
            return;
        }
        let Queue::Playing { active, queued } = &self.queue else {
            return;
        };
        if !matches!(active.kind, ItemKind::Track(_)) {
            return;
        }
        let Some(next) = queued.first() else { return };
        if !matches!(next.kind, ItemKind::Track(_)) || next.readiness != Readiness::Ready {
            return;
        }
        let next_id = next.id.clone();
        if let Some(path) = self.ready_path(&next_id) {
            self.begin_crossfade(&next_id, path, false);
        }
    }

    /// The outgoing Track leaves; the Incoming Track becomes the Active Item.
    fn promote_incoming(&mut self, reason: PlayLogReason, engine_already_promoted: bool) {
        let Queue::Crossfading {
            active,
            incoming,
            queued,
        } = &self.queue
        else {
            self.advance(reason);
            return;
        };
        let (active_id, incoming, queued) = (active.id.clone(), incoming.clone(), queued.clone());
        if !engine_already_promoted && let Some(load) = self.incoming_load {
            self.effects
                .push(Effect::Audio(AudioCommand::PromoteIncoming {
                    incoming: load,
                }));
        }
        self.finish_log(&active_id, reason);
        self.active_load = self.incoming_load.take();
        self.crossfade_requested = false;
        self.complete_after_fade = false;
        self.replace_queue(Queue::Playing {
            active: incoming,
            queued,
        });
    }

    /// The Incoming Track ended before the outgoing one (a very short Track).
    fn incoming_ended(&mut self) {
        let Queue::Crossfading {
            active,
            incoming,
            queued,
        } = &self.queue
        else {
            return;
        };
        let (active, incoming_id, queued) = (active.clone(), incoming.id.clone(), queued.clone());
        self.finish_log(&incoming_id, PlayLogReason::Completed);
        self.incoming_load = None;
        self.complete_after_fade = false;
        self.crossfade_requested = false;
        self.replace_queue(Queue::Playing { active, queued });
    }

    /// The Active Item left: start the next item, stopping at a Break or an unready Track.
    fn advance(&mut self, reason: PlayLogReason) {
        if let Some(active) = self.queue.active() {
            let active_id = active.id.clone();
            self.finish_log(&active_id, reason);
        }
        self.active_load = None;
        self.incoming_load = None;
        self.pending = None;
        self.crossfade_requested = false;
        self.complete_after_fade = false;
        let remaining: Vec<UpcomingItem> = self.queue.items().skip(1).map(requeue).collect();
        self.replace_queue(Queue::Stopped { queued: remaining });
        let Some(next) = self.queue.queued().first() else {
            return;
        };
        let next_id = next.id.clone();
        if let ItemKind::Track(track) = &next.kind
            && next.readiness != Readiness::Ready
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
    fn fail_playback(&mut self, message: String, affected_item: Option<String>) {
        self.finish_participants(PlayLogReason::Error);
        self.stop_audio();
        let queued: Vec<UpcomingItem> = self.queue.items().map(requeue).collect();
        self.queue = Queue::Stopped { queued };
        if let Some(item_id) = affected_item {
            let track = self
                .queue
                .item(&item_id)
                .and_then(QueueItemRef::track)
                .cloned();
            if let Some(track) = track {
                self.preparations.remove(&track.key());
                let mut event = DiagnosticEvent::new("playback")
                    .source(track.source)
                    .error(&message);
                if track.source == SourceId::YoutubeMusic {
                    event = event.video_id(&track.id);
                }
                self.effects.push(Effect::Diagnostic(event));
                if let Some(item) = self
                    .queue
                    .queued_mut()
                    .iter_mut()
                    .find(|item| item.id == item_id)
                {
                    item.readiness = Readiness::Unavailable;
                }
            }
        }
        self.playback_error = Some(message);
    }

    fn finish_participants(&mut self, reason: PlayLogReason) {
        let ids: Vec<String> = self
            .queue
            .items()
            .filter(|item| !item.is_upcoming())
            .map(|item| item.id().to_owned())
            .collect();
        for id in ids {
            self.finish_log(&id, reason);
        }
    }

    fn start_log(&mut self, item_id: &str) {
        let Some(item) = self.queue.item(item_id) else {
            return;
        };
        let stored = item.stored();
        if let Some(track) = item.track()
            && track.source == SourceId::YoutubeMusic
        {
            self.effects
                .push(Effect::RecordYoutubePlay(track.id.clone()));
        }
        let entry = PlayLogEntry {
            id: (self.ids)(),
            item: stored,
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
            .find(|entry| entry.item.id() == item_id)
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
