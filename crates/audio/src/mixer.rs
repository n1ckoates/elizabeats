//! The real-time mixing core.
//!
//! [`Mixer`] owns the two decks and runs inside the audio output callback.
//! It is a plain struct with no device dependency, so all playback rules
//! (gain ramps, crossfades, auto-promotion, end detection, positions and
//! event generation) are unit-tested by calling [`Mixer::render`] directly.
//!
//! Real-time rules: `render` never allocates, locks, blocks or does I/O.
//! Everything it talks to is a lock-free single-producer/single-consumer
//! ring buffer:
//! - commands arrive from the control thread,
//! - events and finished tracks (for deallocation off the audio thread)
//!   go back to it,
//! - each loaded track's decoded samples arrive from its decoder thread.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};

/// Internal identity of one load of one file. Unlike the caller's `LoadId`,
/// a key is never reused, so stale events can always be told apart.
pub(crate) type TrackKey = u64;

/// Length of the fade applied to abrupt starts and stops, to avoid clicks.
const DECLICK_SECS: f64 = 0.005;
/// How quickly the active deck comes back to full gain when an incoming
/// track disappears mid-crossfade (so the outgoing track never fades to dead air).
const RESTORE_SECS: f64 = 0.5;
/// How much audio a deck must have buffered before it starts (avoids an
/// immediate stutter while the decoder spins up).
const PRIME_SECS: f64 = 0.1;
/// Interval between `Progress` events per deck.
const PROGRESS_SECS: f64 = 0.1;

const COMMAND_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 512;
const GARBAGE_CAPACITY: usize = 32;
/// Tracks that are fading out after being stopped/replaced.
const MAX_TAILS: usize = 4;

/// State shared between a track's decoder thread and the mixer.
#[derive(Debug)]
pub(crate) struct TrackShared {
    finished: AtomicBool,
    duration_bits: AtomicU64,
}

const UNKNOWN_DURATION: u64 = u64::MAX;

impl TrackShared {
    fn new() -> Self {
        Self {
            finished: AtomicBool::new(false),
            duration_bits: AtomicU64::new(UNKNOWN_DURATION),
        }
    }

    pub(crate) fn set_duration(&self, secs: Option<f64>) {
        let bits = match secs {
            Some(secs) if secs.is_finite() && secs >= 0.0 => secs.to_bits(),
            _ => UNKNOWN_DURATION,
        };
        self.duration_bits.store(bits, Ordering::Relaxed);
    }

    pub(crate) fn duration_secs(&self) -> Option<f64> {
        match self.duration_bits.load(Ordering::Relaxed) {
            UNKNOWN_DURATION => None,
            bits => Some(f64::from_bits(bits)),
        }
    }

    /// Called by the decoder after its last sample has been pushed.
    pub(crate) fn mark_finished(&self) {
        self.finished.store(true, Ordering::Release);
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::Acquire)
    }
}

/// One loaded file as seen by the mixer.
pub(crate) struct Track {
    key: TrackKey,
    feed: Consumer<f32>,
    shared: Arc<TrackShared>,
    /// Frames taken from `feed` and mixed into the output (the play position).
    frames_played: u64,
    frames_until_progress: u64,
    /// Becomes true once enough audio is buffered to start rendering.
    primed: bool,
}

impl Track {
    /// Creates a track plus the producer end its decoder writes interleaved
    /// samples (already at the device rate and channel count) into.
    /// The buffer holds about one second of audio.
    pub(crate) fn new(
        key: TrackKey,
        sample_rate: u32,
        channels: usize,
    ) -> (Track, Producer<f32>, Arc<TrackShared>) {
        let channels = channels.max(1);
        // A whole number of frames, so reads and writes never split a frame.
        let capacity = sample_rate.max(1) as usize * channels;
        let (producer, feed) = RingBuffer::new(capacity);
        let shared = Arc::new(TrackShared::new());
        let track = Track {
            key,
            feed,
            shared: Arc::clone(&shared),
            frames_played: 0,
            frames_until_progress: 0,
            primed: false,
        };
        (track, producer, shared)
    }

    fn is_drained(&self) -> bool {
        // Order matters: check `finished` first. If it is set, every sample
        // has already been pushed, so an empty buffer really is the end.
        self.shared.is_finished() && self.feed.is_empty()
    }
}

/// Instructions from the control thread.
pub(crate) enum MixerCommand {
    /// Retire everything and start `track` on the active deck.
    Play(Track),
    /// Start `track` on the idle deck and crossfade over `fade_frames`.
    Crossfade { track: Track, fade_frames: u64 },
    /// Make the incoming deck active if it holds `key`.
    Promote { key: TrackKey },
    /// Retire everything.
    Stop,
    /// The decoder for `key` failed: drop it and report.
    Fail { key: TrackKey },
}

/// Notifications for the control thread. All variants are `Copy` so they
/// can be produced without allocating.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum MixerEvent {
    Progress {
        key: TrackKey,
        position_secs: f64,
        duration_secs: Option<f64>,
    },
    Ended {
        key: TrackKey,
        /// Total frames played, for diagnostics and tests.
        frames_played: u64,
    },
    Failed {
        key: TrackKey,
    },
    CrossfadeComplete {
        outgoing: TrackKey,
        incoming: TrackKey,
    },
    /// The track left the mixer because of a command (stop, replace, promote).
    Unloaded {
        key: TrackKey,
    },
}

/// A linear gain ramp evaluated per frame. The gain for frame `n` of the
/// ramp is exactly `from + (to - from) * n / len`, so two ramps started on
/// the same frame stay perfectly aligned.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Ramp {
    from: f32,
    to: f32,
    len: u64,
    pos: u64,
}

impl Ramp {
    pub(crate) fn constant(value: f32) -> Self {
        Self {
            from: value,
            to: value,
            len: 0,
            pos: 0,
        }
    }

    pub(crate) fn new(from: f32, to: f32, len: u64) -> Self {
        Self {
            from,
            to,
            len,
            pos: 0,
        }
    }

    /// Gain for the next frame (without advancing).
    pub(crate) fn value(&self) -> f32 {
        if self.pos >= self.len {
            self.to
        } else {
            let t = self.pos as f64 / self.len as f64;
            self.from + (self.to - self.from) * t as f32
        }
    }

    /// Returns the gain for the current frame and moves to the next one.
    fn next_gain(&mut self) -> f32 {
        let value = self.value();
        self.advance(1);
        value
    }

    fn advance(&mut self, frames: u64) {
        self.pos = self.pos.saturating_add(frames).min(self.len);
    }

    fn is_done(&self) -> bool {
        self.pos >= self.len
    }

    fn target(&self) -> f32 {
        self.to
    }
}

struct Deck {
    track: Option<Track>,
    gain: Ramp,
}

impl Deck {
    fn empty() -> Self {
        Self {
            track: None,
            gain: Ramp::constant(0.0),
        }
    }

    fn key(&self) -> Option<TrackKey> {
        self.track.as_ref().map(|track| track.key)
    }
}

/// A track that was stopped abruptly and is fading out before release.
struct Tail {
    track: Track,
    gain: Ramp,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CrossfadeState {
    Idle,
    /// Waiting for the incoming track to buffer enough audio.
    Pending {
        fade_frames: u64,
    },
    /// Both ramps are running.
    Ramping {
        outgoing: TrackKey,
        incoming: TrackKey,
    },
}

/// The control thread's ends of the mixer's queues.
pub(crate) struct MixerHandles {
    pub(crate) commands: Producer<MixerCommand>,
    pub(crate) events: Consumer<MixerEvent>,
    /// Tracks the mixer is finished with, returned so they are freed off the
    /// audio thread.
    pub(crate) garbage: Consumer<Track>,
}

pub(crate) struct Mixer {
    channels: usize,
    sample_rate: u32,
    decks: [Deck; 2],
    /// Index of the active deck; the other one is the incoming deck.
    active: usize,
    crossfade: CrossfadeState,
    tails: [Option<Tail>; MAX_TAILS],
    commands: Consumer<MixerCommand>,
    events: Producer<MixerEvent>,
    garbage: Producer<Track>,
    declick_frames: u64,
    restore_frames: u64,
    prime_samples: usize,
    progress_frames: u64,
}

fn secs_to_frames(secs: f64, sample_rate: u32) -> u64 {
    let frames = (secs * f64::from(sample_rate)).round();
    if frames.is_finite() && frames > 0.0 {
        frames as u64
    } else {
        0
    }
}

impl Mixer {
    pub(crate) fn new(sample_rate: u32, channels: usize) -> (Mixer, MixerHandles) {
        let sample_rate = sample_rate.max(1);
        let channels = channels.max(1);
        let (command_tx, command_rx) = RingBuffer::new(COMMAND_CAPACITY);
        let (event_tx, event_rx) = RingBuffer::new(EVENT_CAPACITY);
        let (garbage_tx, garbage_rx) = RingBuffer::new(GARBAGE_CAPACITY);
        let mixer = Mixer {
            channels,
            sample_rate,
            decks: [Deck::empty(), Deck::empty()],
            active: 0,
            crossfade: CrossfadeState::Idle,
            tails: std::array::from_fn(|_| None),
            commands: command_rx,
            events: event_tx,
            garbage: garbage_tx,
            declick_frames: secs_to_frames(DECLICK_SECS, sample_rate).max(1),
            restore_frames: secs_to_frames(RESTORE_SECS, sample_rate).max(1),
            prime_samples: secs_to_frames(PRIME_SECS, sample_rate) as usize * channels,
            progress_frames: secs_to_frames(PROGRESS_SECS, sample_rate).max(1),
        };
        let handles = MixerHandles {
            commands: command_tx,
            events: event_rx,
            garbage: garbage_rx,
        };
        (mixer, handles)
    }

    #[cfg(test)]
    fn declick_frames(&self) -> u64 {
        self.declick_frames
    }

    fn incoming(&self) -> usize {
        1 - self.active
    }

    /// Renders one buffer of interleaved output. `out.len()` should be a
    /// multiple of the channel count; a trailing partial frame is silenced.
    pub(crate) fn render(&mut self, out: &mut [f32]) {
        while let Ok(command) = self.commands.pop() {
            self.apply(command);
        }

        out.fill(0.0);
        let channels = self.channels;
        let frames = out.len() / channels;
        let out = &mut out[..frames * channels];

        self.prime_decks();
        self.start_pending_crossfade();

        for deck in &mut self.decks {
            if let Some(track) = deck.track.as_mut().filter(|track| track.primed) {
                mix_track(track, &mut deck.gain, out, channels);
            }
        }
        for slot in 0..MAX_TAILS {
            let finished = match self.tails[slot].as_mut() {
                Some(tail) => {
                    mix_track(&mut tail.track, &mut tail.gain, out, channels);
                    tail.gain.is_done()
                }
                None => false,
            };
            if finished && let Some(tail) = self.tails[slot].take() {
                self.recycle(tail.track);
            }
        }

        for sample in out.iter_mut() {
            *sample = sample.clamp(-1.0, 1.0);
        }

        self.emit_progress(frames as u64);
        self.check_crossfade_complete();
        self.detect_ends();
    }

    fn apply(&mut self, command: MixerCommand) {
        match command {
            MixerCommand::Play(track) => self.play(track),
            MixerCommand::Crossfade { track, fade_frames } => {
                self.start_crossfade(track, fade_frames)
            }
            MixerCommand::Promote { key } => self.promote(key),
            MixerCommand::Stop => {
                self.retire_deck(self.active);
                self.retire_deck(self.incoming());
                self.crossfade = CrossfadeState::Idle;
            }
            MixerCommand::Fail { key } => self.fail(key),
        }
    }

    fn play(&mut self, track: Track) {
        self.retire_deck(self.active);
        self.retire_deck(self.incoming());
        self.crossfade = CrossfadeState::Idle;
        let active = self.active;
        self.decks[active] = Deck {
            track: Some(track),
            gain: Ramp::new(0.0, 1.0, self.declick_frames),
        };
    }

    fn start_crossfade(&mut self, track: Track, fade_frames: u64) {
        if self.decks[self.active].track.is_none() {
            // Nothing to fade from: just start it.
            self.play(track);
            return;
        }
        if self.decks[self.incoming()].track.is_some() {
            // A previous crossfade is still going: drop its outgoing track
            // and continue from the previous incoming one.
            self.retire_deck(self.active);
            self.active = self.incoming();
        }
        let incoming = self.incoming();
        self.decks[incoming] = Deck {
            track: Some(track),
            gain: Ramp::constant(0.0),
        };
        self.crossfade = CrossfadeState::Pending { fade_frames };
    }

    fn promote(&mut self, key: TrackKey) {
        let incoming = self.incoming();
        if self.decks[incoming].key() != Some(key) {
            return;
        }
        self.retire_deck(self.active);
        self.active = incoming;
        let deck = &mut self.decks[incoming];
        deck.gain = Ramp::new(deck.gain.value(), 1.0, self.declick_frames);
        self.crossfade = CrossfadeState::Idle;
    }

    fn fail(&mut self, key: TrackKey) {
        let deck_index = if self.decks[self.active].key() == Some(key) {
            self.active
        } else if self.decks[self.incoming()].key() == Some(key) {
            self.incoming()
        } else {
            return;
        };
        let deck = std::mem::replace(&mut self.decks[deck_index], Deck::empty());
        if let Some(track) = deck.track {
            self.fade_out(track, deck.gain.value());
        }
        self.emit(MixerEvent::Failed { key });
        self.after_deck_lost(deck_index);
    }

    /// Fixes up deck roles after the track on `deck_index` went away on its
    /// own (ended or failed).
    fn after_deck_lost(&mut self, deck_index: usize) {
        if deck_index == self.active {
            self.promote_after_active_lost();
        } else {
            self.restore_after_incoming_lost();
        }
    }

    /// The active track is gone. If a crossfade is under way the incoming
    /// track takes over and carries on with its fade-in undisturbed.
    fn promote_after_active_lost(&mut self) {
        let incoming = self.incoming();
        if self.decks[incoming].track.is_some() {
            if let CrossfadeState::Pending { fade_frames } = self.crossfade {
                // Its ramp had not started yet; fade in as originally planned.
                self.decks[incoming].gain =
                    Ramp::new(0.0, 1.0, fade_frames.max(self.declick_frames));
            }
            self.active = incoming;
        }
        self.crossfade = CrossfadeState::Idle;
    }

    /// The incoming track is gone before being promoted. Bring the active
    /// track back up so a failed or very short incoming track never leaves
    /// dead air.
    fn restore_after_incoming_lost(&mut self) {
        let deck = &mut self.decks[self.active];
        if deck.track.is_some() && deck.gain.target() < 1.0 {
            deck.gain = Ramp::new(deck.gain.value(), 1.0, self.restore_frames);
        }
        self.crossfade = CrossfadeState::Idle;
    }

    /// Removes the deck's track because of a command, fading it out.
    fn retire_deck(&mut self, deck_index: usize) {
        let deck = std::mem::replace(&mut self.decks[deck_index], Deck::empty());
        if let Some(track) = deck.track {
            self.emit(MixerEvent::Unloaded { key: track.key });
            self.fade_out(track, deck.gain.value());
        }
    }

    /// Moves a track to the tail list for a short de-click fade, or releases
    /// it straight away if it is not audible.
    fn fade_out(&mut self, track: Track, gain: f32) {
        if !track.primed || gain <= 0.0 {
            self.recycle(track);
            return;
        }
        let tail = Tail {
            track,
            gain: Ramp::new(gain, 0.0, self.declick_frames),
        };
        let slot = match self.tails.iter().position(Option::is_none) {
            Some(slot) => slot,
            None => {
                // All tail slots busy (many rapid commands): cut the oldest.
                if let Some(old) = self.tails[0].take() {
                    self.recycle(old.track);
                }
                0
            }
        };
        self.tails[slot] = Some(tail);
    }

    fn prime_decks(&mut self) {
        let prime_samples = self.prime_samples;
        for deck in &mut self.decks {
            if let Some(track) = deck.track.as_mut()
                && !track.primed
                && (track.feed.slots() >= prime_samples || track.shared.is_finished())
            {
                track.primed = true;
            }
        }
    }

    fn start_pending_crossfade(&mut self) {
        let CrossfadeState::Pending { fade_frames } = self.crossfade else {
            return;
        };
        let incoming = self.incoming();
        let (Some(incoming_key), Some(outgoing_key)) =
            (self.decks[incoming].key(), self.decks[self.active].key())
        else {
            return;
        };
        // Wait until the incoming track has audio. An incoming track that is
        // already over (e.g. an empty file) never starts a fade; it is
        // reported as ended instead.
        let incoming_ready = self.decks[incoming]
            .track
            .as_ref()
            .is_some_and(|track| track.primed && !track.is_drained());
        if !incoming_ready {
            return;
        }
        // A zero-length crossfade still gets the de-click ramp.
        let len = fade_frames.max(self.declick_frames);
        let active = &mut self.decks[self.active];
        active.gain = Ramp::new(active.gain.value(), 0.0, len);
        self.decks[incoming].gain = Ramp::new(0.0, 1.0, len);
        self.crossfade = CrossfadeState::Ramping {
            outgoing: outgoing_key,
            incoming: incoming_key,
        };
    }

    fn check_crossfade_complete(&mut self) {
        if let CrossfadeState::Ramping { outgoing, incoming } = self.crossfade
            && self.decks[self.active].gain.is_done()
            && self.decks[self.incoming()].gain.is_done()
        {
            self.crossfade = CrossfadeState::Idle;
            self.emit(MixerEvent::CrossfadeComplete { outgoing, incoming });
        }
    }

    fn emit_progress(&mut self, frames: u64) {
        let sample_rate = f64::from(self.sample_rate);
        let interval = self.progress_frames;
        for deck_index in [self.active, self.incoming()] {
            let Some(track) = self.decks[deck_index].track.as_mut() else {
                continue;
            };
            if !track.primed {
                continue;
            }
            if track.frames_until_progress > frames {
                track.frames_until_progress -= frames;
                continue;
            }
            track.frames_until_progress = interval;
            let event = MixerEvent::Progress {
                key: track.key,
                position_secs: track.frames_played as f64 / sample_rate,
                duration_secs: track.shared.duration_secs(),
            };
            self.emit(event);
        }
    }

    fn detect_ends(&mut self) {
        for deck_index in [self.active, self.incoming()] {
            let ended = self.decks[deck_index]
                .track
                .as_ref()
                .is_some_and(|track| track.primed && track.is_drained());
            if !ended {
                continue;
            }
            let deck = std::mem::replace(&mut self.decks[deck_index], Deck::empty());
            if let Some(track) = deck.track {
                let event = MixerEvent::Ended {
                    key: track.key,
                    frames_played: track.frames_played,
                };
                self.recycle(track);
                self.emit(event);
            }
            self.after_deck_lost(deck_index);
            // Roles may have changed; the other deck is checked on the next buffer.
            break;
        }
    }

    fn emit(&mut self, event: MixerEvent) {
        // If the control thread has fallen far behind the event is dropped
        // rather than blocking the audio thread.
        let _ = self.events.push(event);
    }

    /// Hands a finished track back to the control thread to be freed there.
    fn recycle(&mut self, track: Track) {
        if let Err(rtrb::PushError::Full(track)) = self.garbage.push(track) {
            // Should not happen; freeing here is a rare, small cost.
            drop(track);
        }
    }
}

/// Mixes up to one buffer of `track` into `out` with `gain` applied per
/// frame. On underrun the missing frames are silent; the ramp still advances
/// so every deck stays on the same timeline.
fn mix_track(track: &mut Track, gain: &mut Ramp, out: &mut [f32], channels: usize) {
    let frames = out.len() / channels;
    let available = track.feed.slots() / channels;
    let wanted = available.min(frames);
    let mut mixed = 0;
    if wanted > 0
        && let Ok(chunk) = track.feed.read_chunk(wanted * channels)
    {
        let (first, second) = chunk.as_slices();
        let mut samples = first.iter().chain(second.iter());
        for frame in out.chunks_exact_mut(channels).take(wanted) {
            let g = gain.next_gain();
            for (slot, sample) in frame.iter_mut().zip(&mut samples) {
                *slot += sample * g;
            }
        }
        chunk.commit_all();
        mixed = wanted;
    }
    gain.advance((frames - mixed) as u64);
    track.frames_played += mixed as u64;
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 1000; // 1 frame = 1 ms keeps the arithmetic readable.

    /// Test harness: a mixer plus the producers feeding its tracks.
    struct Rig {
        mixer: Mixer,
        handles: MixerHandles,
        channels: usize,
    }

    struct Feed {
        producer: Producer<f32>,
        shared: Arc<TrackShared>,
    }

    impl Feed {
        /// Pushes `frames` frames of a constant value.
        fn push(&mut self, frames: usize, value: f32, channels: usize) {
            for _ in 0..frames * channels {
                self.producer.push(value).unwrap();
            }
        }
        fn finish(&self) {
            self.shared.mark_finished();
        }
    }

    impl Rig {
        fn new(channels: usize) -> Self {
            let (mixer, handles) = Mixer::new(RATE, channels);
            Rig {
                mixer,
                handles,
                channels,
            }
        }

        fn track(&self, key: TrackKey) -> (Track, Feed) {
            let (track, producer, shared) = Track::new(key, RATE, self.channels);
            (track, Feed { producer, shared })
        }

        fn send(&mut self, command: MixerCommand) {
            assert!(self.handles.commands.push(command).is_ok());
        }

        fn play(&mut self, key: TrackKey, frames: usize, value: f32) -> Feed {
            let (track, mut feed) = self.track(key);
            feed.push(frames, value, self.channels);
            self.send(MixerCommand::Play(track));
            feed
        }

        fn render(&mut self, frames: usize) -> Vec<f32> {
            let mut out = vec![9.0; frames * self.channels];
            self.mixer.render(&mut out);
            out
        }

        fn events(&mut self) -> Vec<MixerEvent> {
            let mut events = Vec::new();
            while let Ok(event) = self.handles.events.pop() {
                events.push(event);
            }
            events
        }

        /// Events other than progress, with `Ended` frame counts zeroed so
        /// they compare equal to [`ended`].
        fn non_progress_events(&mut self) -> Vec<MixerEvent> {
            self.events()
                .into_iter()
                .filter(|event| !matches!(event, MixerEvent::Progress { .. }))
                .map(|event| match event {
                    MixerEvent::Ended { key, .. } => ended(key),
                    other => other,
                })
                .collect()
        }

        fn garbage_count(&mut self) -> usize {
            let mut count = 0;
            while self.handles.garbage.pop().is_ok() {
                count += 1;
            }
            count
        }
    }

    fn ended(key: TrackKey) -> MixerEvent {
        MixerEvent::Ended {
            key,
            frames_played: 0,
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn ramp_is_linear_and_sample_accurate() {
        let mut ramp = Ramp::new(0.0, 1.0, 4);
        let values: Vec<f32> = (0..6).map(|_| ramp.next_gain()).collect();
        assert_eq!(values, vec![0.0, 0.25, 0.5, 0.75, 1.0, 1.0]);
        assert!(ramp.is_done());

        let mut down = Ramp::new(0.8, 0.0, 8);
        down.advance(2);
        assert!(close(down.value(), 0.6));
        assert_eq!(Ramp::constant(0.3).value(), 0.3);
    }

    #[test]
    fn play_fades_in_over_declick_then_full_gain() {
        let mut rig = Rig::new(1);
        let declick = rig.mixer.declick_frames() as usize;
        assert_eq!(declick, 5);
        let _feed = rig.play(1, 200, 1.0);
        let out = rig.render(10);
        assert_eq!(out, vec![0.0, 0.2, 0.4, 0.6, 0.8, 1.0, 1.0, 1.0, 1.0, 1.0]);
    }

    #[test]
    fn waits_for_priming_before_starting() {
        let mut rig = Rig::new(1);
        let mut feed = rig.play(1, 50, 1.0); // below the 100 ms prime level
        assert_eq!(rig.render(10), vec![0.0; 10]);
        assert!(rig.events().is_empty());
        feed.push(60, 1.0, 1);
        let out = rig.render(10);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[9], 1.0);
    }

    #[test]
    fn crossfade_ramps_are_linear_and_complementary() {
        let mut rig = Rig::new(1);
        let _a = rig.play(1, 1000, 1.0);
        rig.render(20); // past the de-click
        rig.events();

        let (incoming, mut b) = rig.track(2);
        b.push(1000, 0.5, 1);
        rig.send(MixerCommand::Crossfade {
            track: incoming,
            fade_frames: 100,
        });
        let out = rig.render(100);
        for (n, sample) in out.iter().enumerate() {
            let t = n as f32 / 100.0;
            let expected = (1.0 - t) * 1.0 + t * 0.5;
            assert!(
                close(*sample, expected),
                "frame {n}: {sample} vs {expected}"
            );
        }
        assert_eq!(
            rig.non_progress_events(),
            vec![MixerEvent::CrossfadeComplete {
                outgoing: 1,
                incoming: 2
            }]
        );
        // Outgoing is silent but still playing.
        assert!(rig.render(10).iter().all(|s| close(*s, 0.5)));
        assert!(rig.non_progress_events().is_empty());
    }

    #[test]
    fn crossfade_ramp_spans_buffer_boundaries_exactly() {
        let mut rig = Rig::new(2);
        let _a = rig.play(1, 1000, 1.0);
        rig.render(20);
        let (incoming, mut b) = rig.track(2);
        b.push(1000, 0.0, 2);
        rig.send(MixerCommand::Crossfade {
            track: incoming,
            fade_frames: 64,
        });
        let mut all = Vec::new();
        for size in [7, 13, 1, 50] {
            all.extend(rig.render(size));
        }
        for (frame, pair) in all.chunks(2).enumerate().take(64) {
            let expected = 1.0 - frame as f32 / 64.0;
            assert!(
                close(pair[0], expected) && close(pair[1], expected),
                "frame {frame}"
            );
        }
        assert!(all[128..].iter().all(|s| *s == 0.0));
    }

    #[test]
    fn zero_duration_crossfade_swaps_within_declick() {
        let mut rig = Rig::new(1);
        let _a = rig.play(1, 1000, 1.0);
        rig.render(20);
        let (incoming, mut b) = rig.track(2);
        b.push(1000, 0.25, 1);
        rig.send(MixerCommand::Crossfade {
            track: incoming,
            fade_frames: 0,
        });
        let out = rig.render(10);
        // 5-frame de-click swap, then the incoming deck alone.
        assert!(close(out[0], 1.0));
        assert!(out[5..].iter().all(|s| close(*s, 0.25)));
        assert_eq!(
            rig.non_progress_events(),
            vec![MixerEvent::CrossfadeComplete {
                outgoing: 1,
                incoming: 2
            }]
        );
    }

    #[test]
    fn active_end_during_crossfade_auto_promotes() {
        let mut rig = Rig::new(1);
        let a = rig.play(1, 130, 1.0);
        a.finish();
        rig.render(100);
        let (incoming, mut b) = rig.track(2);
        b.push(1000, 0.5, 1);
        rig.send(MixerCommand::Crossfade {
            track: incoming,
            fade_frames: 100,
        });
        // A has 30 frames left; B's ramp is at 0.3 when A runs out.
        let out = rig.render(40);
        assert_eq!(rig.non_progress_events(), vec![ended(1)]);
        // Incoming keeps ramping smoothly: frame 35 is gain 0.35 of 0.5.
        assert!(close(out[35], 0.35 * 0.5), "{}", out[35]);
        // It is now the active deck, so promoting its key is a no-op.
        rig.send(MixerCommand::Promote { key: 2 });
        let out = rig.render(10);
        assert!(close(out[0], 0.40 * 0.5), "{}", out[0]);
        rig.render(100);
        // No CrossfadeComplete after the auto-promotion.
        assert!(rig.non_progress_events().is_empty());
        assert!(rig.render(5).iter().all(|s| close(*s, 0.5)));
    }

    #[test]
    fn ended_fires_only_after_buffer_drained() {
        let mut rig = Rig::new(2);
        let feed = rig.play(7, 150, 0.5);
        feed.finish();
        rig.render(100);
        assert!(rig.non_progress_events().is_empty());
        rig.render(49);
        assert!(rig.non_progress_events().is_empty());
        rig.render(1);
        assert_eq!(rig.non_progress_events(), vec![ended(7)]);
        // Deck is idle afterwards and the track was handed back for freeing.
        assert!(rig.render(10).iter().all(|s| *s == 0.0));
        assert!(rig.events().is_empty());
        assert_eq!(rig.garbage_count(), 1);
    }

    #[test]
    fn not_ended_while_decoder_still_running() {
        let mut rig = Rig::new(1);
        let _feed = rig.play(1, 120, 1.0);
        rig.render(200); // drains everything, but decoder never said "finished"
        assert!(rig.non_progress_events().is_empty());
    }

    #[test]
    fn underrun_renders_silence_without_ending() {
        let mut rig = Rig::new(1);
        let mut feed = rig.play(1, 110, 1.0);
        let out = rig.render(120);
        assert!(out[100..110].iter().all(|s| *s == 1.0));
        assert!(out[110..].iter().all(|s| *s == 0.0));
        assert!(rig.non_progress_events().is_empty());
        feed.push(10, 1.0, 1);
        let out = rig.render(10);
        assert!(out.iter().all(|s| *s == 1.0));
        // Position counts only frames actually played.
        let active = rig.mixer.active;
        let played = rig.mixer.decks[active]
            .track
            .as_ref()
            .map(|t| t.frames_played);
        assert_eq!(played, Some(120));
    }

    #[test]
    fn progress_reports_position_and_duration_every_100ms() {
        let mut rig = Rig::new(2);
        let feed = rig.play(3, 900, 0.1);
        feed.shared.set_duration(Some(0.9));
        let mut progress = Vec::new();
        for _ in 0..25 {
            rig.render(10);
            for event in rig.events() {
                if let MixerEvent::Progress {
                    key,
                    position_secs,
                    duration_secs,
                } = event
                {
                    assert_eq!(key, 3);
                    assert_eq!(duration_secs, Some(0.9));
                    progress.push(position_secs);
                }
            }
        }
        assert_eq!(progress.len(), 3, "{progress:?}");
        assert!(close(progress[0] as f32, 0.01));
        assert!(close(progress[1] as f32, 0.11));
        assert!(close(progress[2] as f32, 0.21));
    }

    #[test]
    fn promote_with_stale_key_is_noop() {
        let mut rig = Rig::new(1);
        let _a = rig.play(1, 1000, 1.0);
        rig.render(20);
        let (incoming, mut b) = rig.track(2);
        b.push(1000, 0.5, 1);
        rig.send(MixerCommand::Crossfade {
            track: incoming,
            fade_frames: 100,
        });
        rig.render(10);
        rig.send(MixerCommand::Promote { key: 1 }); // active, not incoming
        rig.send(MixerCommand::Promote { key: 99 }); // unknown
        let out = rig.render(10);
        // Still crossfading: frame 10 of the fade.
        assert!(close(out[0], 0.9 + 0.1 * 0.5));
        assert!(rig.non_progress_events().is_empty());
    }

    #[test]
    fn promote_cuts_outgoing_with_declick() {
        let mut rig = Rig::new(1);
        let _a = rig.play(1, 1000, 1.0);
        rig.render(20);
        let (incoming, mut b) = rig.track(2);
        b.push(1000, 0.0, 1);
        rig.send(MixerCommand::Crossfade {
            track: incoming,
            fade_frames: 100,
        });
        rig.render(50);
        rig.events();
        rig.send(MixerCommand::Promote { key: 2 });
        let out = rig.render(10);
        // Outgoing tail fades from 0.5 to 0 over 5 frames.
        for (sample, expected) in out.iter().zip([0.5, 0.4, 0.3, 0.2, 0.1, 0.0]) {
            assert!(close(*sample, expected), "{out:?}");
        }
        assert!(out[5..].iter().all(|s| *s == 0.0));
        assert_eq!(
            rig.non_progress_events(),
            vec![MixerEvent::Unloaded { key: 1 }]
        );
        assert_eq!(rig.garbage_count(), 1);
    }

    #[test]
    fn stop_suppresses_later_events() {
        let mut rig = Rig::new(1);
        let a = rig.play(1, 150, 1.0);
        a.finish();
        rig.render(20);
        rig.events();
        rig.send(MixerCommand::Stop);
        let out = rig.render(200);
        // De-click fade-out from full gain, then silence.
        assert_eq!(out[0], 1.0);
        assert!(out[5..].iter().all(|s| *s == 0.0));
        assert_eq!(rig.events(), vec![MixerEvent::Unloaded { key: 1 }]);
        rig.render(200);
        assert!(rig.events().is_empty());
        assert_eq!(rig.garbage_count(), 1);
    }

    #[test]
    fn play_over_playing_deck_replaces_it() {
        let mut rig = Rig::new(1);
        let _a = rig.play(1, 1000, 1.0);
        rig.render(20);
        rig.events();
        let _b = rig.play(2, 1000, -1.0);
        let out = rig.render(10);
        // Old fades 1→0 while new fades 0→-1: they cancel exactly mid-way.
        assert!(close(out[0], 1.0));
        assert!(out[5..].iter().all(|s| close(*s, -1.0)));
        let events = rig.events();
        assert_eq!(events[0], MixerEvent::Unloaded { key: 1 });
        assert!(
            events[1..]
                .iter()
                .all(|e| matches!(e, MixerEvent::Progress { key: 2, .. }))
        );
    }

    #[test]
    fn incoming_failure_restores_active_gain() {
        let mut rig = Rig::new(1);
        let _a = rig.play(1, 1000, 1.0);
        rig.render(20);
        let (incoming, mut b) = rig.track(2);
        b.push(1000, 0.0, 1);
        rig.send(MixerCommand::Crossfade {
            track: incoming,
            fade_frames: 400,
        });
        rig.render(200); // active at 0.5
        rig.events();
        rig.send(MixerCommand::Fail { key: 2 });
        let out = rig.render(600);
        // Back to full gain over RESTORE_SECS (500 frames).
        assert!(close(out[0], 0.5));
        assert!(close(out[250], 0.75));
        assert!(out[500..].iter().all(|s| close(*s, 1.0)));
        assert_eq!(
            rig.non_progress_events(),
            vec![MixerEvent::Failed { key: 2 }]
        );
    }

    #[test]
    fn active_failure_during_crossfade_promotes_incoming() {
        let mut rig = Rig::new(1);
        let _a = rig.play(1, 1000, 0.0);
        rig.render(20);
        let (incoming, mut b) = rig.track(2);
        b.push(1000, 1.0, 1);
        rig.send(MixerCommand::Crossfade {
            track: incoming,
            fade_frames: 100,
        });
        rig.render(50);
        rig.events();
        rig.send(MixerCommand::Fail { key: 1 });
        let out = rig.render(60);
        assert!(close(out[0], 0.5));
        assert!(close(out[59], 1.0));
        assert_eq!(
            rig.non_progress_events(),
            vec![MixerEvent::Failed { key: 1 }]
        );
        // Fail for an unknown key is ignored.
        rig.send(MixerCommand::Fail { key: 1 });
        rig.render(1);
        assert!(rig.non_progress_events().is_empty());
    }

    #[test]
    fn crossfade_with_nothing_playing_just_plays() {
        let mut rig = Rig::new(1);
        let (track, mut feed) = rig.track(5);
        feed.push(200, 1.0, 1);
        rig.send(MixerCommand::Crossfade {
            track,
            fade_frames: 1000,
        });
        let out = rig.render(10);
        assert!(out[5..].iter().all(|s| *s == 1.0));
    }

    #[test]
    fn crossfade_during_crossfade_continues_from_incoming() {
        let mut rig = Rig::new(1);
        let _a = rig.play(1, 1000, 0.0);
        rig.render(20);
        let (b_track, mut b) = rig.track(2);
        b.push(1000, 1.0, 1);
        rig.send(MixerCommand::Crossfade {
            track: b_track,
            fade_frames: 100,
        });
        rig.render(50); // B at 0.5
        rig.events();
        let (c_track, mut c) = rig.track(3);
        c.push(1000, 0.0, 1);
        rig.send(MixerCommand::Crossfade {
            track: c_track,
            fade_frames: 100,
        });
        let out = rig.render(100);
        assert!(close(out[0], 0.5));
        assert!(close(out[50], 0.25));
        let events = rig.non_progress_events();
        assert_eq!(events[0], MixerEvent::Unloaded { key: 1 });
        assert_eq!(
            events[1],
            MixerEvent::CrossfadeComplete {
                outgoing: 2,
                incoming: 3
            }
        );
    }

    #[test]
    fn short_incoming_ending_restores_outgoing() {
        let mut rig = Rig::new(1);
        let _a = rig.play(1, 1000, 1.0);
        rig.render(20);
        let (incoming, b) = rig.track(2);
        b.finish(); // empty file
        rig.send(MixerCommand::Crossfade {
            track: incoming,
            fade_frames: 100,
        });
        let out = rig.render(10);
        assert!(out.iter().all(|s| *s == 1.0));
        assert_eq!(rig.non_progress_events(), vec![ended(2)]);
    }

    #[test]
    fn output_is_clamped() {
        let mut rig = Rig::new(1);
        let _a = rig.play(1, 1000, 3.0);
        let out = rig.render(10);
        assert!(out.iter().all(|s| s.abs() <= 1.0));
    }
}
