//! End-to-end tests: real decoding and the real control thread, with a fake
//! output device that runs the mixer faster than real time.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::decoder::spawn_decoder;
use crate::decoder::tests::write_wav;
use crate::mixer::{Mixer, MixerCommand, MixerEvent, Track};
use crate::output::fake::{self, FakeControls};
use crate::{AudioEngine, AudioEvent};

const FAKE_RATE: u32 = 8_000;

#[test]
fn engine_is_send_and_sync() {
    fn check<T: Send + Sync>() {}
    check::<AudioEngine>();
}

/// Decodes a file through a real decoder thread into the mixer and returns
/// the frame count reported when it ended.
fn play_through_mixer(path: &Path, rate: u32, channels: usize) -> u64 {
    let (mut mixer, mut handles) = Mixer::new(rate, channels);
    let (track, producer, shared) = Track::new(1, rate, channels);
    let cancel = Arc::new(AtomicBool::new(false));
    let failed = Arc::new(Mutex::new(None));
    let failed_slot = Arc::clone(&failed);
    let decoder = spawn_decoder(
        path.to_path_buf(),
        rate,
        channels,
        producer,
        shared,
        cancel,
        move |message| *failed_slot.lock().unwrap() = Some(message),
    )
    .unwrap();
    assert!(handles.commands.push(MixerCommand::Play(track)).is_ok());

    let mut buffer = vec![0.0; 512 * channels];
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "never ended");
        mixer.render(&mut buffer);
        while let Ok(event) = handles.events.pop() {
            if let MixerEvent::Ended { frames_played, .. } = event {
                decoder.join().unwrap();
                assert!(failed.lock().unwrap().is_none());
                return frames_played;
            }
        }
        thread::sleep(Duration::from_micros(200));
    }
}

#[test]
fn decode_to_end_plays_every_frame() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.wav");
    write_wav(&path, 8_000, 1, 4_321, 300.0);
    assert_eq!(play_through_mixer(&path, 8_000, 2), 4_321);
}

#[test]
fn decode_to_end_with_resampling_plays_expected_frames() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cd.wav");
    write_wav(&path, 44_100, 2, 44_100, 440.0);
    assert_eq!(play_through_mixer(&path, 48_000, 2), 48_000);
}

/// An engine on the fake device plus everything it reported.
struct TestEngine {
    engine: AudioEngine,
    events: Arc<Mutex<Vec<AudioEvent>>>,
    controls: FakeControls,
    _dir: tempfile::TempDir,
    dir: PathBuf,
}

impl TestEngine {
    fn new() -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        let controls = FakeControls::default();
        let engine = AudioEngine::with_output(
            Box::new(move |event| sink.lock().unwrap().push(event)),
            fake::opener(FAKE_RATE, 2, controls.clone()),
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        TestEngine {
            engine,
            events,
            controls,
            _dir: dir,
            dir: path,
        }
    }

    /// Writes a WAV of `secs` seconds and returns its path.
    fn wav(&self, name: &str, secs: f64) -> PathBuf {
        let path = self.dir.join(name);
        write_wav(
            &path,
            FAKE_RATE,
            2,
            (secs * f64::from(FAKE_RATE)) as usize,
            200.0,
        );
        path
    }

    fn wait_for(&self, what: &str, predicate: impl Fn(&AudioEvent) -> bool) -> AudioEvent {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(event) = self.events.lock().unwrap().iter().find(|e| predicate(e)) {
                return event.clone();
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {:?}",
                self.events()
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn events(&self) -> Vec<AudioEvent> {
        self.events.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.events.lock().unwrap().clear();
    }

    fn non_progress(&self) -> Vec<AudioEvent> {
        self.events()
            .into_iter()
            .filter(|e| !matches!(e, AudioEvent::Progress { .. }))
            .collect()
    }
}

#[test]
fn plays_to_end_with_progress_then_ended() {
    let t = TestEngine::new();
    let path = t.wav("a.wav", 0.5);
    t.engine.play(1, path);
    t.wait_for("ended", |e| *e == AudioEvent::Ended { id: 1 });
    thread::sleep(Duration::from_millis(50));

    let events = t.events();
    let positions: Vec<f64> = events
        .iter()
        .filter_map(|e| match e {
            AudioEvent::Progress {
                id: 1,
                position_secs,
                duration_secs,
            } => {
                assert_eq!(*duration_secs, Some(0.5));
                Some(*position_secs)
            }
            _ => None,
        })
        .collect();
    assert!(positions.len() >= 3, "{positions:?}");
    assert!(positions.windows(2).all(|w| w[0] <= w[1]));
    assert!(positions.iter().all(|p| *p <= 0.5));
    // Ended is the last thing reported.
    assert_eq!(events.last(), Some(&AudioEvent::Ended { id: 1 }));
}

#[test]
fn missing_file_reports_failed() {
    let t = TestEngine::new();
    t.engine.play(9, t.dir.join("nope.mp3"));
    let event = t.wait_for("failure", |e| matches!(e, AudioEvent::Failed { id: 9, .. }));
    let AudioEvent::Failed { message, .. } = event else {
        unreachable!()
    };
    assert!(message.contains("Could not open"), "{message}");
}

#[test]
fn stop_suppresses_later_events() {
    let t = TestEngine::new();
    let path = t.wav("long.wav", 30.0);
    t.engine.play(1, path);
    t.wait_for("progress", |e| {
        matches!(e, AudioEvent::Progress { id: 1, .. })
    });
    t.engine.stop();
    thread::sleep(Duration::from_millis(50));
    t.clear();
    thread::sleep(Duration::from_millis(300));
    assert!(t.events().is_empty(), "{:?}", t.events());
}

#[test]
fn active_end_during_crossfade_auto_promotes() {
    let t = TestEngine::new();
    t.engine.play(1, t.wav("a.wav", 0.3));
    t.wait_for("a progress", |e| {
        matches!(e, AudioEvent::Progress { id: 1, .. })
    });
    // Fade much longer than what is left of A.
    t.engine
        .crossfade(2, t.wav("b.wav", 0.5), Duration::from_secs(2));
    t.wait_for("b ended", |e| *e == AudioEvent::Ended { id: 2 });
    assert_eq!(
        t.non_progress(),
        vec![AudioEvent::Ended { id: 1 }, AudioEvent::Ended { id: 2 }]
    );
}

#[test]
fn crossfade_complete_then_promote() {
    let t = TestEngine::new();
    t.engine.play(1, t.wav("a.wav", 20.0));
    t.wait_for("a progress", |e| {
        matches!(e, AudioEvent::Progress { id: 1, .. })
    });
    t.engine
        .crossfade(2, t.wav("b.wav", 1.0), Duration::from_millis(200));
    t.wait_for("complete", |e| {
        *e == AudioEvent::CrossfadeComplete {
            outgoing: 1,
            incoming: 2,
        }
    });
    // Stale/unknown ids are ignored.
    t.engine.promote_incoming(1);
    t.engine.promote_incoming(77);
    t.engine.promote_incoming(2);
    t.wait_for("b ended", |e| *e == AudioEvent::Ended { id: 2 });
    thread::sleep(Duration::from_millis(100));
    assert_eq!(
        t.non_progress(),
        vec![
            AudioEvent::CrossfadeComplete {
                outgoing: 1,
                incoming: 2
            },
            AudioEvent::Ended { id: 2 }
        ]
    );
}

#[test]
fn failing_incoming_is_reported_and_outgoing_continues() {
    let t = TestEngine::new();
    t.engine.play(1, t.wav("a.wav", 20.0));
    t.wait_for("a progress", |e| {
        matches!(e, AudioEvent::Progress { id: 1, .. })
    });
    t.engine
        .crossfade(2, t.dir.join("missing.m4a"), Duration::from_secs(1));
    t.wait_for("b failed", |e| {
        matches!(e, AudioEvent::Failed { id: 2, .. })
    });
    t.clear();
    t.wait_for("a still playing", |e| {
        matches!(e, AudioEvent::Progress { id: 1, .. })
    });
}

#[test]
fn device_failure_reports_loaded_ids_and_reopens() {
    let t = TestEngine::new();
    t.engine.play(1, t.wav("a.wav", 20.0));
    t.wait_for("a progress", |e| {
        matches!(e, AudioEvent::Progress { id: 1, .. })
    });
    t.controls.fail_device("unplugged");
    let event = t.wait_for("failure", |e| matches!(e, AudioEvent::Failed { id: 1, .. }));
    assert!(matches!(event, AudioEvent::Failed { message, .. } if message.contains("unplugged")));
    thread::sleep(Duration::from_millis(100));
    t.clear();
    thread::sleep(Duration::from_millis(100));
    assert!(t.events().is_empty());

    t.engine.play(2, t.wav("b.wav", 0.2));
    t.wait_for("b ended", |e| *e == AudioEvent::Ended { id: 2 });
    assert_eq!(t.controls.opened.load(Ordering::Relaxed), 2);
}

#[test]
fn stalled_device_is_detected() {
    let t = TestEngine::new();
    t.engine.play(1, t.wav("a.wav", 20.0));
    t.wait_for("a progress", |e| {
        matches!(e, AudioEvent::Progress { id: 1, .. })
    });
    t.controls.stall.store(true, Ordering::Relaxed);
    let event = t.wait_for("failure", |e| matches!(e, AudioEvent::Failed { id: 1, .. }));
    assert!(
        matches!(event, AudioEvent::Failed { message, .. } if message.contains("stopped responding"))
    );
}

#[test]
fn replaying_same_id_does_not_leak_old_events() {
    let t = TestEngine::new();
    t.engine.play(1, t.wav("a.wav", 0.15));
    t.engine.play(1, t.wav("b.wav", 0.4));
    t.wait_for("ended", |e| *e == AudioEvent::Ended { id: 1 });
    thread::sleep(Duration::from_millis(100));
    // Only the second load ends; the first was replaced before ending.
    assert_eq!(t.non_progress(), vec![AudioEvent::Ended { id: 1 }]);
}

#[test]
fn drop_shuts_down_promptly() {
    let t = TestEngine::new();
    t.engine.play(1, t.wav("a.wav", 20.0));
    t.wait_for("a progress", |e| {
        matches!(e, AudioEvent::Progress { id: 1, .. })
    });
    let started = Instant::now();
    drop(t);
    assert!(started.elapsed() < Duration::from_secs(1));
}
