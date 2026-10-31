//! The control thread: turns API calls into mixer commands, owns the output
//! stream and the decoder threads, and turns mixer events into
//! [`AudioEvent`]s for the caller.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::decoder::spawn_decoder;
use crate::mixer::{MixerCommand, MixerEvent, Track, TrackKey};
use crate::output::{OpenedOutput, OutputOpener};
use crate::{AudioEvent, LoadId};

/// How often mixer events are collected while an output is open.
const POLL_INTERVAL: Duration = Duration::from_millis(10);
/// The output is considered dead if its callback stops running this long
/// while something is loaded.
const STALL_TIMEOUT: Duration = Duration::from_secs(3);
/// How long to wait for space in the mixer's command queue.
const COMMAND_TIMEOUT: Duration = Duration::from_millis(250);

pub(crate) enum ControlMsg {
    Play {
        id: LoadId,
        path: PathBuf,
    },
    Crossfade {
        id: LoadId,
        path: PathBuf,
        duration: Duration,
    },
    Promote {
        id: LoadId,
    },
    Stop,
    DecoderFailed {
        key: TrackKey,
        message: String,
    },
    OutputFailed {
        generation: u64,
        message: String,
    },
    Shutdown,
}

/// A track the caller still cares about.
struct LiveTrack {
    key: TrackKey,
    id: LoadId,
    cancel: Arc<AtomicBool>,
    decoder: Option<JoinHandle<()>>,
    failure: Option<String>,
}

struct Output {
    opened: OpenedOutput,
    generation: u64,
    last_heartbeat: u64,
    last_heartbeat_change: Instant,
}

pub(crate) struct Controller {
    on_event: Box<dyn Fn(AudioEvent) + Send + Sync>,
    sender: Sender<ControlMsg>,
    open_output: OutputOpener,
    output: Option<Output>,
    live: Vec<LiveTrack>,
    /// Decoder threads that were told to stop and are not yet joined.
    retiring: Vec<JoinHandle<()>>,
    next_key: TrackKey,
    next_generation: u64,
}

impl Controller {
    pub(crate) fn new(
        on_event: Box<dyn Fn(AudioEvent) + Send + Sync>,
        sender: Sender<ControlMsg>,
        open_output: OutputOpener,
    ) -> Self {
        Self {
            on_event,
            sender,
            open_output,
            output: None,
            live: Vec::new(),
            retiring: Vec::new(),
            next_key: 1,
            next_generation: 1,
        }
    }

    /// Runs until `Shutdown` arrives (or every sender is gone).
    pub(crate) fn run(mut self, receiver: Receiver<ControlMsg>) {
        loop {
            let message = if self.output.is_some() {
                match receiver.recv_timeout(POLL_INTERVAL) {
                    Ok(message) => Some(message),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            } else {
                // Nothing can produce mixer events without an output.
                match receiver.recv() {
                    Ok(message) => Some(message),
                    Err(_) => break,
                }
            };
            match message {
                Some(ControlMsg::Shutdown) => break,
                Some(message) => self.handle(message),
                None => {}
            }
            self.pump_mixer();
            self.check_heartbeat();
            self.reap_decoders();
        }
        self.shutdown();
    }

    fn handle(&mut self, message: ControlMsg) {
        match message {
            ControlMsg::Play { id, path } => self.play(id, path),
            ControlMsg::Crossfade { id, path, duration } => self.crossfade(id, path, duration),
            ControlMsg::Promote { id } => self.promote(id),
            ControlMsg::Stop => self.stop(),
            ControlMsg::DecoderFailed { key, message } => self.decoder_failed(key, message),
            ControlMsg::OutputFailed {
                generation,
                message,
            } => {
                if self.output.as_ref().map(|o| o.generation) == Some(generation) {
                    self.output_failed(&format!("The audio output device failed: {message}"));
                }
            }
            ControlMsg::Shutdown => {}
        }
    }

    fn play(&mut self, id: LoadId, path: PathBuf) {
        self.forget_all();
        if let Some(track) = self.load(id, path) {
            self.send_command(MixerCommand::Play(track));
        }
    }

    fn crossfade(&mut self, id: LoadId, path: PathBuf, duration: Duration) {
        let Some(track) = self.load(id, path) else {
            return;
        };
        let rate = self.output.as_ref().map_or(0, |o| o.opened.sample_rate);
        let frames = (duration.as_secs_f64() * f64::from(rate)).round();
        let fade_frames = if frames.is_finite() && frames > 0.0 {
            frames as u64
        } else {
            0
        };
        self.send_command(MixerCommand::Crossfade { track, fade_frames });
    }

    fn promote(&mut self, id: LoadId) {
        // The newest load with this id is the one that can be incoming; the
        // mixer ignores the command if it is not.
        if let Some(key) = self.live.iter().rev().find(|t| t.id == id).map(|t| t.key) {
            self.send_command(MixerCommand::Promote { key });
        }
    }

    fn stop(&mut self) {
        self.forget_all();
        if self.output.is_some() {
            self.send_command(MixerCommand::Stop);
        }
    }

    fn decoder_failed(&mut self, key: TrackKey, message: String) {
        if let Some(track) = self.live.iter_mut().find(|t| t.key == key) {
            track.failure = Some(message);
            self.send_command(MixerCommand::Fail { key });
        }
    }

    /// Opens the output if needed, then creates the track's ring buffer and
    /// starts its decoder. On failure reports `Failed` and returns `None`.
    fn load(&mut self, id: LoadId, path: PathBuf) -> Option<Track> {
        if let Err(message) = self.ensure_output() {
            self.emit(AudioEvent::Failed { id, message });
            return None;
        }
        let (rate, channels) = self
            .output
            .as_ref()
            .map(|o| (o.opened.sample_rate, o.opened.channels))?;
        let key = self.next_key;
        self.next_key += 1;
        let (track, producer, shared) = Track::new(key, rate, channels);
        let cancel = Arc::new(AtomicBool::new(false));
        let sender = self.sender.clone();
        let spawned = spawn_decoder(
            path,
            rate,
            channels,
            producer,
            shared,
            Arc::clone(&cancel),
            move |message| {
                let _ = sender.send(ControlMsg::DecoderFailed { key, message });
            },
        );
        match spawned {
            Ok(handle) => {
                self.live.push(LiveTrack {
                    key,
                    id,
                    cancel,
                    decoder: Some(handle),
                    failure: None,
                });
                Some(track)
            }
            Err(err) => {
                self.emit(AudioEvent::Failed {
                    id,
                    message: format!("Could not start the audio decoder: {err}"),
                });
                None
            }
        }
    }

    fn ensure_output(&mut self) -> Result<(), String> {
        if self.output.is_some() {
            return Ok(());
        }
        let generation = self.next_generation;
        self.next_generation += 1;
        let sender = self.sender.clone();
        let on_error = Box::new(move |message: String| {
            let _ = sender.send(ControlMsg::OutputFailed {
                generation,
                message,
            });
        });
        let opened = (self.open_output)(on_error)?;
        self.output = Some(Output {
            opened,
            generation,
            last_heartbeat: 0,
            last_heartbeat_change: Instant::now(),
        });
        Ok(())
    }

    /// Queues a command for the audio callback, waiting briefly if the
    /// queue is full. If the callback is not draining it, the output is
    /// treated as failed.
    fn send_command(&mut self, mut command: MixerCommand) {
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        loop {
            let Some(output) = self.output.as_mut() else {
                return;
            };
            match output.opened.handles.commands.push(command) {
                Ok(()) => return,
                Err(rtrb::PushError::Full(returned)) => {
                    if Instant::now() >= deadline {
                        self.output_failed("The audio output stopped responding.");
                        return;
                    }
                    command = returned;
                    thread::sleep(Duration::from_millis(1));
                }
            }
        }
    }

    /// Forwards mixer events for live tracks and frees finished tracks.
    fn pump_mixer(&mut self) {
        while let Some(event) = self
            .output
            .as_mut()
            .and_then(|o| o.opened.handles.events.pop().ok())
        {
            self.forward(event);
        }
        if let Some(output) = self.output.as_mut() {
            // Dropping here (not on the audio thread) frees the buffers.
            while output.opened.handles.garbage.pop().is_ok() {}
        }
    }

    fn forward(&mut self, event: MixerEvent) {
        match event {
            MixerEvent::Progress {
                key,
                position_secs,
                duration_secs,
            } => {
                if let Some(id) = self.id_of(key) {
                    self.emit(AudioEvent::Progress {
                        id,
                        position_secs,
                        duration_secs,
                    });
                }
            }
            MixerEvent::Ended { key, .. } => {
                if let Some(track) = self.forget(key) {
                    self.emit(AudioEvent::Ended { id: track.id });
                }
            }
            MixerEvent::Failed { key } => {
                if let Some(track) = self.forget(key) {
                    let message = track
                        .failure
                        .clone()
                        .unwrap_or_else(|| "This track could not be played.".into());
                    self.emit(AudioEvent::Failed {
                        id: track.id,
                        message,
                    });
                }
            }
            MixerEvent::CrossfadeComplete { outgoing, incoming } => {
                if let (Some(outgoing), Some(incoming)) =
                    (self.id_of(outgoing), self.id_of(incoming))
                {
                    self.emit(AudioEvent::CrossfadeComplete { outgoing, incoming });
                }
            }
            MixerEvent::Unloaded { key } => {
                self.forget(key);
            }
        }
    }

    fn id_of(&self, key: TrackKey) -> Option<LoadId> {
        self.live.iter().find(|t| t.key == key).map(|t| t.id)
    }

    /// Stops caring about a track: no more events for it, and its decoder
    /// is told to stop. Returns its record if it was live.
    fn forget(&mut self, key: TrackKey) -> Option<LiveTrack> {
        let index = self.live.iter().position(|t| t.key == key)?;
        let mut track = self.live.remove(index);
        self.retire_decoder(&mut track);
        Some(track)
    }

    fn forget_all(&mut self) {
        for mut track in std::mem::take(&mut self.live) {
            self.retire_decoder(&mut track);
        }
    }

    fn retire_decoder(&mut self, track: &mut LiveTrack) {
        track.cancel.store(true, Ordering::Relaxed);
        if let Some(handle) = track.decoder.take() {
            self.retiring.push(handle);
        }
    }

    fn reap_decoders(&mut self) {
        let (done, pending): (Vec<_>, Vec<_>) = std::mem::take(&mut self.retiring)
            .into_iter()
            .partition(|handle| handle.is_finished());
        self.retiring = pending;
        for handle in done {
            let _ = handle.join();
        }
    }

    /// Detects an output whose callback has silently stopped running.
    fn check_heartbeat(&mut self) {
        let has_live = !self.live.is_empty();
        let Some(output) = self.output.as_mut() else {
            return;
        };
        let beat = output.opened.heartbeat.load(Ordering::Relaxed);
        let now = Instant::now();
        if beat != output.last_heartbeat || !has_live {
            output.last_heartbeat = beat;
            output.last_heartbeat_change = now;
            return;
        }
        if now.duration_since(output.last_heartbeat_change) > STALL_TIMEOUT {
            self.output_failed("The audio output stopped responding.");
        }
    }

    /// Drops the output and reports every live track as failed. The next
    /// `play`/`crossfade` reopens the then-current default device.
    fn output_failed(&mut self, message: &str) {
        self.output = None;
        for mut track in std::mem::take(&mut self.live) {
            self.retire_decoder(&mut track);
            self.emit(AudioEvent::Failed {
                id: track.id,
                message: message.to_string(),
            });
        }
    }

    fn emit(&self, event: AudioEvent) {
        (self.on_event)(event);
    }

    fn shutdown(&mut self) {
        // Stop the device first so nothing is reading the buffers.
        self.output = None;
        self.forget_all();
        for handle in self.retiring.drain(..) {
            let _ = handle.join();
        }
    }
}
