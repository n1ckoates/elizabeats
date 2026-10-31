//! The control thread: turns API calls into mixer commands, owns the output
//! stream and the decoder threads, and turns mixer events into
//! [`AudioEvent`]s for the caller.

use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::decoder::spawn_decoder;
use crate::mixer::{MixerCommand, MixerEvent, Track, TrackKey, secs_to_frames};
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

/// A track the caller still cares about: events for it are forwarded.
struct Live {
    key: TrackKey,
    id: LoadId,
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
    live: Vec<Live>,
    /// Decoder threads not yet joined. Each exits once its track is decoded
    /// or dropped by the mixer.
    decoders: Vec<JoinHandle<()>>,
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
            decoders: Vec::new(),
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
            self.decoders.retain(|handle| !handle.is_finished());
        }
        // Stop the device first: dropping the mixer drops every track, which
        // stops their decoders.
        self.output = None;
        for handle in self.decoders.drain(..) {
            let _ = handle.join();
        }
    }

    fn handle(&mut self, message: ControlMsg) {
        match message {
            ControlMsg::Play { id, path } => {
                self.live.clear();
                if let Some(track) = self.load(id, path) {
                    self.send_command(MixerCommand::Play(track));
                }
            }
            ControlMsg::Crossfade { id, path, duration } => {
                if let Some(track) = self.load(id, path) {
                    let rate = self.output.as_ref().map_or(0, |o| o.opened.sample_rate);
                    let fade_frames = secs_to_frames(duration.as_secs_f64(), rate);
                    self.send_command(MixerCommand::Crossfade { track, fade_frames });
                }
            }
            ControlMsg::Promote { id } => {
                // The newest load with this id is the one that can be
                // incoming; the mixer ignores the command if it is not.
                if let Some(live) = self.live.iter().rev().find(|live| live.id == id) {
                    let key = live.key;
                    self.send_command(MixerCommand::Promote { key });
                }
            }
            ControlMsg::Stop => {
                self.live.clear();
                self.send_command(MixerCommand::Stop);
            }
            ControlMsg::DecoderFailed { key, message } => {
                if let Some(id) = self.forget(key) {
                    self.emit(AudioEvent::Failed { id, message });
                    self.send_command(MixerCommand::Fail { key });
                }
            }
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

    /// Opens the output if needed, then creates the track and starts its
    /// decoder. On failure reports `Failed` and returns `None`.
    fn load(&mut self, id: LoadId, path: PathBuf) -> Option<Track> {
        let (rate, channels) = match self.ensure_output() {
            Ok(format) => format,
            Err(message) => {
                self.emit(AudioEvent::Failed { id, message });
                return None;
            }
        };
        let key = self.next_key;
        self.next_key += 1;
        let sender = self.sender.clone();
        let spawned = spawn_decoder(key, path, rate, channels, move |message| {
            let _ = sender.send(ControlMsg::DecoderFailed { key, message });
        });
        match spawned {
            Ok((track, handle)) => {
                self.decoders.push(handle);
                self.live.push(Live { key, id });
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

    /// Opens the output if needed; returns its sample rate and channel count.
    fn ensure_output(&mut self) -> Result<(u32, usize), String> {
        let output = match self.output.take() {
            Some(output) => output,
            None => {
                let generation = self.next_generation;
                self.next_generation += 1;
                let sender = self.sender.clone();
                let on_error = Box::new(move |message: String| {
                    let _ = sender.send(ControlMsg::OutputFailed {
                        generation,
                        message,
                    });
                });
                Output {
                    opened: (self.open_output)(on_error)?,
                    generation,
                    last_heartbeat: 0,
                    last_heartbeat_change: Instant::now(),
                }
            }
        };
        let format = (output.opened.sample_rate, output.opened.channels);
        self.output = Some(output);
        Ok(format)
    }

    /// Queues a command for the audio callback (if there is one), waiting
    /// briefly if the queue is full. If the callback is not draining it, the
    /// output is treated as failed.
    fn send_command(&mut self, mut command: MixerCommand) {
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        while let Some(output) = self.output.as_mut() {
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
        let event = match event {
            MixerEvent::Progress {
                key,
                position_secs,
                duration_secs,
            } => self.id_of(key).map(|id| AudioEvent::Progress {
                id,
                position_secs,
                duration_secs,
            }),
            MixerEvent::Ended { key, .. } => self.forget(key).map(|id| AudioEvent::Ended { id }),
            MixerEvent::CrossfadeComplete { outgoing, incoming } => self
                .id_of(outgoing)
                .zip(self.id_of(incoming))
                .map(|(outgoing, incoming)| AudioEvent::CrossfadeComplete { outgoing, incoming }),
            MixerEvent::Unloaded { key } => {
                self.forget(key);
                None
            }
        };
        if let Some(event) = event {
            self.emit(event);
        }
    }

    fn id_of(&self, key: TrackKey) -> Option<LoadId> {
        self.live
            .iter()
            .find(|live| live.key == key)
            .map(|live| live.id)
    }

    /// Stops forwarding events for a track. Returns its id if it was live.
    fn forget(&mut self, key: TrackKey) -> Option<LoadId> {
        let index = self.live.iter().position(|live| live.key == key)?;
        Some(self.live.remove(index).id)
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
        for live in std::mem::take(&mut self.live) {
            self.emit(AudioEvent::Failed {
                id: live.id,
                message: message.to_string(),
            });
        }
    }

    fn emit(&self, event: AudioEvent) {
        (self.on_event)(event);
    }
}
