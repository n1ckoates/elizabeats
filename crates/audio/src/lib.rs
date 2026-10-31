//! Two-deck audio playback engine for Elizabeats.
//!
//! [`AudioEngine`] plays files on the default output device with two
//! "decks" so one Track can crossfade into the next, mirroring the original
//! WebAudio implementation.
//!
//! # Threads
//! - **Control thread** (one per engine): receives API calls, opens the
//!   output device lazily, starts decoders and delivers [`AudioEvent`]s
//!   by calling the `on_event` callback.
//! - **Decoder threads** (one per loaded file): decode, map channels and
//!   resample into a lock-free ring buffer holding about one second of audio.
//! - **Audio callback** (owned by the OS audio system): runs the mixer,
//!   which never allocates, locks or blocks.
//!
//! All public methods only queue a message and return immediately.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod channels;
mod decoder;
mod engine;
mod mixer;
mod output;

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use engine::{ControlMsg, Controller};
use output::OutputOpener;

/// The engine's notifications (delivered on its control thread) and the
/// caller-chosen id of one load of a file, shared with the Session.
pub use elizabeats_core::session::{AudioEvent, LoadId};

/// Handle to the playback engine. Dropping it stops playback and joins all
/// of the engine's threads.
pub struct AudioEngine {
    sender: Sender<ControlMsg>,
    control: Option<JoinHandle<()>>,
}

impl AudioEngine {
    /// Spawns the control thread. The output stream is opened lazily on first play.
    /// `on_event` is called from a background thread; it must be cheap and non-blocking.
    pub fn new(on_event: impl Fn(AudioEvent) + Send + Sync + 'static) -> Self {
        Self::with_output(Box::new(on_event), Box::new(output::open_default_output))
    }

    fn with_output(
        on_event: Box<dyn Fn(AudioEvent) + Send + Sync>,
        open_output: OutputOpener,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let control_sender = sender.clone();
        let control = std::thread::Builder::new()
            .name("elizabeats-audio".into())
            .spawn(move || Controller::new(on_event, control_sender, open_output).run(receiver))
            .ok();
        // If the thread could not be spawned every call is a silent no-op;
        // there is no thread to report through.
        Self { sender, control }
    }

    /// Stops everything and starts `path` on the active deck at full gain.
    pub fn play(&self, id: LoadId, path: PathBuf) {
        self.send(ControlMsg::Play { id, path });
    }

    /// Starts `path` from 0 on the idle deck at gain 0 and linearly ramps
    /// active 1→0 / incoming 0→1 over `duration` (zero duration = instant swap of gains).
    /// The outgoing deck keeps playing (silently once faded) until it ends naturally
    /// or promote_incoming is called.
    pub fn crossfade(&self, id: LoadId, path: PathBuf, duration: Duration) {
        self.send(ControlMsg::Crossfade { id, path, duration });
    }

    /// If the incoming deck currently holds `incoming`, stop the outgoing deck
    /// immediately and make the incoming deck the active one (gain 1). Otherwise no-op.
    pub fn promote_incoming(&self, incoming: LoadId) {
        self.send(ControlMsg::Promote { id: incoming });
    }

    /// Stops and unloads both decks.
    pub fn stop(&self) {
        self.send(ControlMsg::Stop);
    }

    fn send(&self, message: ControlMsg) {
        // Fails only if the control thread is gone, in which case there is
        // nothing left to do.
        let _ = self.sender.send(message);
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        self.send(ControlMsg::Shutdown);
        if let Some(control) = self.control.take() {
            let _ = control.join();
        }
    }
}

#[cfg(test)]
mod tests;
