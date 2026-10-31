#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use chrono::{TimeZone, Utc};

use super::*;
use crate::queue::{QueueItemRef, Readiness};

fn track(id: &str, source: SourceId, duration: Option<f64>) -> Track {
    Track {
        id: id.into(),
        source,
        title: format!("Title {id}"),
        artist: "Artist".into(),
        album: None,
        duration_seconds: duration,
        artwork_url: None,
        explicit: false,
    }
}

fn local(id: &str) -> Track {
    track(id, SourceId::Local, Some(180.0))
}

fn yt(id: &str) -> Track {
    track(id, SourceId::YoutubeMusic, Some(200.0))
}

fn path_for(track: &Track) -> PathBuf {
    PathBuf::from(format!("/audio/{}", track.id))
}

/// Drives a Session with a deterministic clock and ids, recording effects.
struct Harness {
    session: Session,
    effects: Vec<Effect>,
    clock: Arc<AtomicI64>,
}

impl Harness {
    fn new() -> Self {
        let clock = Arc::new(AtomicI64::new(1_700_000_000));
        let ids = Arc::new(AtomicU64::new(0));
        let clock_source = clock.clone();
        let session = Session::new(
            Box::new(move || {
                Utc.timestamp_opt(clock_source.load(Ordering::SeqCst), 0)
                    .single()
                    .unwrap()
            }),
            Box::new(move || format!("id-{}", ids.fetch_add(1, Ordering::SeqCst) + 1)),
        );
        let mut harness = Self {
            session,
            effects: Vec::new(),
            clock,
        };
        harness.session.recovery_loaded(Ok(None));
        harness
    }

    fn tick(&self, seconds: i64) {
        self.clock.fetch_add(seconds, Ordering::SeqCst);
    }

    fn record(&mut self, effects: Vec<Effect>) -> Vec<Effect> {
        self.effects.extend(effects.iter().cloned());
        effects
    }

    fn add(&mut self, track: Track) -> String {
        let effects = self.session.add_track_to_end(track);
        self.record(effects);
        self.session.queue().items().last().unwrap().id().to_owned()
    }

    fn add_break(&mut self) -> String {
        let effects = self.session.insert_break_at(usize::MAX);
        self.record(effects);
        self.session.queue().items().last().unwrap().id().to_owned()
    }

    /// Answers every outstanding Prepare effect successfully.
    fn prepare_all(&mut self) {
        loop {
            let pending: Vec<(RequestId, Track)> = self
                .effects
                .iter()
                .filter_map(|effect| match effect {
                    Effect::Prepare { request, track } => Some((*request, track.clone())),
                    _ => None,
                })
                .collect();
            self.effects
                .retain(|effect| !matches!(effect, Effect::Prepare { .. }));
            if pending.is_empty() {
                return;
            }
            for (request, track) in pending {
                let effects =
                    self.session
                        .track_prepared(request, &track.key(), Ok(path_for(&track)));
                self.record(effects);
            }
        }
    }

    fn take_prepares(&mut self) -> Vec<(RequestId, Track)> {
        let pending = self
            .effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Prepare { request, track } => Some((*request, track.clone())),
                _ => None,
            })
            .collect();
        self.effects
            .retain(|effect| !matches!(effect, Effect::Prepare { .. }));
        pending
    }

    fn play(&mut self, item_id: &str) {
        let effects = self.session.play_item(item_id);
        self.record(effects);
    }

    fn skip(&mut self) {
        let effects = self.session.skip();
        self.record(effects);
    }

    fn audio(&mut self, event: AudioEvent) {
        let effects = self.session.audio_event(event);
        self.record(effects);
    }

    fn audio_commands(&self) -> Vec<AudioCommand> {
        self.effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Audio(command) => Some(command.clone()),
                _ => None,
            })
            .collect()
    }

    fn last_play_load(&self) -> LoadId {
        self.audio_commands()
            .iter()
            .rev()
            .find_map(|command| match command {
                AudioCommand::Play { id, .. } => Some(*id),
                _ => None,
            })
            .unwrap()
    }

    fn last_crossfade_load(&self) -> LoadId {
        self.audio_commands()
            .iter()
            .rev()
            .find_map(|command| match command {
                AudioCommand::Crossfade { id, .. } => Some(*id),
                _ => None,
            })
            .unwrap()
    }

    fn clear(&mut self) {
        self.effects.clear();
    }

    fn ids(&self) -> Vec<String> {
        self.session
            .queue()
            .items()
            .map(|item| item.id().to_owned())
            .collect()
    }

    fn active_id(&self) -> Option<String> {
        self.session.queue().active().map(|item| item.id.clone())
    }

    fn readiness(&self, item_id: &str) -> Option<Readiness> {
        match self.session.queue().item(item_id) {
            Some(QueueItemRef::Upcoming(item)) => Some(item.readiness),
            _ => None,
        }
    }

    fn log_reason(&self, item_id: &str) -> Option<PlayLogReason> {
        self.session
            .play_log()
            .iter()
            .find(|entry| entry.item.id() == item_id)
            .and_then(|entry| entry.reason)
    }

    fn progress(&mut self, id: LoadId, position: f64, duration: f64) {
        self.audio(AudioEvent::Progress {
            id,
            position_secs: position,
            duration_secs: Some(duration),
        });
    }
}

#[test]
fn adding_tracks_prepares_once_per_track_and_never_plays() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let a2 = h.add(local("a"));
    assert_eq!(h.readiness(&a), Some(Readiness::Preparing));
    assert_eq!(h.take_prepares().len(), 1, "same Track is prepared once");
    assert!(
        h.audio_commands().is_empty(),
        "an empty Session never starts by itself"
    );
    assert!(h.session.queue().is_stopped());

    let effects = h
        .session
        .track_prepared(1, &local("a").key(), Ok(PathBuf::from("/a")));
    h.record(effects);
    assert_eq!(h.readiness(&a), Some(Readiness::Ready));
    assert_eq!(h.readiness(&a2), Some(Readiness::Ready));
    assert!(h.audio_commands().is_empty());
}

#[test]
fn every_structural_change_saves_an_interrupted_snapshot() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let snapshot = h
        .effects
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::SaveRecovery(snapshot) => Some(snapshot.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(snapshot.status, RecoveryStatus::Interrupted);
    assert_eq!(snapshot.queue.len(), 1);
    assert_eq!(snapshot.queue[0].id(), a);
    assert!(
        h.effects
            .iter()
            .any(|effect| matches!(effect, Effect::ProtectTracks(tracks) if tracks.len() == 1))
    );
}

#[test]
fn playing_a_preparing_track_is_ignored() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    h.clear();
    h.play(&a);
    assert!(h.audio_commands().is_empty());
    assert!(h.session.queue().is_stopped());
}

#[test]
fn play_starts_track_and_logs_it() {
    let mut h = Harness::new();
    let a = h.add(yt("a"));
    h.prepare_all();
    h.clear();
    h.play(&a);
    assert_eq!(
        h.audio_commands(),
        vec![
            AudioCommand::Stop,
            AudioCommand::Play {
                id: h.last_play_load(),
                path: PathBuf::from("/audio/a")
            }
        ]
    );
    assert_eq!(h.active_id().as_deref(), Some(a.as_str()));
    assert_eq!(h.session.play_log().len(), 1);
    assert!(h.session.play_log()[0].ended_at.is_none());
    assert!(h.effects.contains(&Effect::RecordYoutubePlay("a".into())));
    assert!(h.session.needs_close_confirmation());
}

#[test]
fn natural_end_advances_to_next_track() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let b = h.add(local("b"));
    h.prepare_all();
    h.play(&a);
    let load = h.last_play_load();
    h.clear();
    h.tick(180);
    h.audio(AudioEvent::Ended { id: load });
    assert_eq!(h.active_id().as_deref(), Some(b.as_str()));
    assert_eq!(h.log_reason(&a), Some(PlayLogReason::Completed));
    assert!(matches!(
        h.audio_commands().as_slice(),
        [AudioCommand::Play { .. }]
    ));
    assert_ne!(h.last_play_load(), load);
}

#[test]
fn approaching_end_crossfades_into_adjacent_ready_track() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let b = h.add(local("b"));
    h.prepare_all();
    h.play(&a);
    let load_a = h.last_play_load();
    h.clear();

    h.progress(load_a, 100.0, 180.0);
    assert!(h.audio_commands().is_empty());
    h.progress(load_a, 177.5, 180.0);
    assert_eq!(
        h.audio_commands(),
        vec![AudioCommand::Crossfade {
            id: h.last_crossfade_load(),
            path: PathBuf::from("/audio/b"),
            duration_seconds: 3
        }]
    );
    let load_b = h.last_crossfade_load();
    assert!(h.session.queue().is_crossfading());
    assert_eq!(
        h.session.queue().incoming().map(|item| item.id.as_str()),
        Some(b.as_str())
    );
    assert_eq!(
        h.session.play_log().len(),
        2,
        "incoming Track entered Playing"
    );

    // A second progress tick does not start another crossfade.
    h.clear();
    h.progress(load_a, 178.0, 180.0);
    h.progress(load_b, 0.5, 200.0);
    assert!(h.audio_commands().is_empty());
    assert_eq!(
        h.session.queue().incoming().unwrap().progress.current_time,
        0.5
    );

    // The outgoing Track ends; the engine already promoted, so no PromoteIncoming.
    h.audio(AudioEvent::Ended { id: load_a });
    assert!(h.audio_commands().is_empty());
    assert_eq!(h.active_id().as_deref(), Some(b.as_str()));
    assert!(!h.session.queue().is_crossfading());
    assert_eq!(h.log_reason(&a), Some(PlayLogReason::Completed));

    // The new Active Item keeps receiving progress under its load id.
    h.progress(load_b, 10.0, 200.0);
    assert_eq!(
        h.session.queue().active().unwrap().progress.current_time,
        10.0
    );
}

#[test]
fn never_crossfades_across_a_break() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let br = h.add_break();
    let c = h.add(local("c"));
    h.prepare_all();
    h.play(&a);
    let load_a = h.last_play_load();
    h.clear();
    h.progress(load_a, 179.0, 180.0);
    assert!(h.audio_commands().is_empty());

    h.audio(AudioEvent::Ended { id: load_a });
    assert_eq!(h.active_id().as_deref(), Some(br.as_str()));
    assert_eq!(
        h.audio_commands(),
        vec![AudioCommand::Stop],
        "a Break is silent"
    );
    assert_eq!(h.session.play_log()[0].item.id(), br, "Break is logged");

    // Nothing advances past the Break until the DJ skips it.
    h.clear();
    h.audio(AudioEvent::Ended { id: load_a });
    assert_eq!(h.active_id().as_deref(), Some(br.as_str()));
    h.skip();
    assert_eq!(h.active_id().as_deref(), Some(c.as_str()));
    assert_eq!(h.log_reason(&br), Some(PlayLogReason::Skipped));
    assert!(matches!(
        h.audio_commands().last(),
        Some(AudioCommand::Play { .. })
    ));
}

#[test]
fn unready_next_track_stops_playback_with_an_error() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    h.prepare_all();
    let b = h.add(yt("b"));
    let _ = h.take_prepares(); // b never finishes preparing
    h.play(&a);
    let load_a = h.last_play_load();
    h.clear();
    h.progress(load_a, 179.0, 180.0);
    assert!(
        h.audio_commands().is_empty(),
        "no crossfade into an unready Track"
    );
    h.audio(AudioEvent::Ended { id: load_a });
    assert!(h.session.queue().is_stopped());
    assert_eq!(h.ids(), vec![b.clone()]);
    assert_eq!(
        h.session.playback_error(),
        Some("Title b was not ready when the previous track ended.")
    );
    assert!(h.audio_commands().is_empty());
}

#[test]
fn manual_skip_crossfades_then_completes_after_fade() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let b = h.add(local("b"));
    h.prepare_all();
    h.play(&a);
    let load_a = h.last_play_load();
    h.clear();
    h.skip();
    let load_b = h.last_crossfade_load();
    assert!(h.session.queue().is_crossfading());

    h.clear();
    h.audio(AudioEvent::CrossfadeComplete {
        outgoing: load_a,
        incoming: load_b,
    });
    assert_eq!(
        h.audio_commands(),
        vec![AudioCommand::PromoteIncoming { incoming: load_b }]
    );
    assert_eq!(h.active_id().as_deref(), Some(b.as_str()));
    assert_eq!(h.log_reason(&a), Some(PlayLogReason::Skipped));

    // A late Ended for the outgoing deck is ignored.
    h.clear();
    h.audio(AudioEvent::Ended { id: load_a });
    assert_eq!(h.active_id().as_deref(), Some(b.as_str()));
    assert!(h.audio_commands().is_empty());
}

#[test]
fn automatic_crossfade_does_not_promote_on_fade_complete() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let _b = h.add(local("b"));
    h.prepare_all();
    h.play(&a);
    let load_a = h.last_play_load();
    h.progress(load_a, 178.0, 180.0);
    let load_b = h.last_crossfade_load();
    h.clear();
    h.audio(AudioEvent::CrossfadeComplete {
        outgoing: load_a,
        incoming: load_b,
    });
    assert!(h.audio_commands().is_empty());
    assert!(
        h.session.queue().is_crossfading(),
        "waits for the natural end"
    );
}

#[test]
fn skip_while_crossfading_promotes_incoming() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let b = h.add(local("b"));
    h.prepare_all();
    h.play(&a);
    let load_a = h.last_play_load();
    h.progress(load_a, 178.0, 180.0);
    let load_b = h.last_crossfade_load();
    h.clear();
    h.skip();
    assert_eq!(
        h.audio_commands(),
        vec![AudioCommand::PromoteIncoming { incoming: load_b }]
    );
    assert_eq!(h.active_id().as_deref(), Some(b.as_str()));
    assert_eq!(h.log_reason(&a), Some(PlayLogReason::Skipped));
}

#[test]
fn skip_waits_for_next_track_to_prepare() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    h.prepare_all();
    let b = h.add(yt("b"));
    let prepares = h.take_prepares();
    h.play(&a);
    h.clear();
    h.skip();
    assert!(
        h.audio_commands().is_empty(),
        "active Track keeps playing meanwhile"
    );
    assert_eq!(h.active_id().as_deref(), Some(a.as_str()));

    let (request, track) = prepares[0].clone();
    let effects = h
        .session
        .track_prepared(request, &track.key(), Ok(PathBuf::from("/b")));
    h.record(effects);
    assert!(h.session.queue().is_crossfading());
    assert_eq!(h.session.queue().incoming().unwrap().id, b);
}

#[test]
fn pending_skip_is_cancelled_when_active_ends_first() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    h.prepare_all();
    let b = h.add(yt("b"));
    let prepares = h.take_prepares();
    h.play(&a);
    let load_a = h.last_play_load();
    h.skip();
    h.audio(AudioEvent::Ended { id: load_a });
    assert!(h.session.queue().is_stopped());
    assert!(h.session.playback_error().is_some());

    h.clear();
    let (request, track) = prepares[0].clone();
    let effects = h
        .session
        .track_prepared(request, &track.key(), Ok(PathBuf::from("/b")));
    h.record(effects);
    assert!(
        h.audio_commands().is_empty(),
        "late preparation must not start playback"
    );
    assert_eq!(h.readiness(&b), Some(Readiness::Ready));
}

#[test]
fn failed_manual_skip_stops_and_reports() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    h.prepare_all();
    let b = h.add(yt("b"));
    let prepares = h.take_prepares();
    h.play(&a);
    h.skip();
    h.clear();
    let (request, track) = prepares[0].clone();
    let effects = h.session.track_prepared(
        request,
        &track.key(),
        Err("YouTube download timed out.".into()),
    );
    h.record(effects);
    assert!(h.session.queue().is_stopped());
    assert_eq!(
        h.session.playback_error(),
        Some("YouTube download timed out.")
    );
    assert_eq!(h.readiness(&b), Some(Readiness::Unavailable));
    assert_eq!(h.log_reason(&a), Some(PlayLogReason::Error));
    assert!(h.audio_commands().contains(&AudioCommand::Stop));
}

#[test]
fn skip_into_break_stops_audio() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let br = h.add_break();
    h.prepare_all();
    h.play(&a);
    h.clear();
    h.skip();
    assert_eq!(h.active_id().as_deref(), Some(br.as_str()));
    assert!(
        h.audio_commands()
            .iter()
            .all(|command| *command == AudioCommand::Stop)
    );
    assert_eq!(h.log_reason(&a), Some(PlayLogReason::Skipped));
}

#[test]
fn skipping_the_last_item_stops() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    h.prepare_all();
    h.play(&a);
    h.skip();
    assert!(h.session.queue().is_empty());
    assert!(!h.session.needs_close_confirmation());
    assert_eq!(h.log_reason(&a), Some(PlayLogReason::Skipped));
}

#[test]
fn stale_engine_events_are_ignored() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let b = h.add(local("b"));
    h.prepare_all();
    h.play(&a);
    let old = h.last_play_load();
    h.play(&b); // manual crossfade
    h.skip(); // promote b
    h.clear();
    h.audio(AudioEvent::Failed {
        id: old,
        message: "boom".into(),
    });
    h.audio(AudioEvent::Progress {
        id: old,
        position_secs: 5.0,
        duration_secs: Some(10.0),
    });
    assert_eq!(h.active_id().as_deref(), Some(b.as_str()));
    assert!(h.session.playback_error().is_none());
    assert!(h.audio_commands().is_empty());
}

#[test]
fn audio_failure_stops_marks_unavailable_and_allows_retry() {
    let mut h = Harness::new();
    let a = h.add(yt("a"));
    let b = h.add(local("b"));
    h.prepare_all();
    h.play(&a);
    let load = h.last_play_load();
    h.clear();
    h.audio(AudioEvent::Failed {
        id: load,
        message: "Unsupported audio".into(),
    });
    assert!(h.session.queue().is_stopped());
    assert_eq!(h.ids(), vec![a.clone(), b.clone()]);
    assert_eq!(h.readiness(&a), Some(Readiness::Unavailable));
    assert_eq!(h.readiness(&b), Some(Readiness::Ready));
    assert_eq!(h.session.playback_error(), Some("Unsupported audio"));
    assert_eq!(h.log_reason(&a), Some(PlayLogReason::Error));
    assert!(h.effects.iter().any(|effect| matches!(
        effect,
        Effect::Diagnostic(event) if event.video_id.as_deref() == Some("a") && event.operation == "playback"
    )));

    // Retrying re-resolves the Track rather than reusing the failed file.
    h.clear();
    h.play(&a);
    assert_eq!(h.take_prepares().len(), 1);
    assert!(h.session.playback_error().is_none());
    assert_eq!(h.readiness(&a), Some(Readiness::Preparing));
}

#[test]
fn failed_preparation_for_pending_start_reports_error() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let prepares = h.take_prepares();
    let effects = h
        .session
        .track_prepared(prepares[0].0, &prepares[0].1.key(), Err("gone".into()));
    h.record(effects);
    assert_eq!(h.readiness(&a), Some(Readiness::Unavailable));
    // Unavailable Tracks can be retried with Play now.
    h.clear();
    h.play(&a);
    let prepares = h.take_prepares();
    assert_eq!(prepares.len(), 1);
    let effects = h.session.track_prepared(
        prepares[0].0,
        &prepares[0].1.key(),
        Err("still gone".into()),
    );
    h.record(effects);
    assert_eq!(h.session.playback_error(), Some("still gone"));
    assert!(h.session.queue().is_stopped());
    assert!(
        h.audio_commands()
            .iter()
            .all(|command| *command == AudioCommand::Stop)
    );
}

#[test]
fn play_now_from_stopped_drops_earlier_items() {
    let mut h = Harness::new();
    let _a = h.add(local("a"));
    let b = h.add(local("b"));
    let c = h.add(local("c"));
    h.prepare_all();
    h.play(&b);
    assert_eq!(h.ids(), vec![b.clone(), c]);
    assert_eq!(h.active_id().as_deref(), Some(b.as_str()));
}

#[test]
fn play_now_while_playing_crossfades_and_moves_target_first() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let b = h.add(local("b"));
    let c = h.add(local("c"));
    h.prepare_all();
    h.play(&a);
    h.clear();
    h.play(&c);
    assert!(h.session.queue().is_crossfading());
    assert_eq!(h.ids(), vec![a, c, b]);
}

#[test]
fn play_now_on_break_while_playing_stops_audio() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let br = h.add_break();
    h.prepare_all();
    h.play(&a);
    h.clear();
    h.play(&br);
    assert_eq!(h.active_id().as_deref(), Some(br.as_str()));
    assert_eq!(h.log_reason(&a), Some(PlayLogReason::Skipped));
    assert!(
        h.audio_commands()
            .iter()
            .all(|command| *command == AudioCommand::Stop)
    );
}

#[test]
fn zero_crossfade_advances_on_end_without_crossfading() {
    let mut h = Harness::new();
    h.session.set_crossfade_seconds(0);
    let a = h.add(local("a"));
    let b = h.add(local("b"));
    h.prepare_all();
    h.play(&a);
    let load = h.last_play_load();
    h.clear();
    h.progress(load, 180.0, 180.0);
    assert!(h.audio_commands().is_empty());
    h.audio(AudioEvent::Ended { id: load });
    assert_eq!(h.active_id().as_deref(), Some(b.as_str()));
    assert!(matches!(
        h.audio_commands().as_slice(),
        [AudioCommand::Play { .. }]
    ));
}

#[test]
fn short_incoming_track_ending_first_is_logged_and_dropped() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let b = h.add(local("b"));
    let c = h.add(local("c"));
    h.prepare_all();
    h.play(&a);
    let load_a = h.last_play_load();
    h.progress(load_a, 178.0, 180.0);
    let load_b = h.last_crossfade_load();
    h.audio(AudioEvent::Ended { id: load_b });
    assert!(!h.session.queue().is_crossfading());
    assert_eq!(h.ids(), vec![a.clone(), c.clone()]);
    assert_eq!(h.log_reason(&b), Some(PlayLogReason::Completed));
    h.audio(AudioEvent::Ended { id: load_a });
    assert_eq!(h.active_id().as_deref(), Some(c.as_str()));
}

#[test]
fn breaks_cannot_be_inserted_between_crossfading_tracks() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let _b = h.add(local("b"));
    h.prepare_all();
    h.play(&a);
    let load = h.last_play_load();
    h.progress(load, 178.0, 180.0);
    let before = h.ids();
    let effects = h.session.insert_break_at(1);
    assert!(effects.is_empty());
    assert_eq!(h.ids(), before);
}

#[test]
fn removing_a_pending_start_cancels_it() {
    let mut h = Harness::new();
    let a = h.add(yt("a"));
    let prepares = h.take_prepares();
    // Make it unavailable, then retry so a start is pending.
    let effects = h
        .session
        .track_prepared(prepares[0].0, &prepares[0].1.key(), Err("x".into()));
    h.record(effects);
    h.play(&a);
    let prepares = h.take_prepares();
    let effects = h.session.remove_items(std::slice::from_ref(&a));
    h.record(effects);
    h.clear();
    let effects =
        h.session
            .track_prepared(prepares[0].0, &prepares[0].1.key(), Ok(PathBuf::from("/a")));
    h.record(effects);
    assert!(h.audio_commands().is_empty());
    assert!(h.session.queue().is_empty());
}

#[test]
fn stale_preparation_results_are_ignored() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let first = h.take_prepares();
    let effects = h.session.library_changed();
    h.record(effects);
    let second = h.take_prepares();
    assert_eq!(second.len(), 1);
    assert_ne!(first[0].0, second[0].0);
    let effects = h
        .session
        .track_prepared(first[0].0, &first[0].1.key(), Err("old".into()));
    h.record(effects);
    assert_eq!(h.readiness(&a), Some(Readiness::Preparing));
    let effects =
        h.session
            .track_prepared(second[0].0, &second[0].1.key(), Ok(PathBuf::from("/a")));
    h.record(effects);
    assert_eq!(h.readiness(&a), Some(Readiness::Ready));
}

#[test]
fn library_change_reprepares_only_local_tracks() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let y = h.add(yt("y"));
    h.prepare_all();
    h.clear();
    let effects = h.session.library_changed();
    h.record(effects);
    let prepares = h.take_prepares();
    assert_eq!(prepares.len(), 1);
    assert_eq!(prepares[0].1.id, "a");
    assert_eq!(h.readiness(&a), Some(Readiness::Preparing));
    assert_eq!(h.readiness(&y), Some(Readiness::Ready));
}

#[test]
fn recovery_restore_and_discard() {
    let mut h = Harness::new();
    let snapshot = RecoverySnapshot {
        version: 1,
        status: RecoveryStatus::Interrupted,
        queue: vec![
            StoredQueueItem::Track {
                id: "q1".into(),
                track: local("a"),
            },
            StoredQueueItem::Break { id: "q2".into() },
        ],
        session_started_at: Some("2026-01-02T03:04:05.000Z".into()),
        updated_at: "2026-01-02T04:00:00.000Z".into(),
    };
    h.session.recovery_loaded(Ok(Some(snapshot.clone())));
    assert!(matches!(h.session.recovery(), RecoveryState::Pending(_)));
    let effects = h.session.restore_recovery();
    h.record(effects);
    assert_eq!(h.ids(), vec!["q1".to_owned(), "q2".to_owned()]);
    assert_eq!(h.readiness("q1"), Some(Readiness::Preparing));
    assert!(
        h.session.queue().is_stopped(),
        "recovered Queue stays stopped"
    );
    assert_eq!(
        h.session.session_started_at(),
        Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).single().unwrap()
    );
    assert_eq!(h.take_prepares().len(), 1);

    let mut h = Harness::new();
    h.session.recovery_loaded(Ok(Some(snapshot.clone())));
    let effects = h.session.discard_recovery();
    assert!(effects.contains(&Effect::DiscardRecovery));
    assert!(h.session.queue().is_empty());

    let mut clean = snapshot.clone();
    clean.status = RecoveryStatus::Clean;
    h.session.recovery_loaded(Ok(Some(clean)));
    assert_eq!(h.session.recovery(), &RecoveryState::None);

    let mut empty = snapshot;
    empty.queue.clear();
    h.session.recovery_loaded(Ok(Some(empty)));
    assert_eq!(h.session.recovery(), &RecoveryState::None);

    h.session.recovery_loaded(Err("bad json".into()));
    assert_eq!(h.session.recovery(), &RecoveryState::None);
    assert_eq!(
        h.session.playback_error(),
        Some("Queue recovery could not be read: bad json")
    );
}

#[test]
fn shutdown_closes_log_and_saves_clean_snapshot() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    let b = h.add(local("b"));
    h.prepare_all();
    h.play(&a);
    h.clear();
    let effects = h.session.shutdown();
    assert!(effects.contains(&Effect::Audio(AudioCommand::Stop)));
    let snapshot = effects
        .iter()
        .find_map(|effect| match effect {
            Effect::SaveRecovery(snapshot) => Some(snapshot.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(snapshot.status, RecoveryStatus::Clean);
    assert_eq!(
        snapshot
            .queue
            .iter()
            .map(StoredQueueItem::id)
            .collect::<Vec<_>>(),
        vec![a.as_str(), b.as_str()]
    );
    assert_eq!(h.log_reason(&a), Some(PlayLogReason::Skipped));
    assert!(h.session.queue().is_stopped());
}

#[test]
fn play_log_entries_are_saved_on_start_and_finish() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    h.prepare_all();
    h.clear();
    h.play(&a);
    let saved: Vec<PlayLogEntry> = h
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::SavePlayLogEntry(entry) => Some(entry.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(saved.len(), 1);
    assert!(saved[0].ended_at.is_none());
    h.clear();
    h.tick(10);
    h.skip();
    let saved: Vec<PlayLogEntry> = h
        .effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::SavePlayLogEntry(entry) => Some(entry.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].reason, Some(PlayLogReason::Skipped));
    assert!(saved[0].ended_at.as_deref() > Some(saved[0].started_at.as_str()));
}

#[test]
fn play_log_loading_merges_with_entries_from_this_launch() {
    let mut h = Harness::new();
    let a = h.add(local("a"));
    h.prepare_all();
    h.play(&a);
    let current = h.session.play_log()[0].clone();
    let older = PlayLogEntry {
        id: "old".into(),
        item: StoredQueueItem::Break { id: "x".into() },
        started_at: "2020-01-01T00:00:00.000Z".into(),
        ended_at: Some("2020-01-01T00:01:00.000Z".into()),
        reason: Some(PlayLogReason::Skipped),
    };
    h.session.play_log_loaded(Ok(vec![older.clone()]));
    assert_eq!(h.session.play_log(), &[current, older]);
    let effects = h.session.clear_play_log();
    assert_eq!(effects, vec![Effect::ClearPlayLog]);
    assert!(h.session.play_log().is_empty());
}

#[test]
fn insert_break_goes_above_the_selection_or_at_the_end() {
    let mut h = Harness::new();
    let is_break = |h: &Harness, id: &str| {
        matches!(h.session.queue().item(id).unwrap().kind(), ItemKind::Break)
    };
    let effects = h.session.insert_break(None);
    h.record(effects);
    assert_eq!(h.ids().len(), 1, "an empty Queue gets the Break");
    let a = h.add(local("a"));
    let b = h.add(local("b"));
    let c = h.add(local("c"));
    h.prepare_all();
    h.play(&a);

    // Nothing selected: the end of the Queue, even while something plays.
    let effects = h.session.insert_break(None);
    h.record(effects);
    let ids = h.ids();
    assert_eq!(ids[1..4], [b.clone(), c.clone(), ids[3].clone()]);
    assert!(is_break(&h, &ids[3]));

    // `c` selected: right above it.
    let effects = h.session.insert_break(Some(&c));
    h.record(effects);
    let ids = h.ids();
    assert_eq!(ids[1], b);
    assert!(is_break(&h, &ids[2]));
    assert_eq!(ids[3], c);
}

#[test]
fn active_log_entry_tracks_break_start_time() {
    let mut h = Harness::new();
    let br = h.add_break();
    h.play(&br);
    let entry = h.session.active_log_entry().unwrap();
    assert_eq!(entry.item.id(), br);
    assert!(entry.ended_at.is_none());
}
