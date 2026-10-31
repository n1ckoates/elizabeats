//! Opening the audio output device and running the [`Mixer`] in its callback.

use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::mixer::{Mixer, MixerHandles};

/// Largest block the mixer renders at once; bigger device buffers are split.
const SCRATCH_FRAMES: usize = 4096;

/// Receives fatal stream errors (called from a backend thread).
pub(crate) type ErrorSink = Box<dyn FnMut(String) + Send>;

/// Opens an output and returns it running. Swappable so the engine can be
/// tested without a sound card.
pub(crate) type OutputOpener = Box<dyn FnMut(ErrorSink) -> Result<OpenedOutput, String> + Send>;

/// A running output stream.
pub(crate) struct OpenedOutput {
    pub(crate) sample_rate: u32,
    pub(crate) channels: usize,
    pub(crate) handles: MixerHandles,
    /// Incremented by every audio callback; used to detect a stalled device.
    pub(crate) heartbeat: Arc<AtomicU64>,
    /// Owns the stream. Dropping it stops the output.
    pub(crate) _keepalive: Box<dyn Any>,
}

/// Opens the system's current default output device.
pub(crate) fn open_default_output(on_error: ErrorSink) -> Result<OpenedOutput, String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "No audio output device is available.".to_string())?;
    let supported = device
        .default_output_config()
        .map_err(|err| format!("Could not query the audio output device: {err}"))?;
    let sample_format = supported.sample_format();
    let config = supported.config();
    let sample_rate = config.sample_rate;
    let channels = usize::from(config.channels);
    if sample_rate == 0 || channels == 0 {
        return Err("The audio output device reported an invalid format.".into());
    }

    let (mixer, handles) = Mixer::new(sample_rate, channels);
    let heartbeat = Arc::new(AtomicU64::new(0));
    let beat = Arc::clone(&heartbeat);

    let stream = match sample_format {
        SampleFormat::F32 => build::<f32>(&device, config, mixer, beat, on_error),
        SampleFormat::F64 => build::<f64>(&device, config, mixer, beat, on_error),
        SampleFormat::I16 => build::<i16>(&device, config, mixer, beat, on_error),
        SampleFormat::I32 => build::<i32>(&device, config, mixer, beat, on_error),
        SampleFormat::U16 => build::<u16>(&device, config, mixer, beat, on_error),
        SampleFormat::I8 => build::<i8>(&device, config, mixer, beat, on_error),
        SampleFormat::U8 => build::<u8>(&device, config, mixer, beat, on_error),
        other => {
            return Err(format!(
                "The audio output device uses an unsupported sample format ({other})."
            ));
        }
    }
    .map_err(|err| format!("Could not open the audio output: {err}"))?;
    stream
        .play()
        .map_err(|err| format!("Could not start the audio output: {err}"))?;

    Ok(OpenedOutput {
        sample_rate,
        channels,
        handles,
        heartbeat,
        _keepalive: Box::new(stream),
    })
}

fn build<T>(
    device: &cpal::Device,
    config: StreamConfig,
    mut mixer: Mixer,
    heartbeat: Arc<AtomicU64>,
    mut on_error: ErrorSink,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels).max(1);
    // Allocated once here; the callback itself never allocates.
    let mut scratch = vec![0.0f32; SCRATCH_FRAMES * channels];
    device.build_output_stream::<T, _, _>(
        config,
        move |data: &mut [T], _info| {
            for chunk in data.chunks_mut(scratch.len()) {
                let block = &mut scratch[..chunk.len()];
                mixer.render(block);
                for (out, sample) in chunk.iter_mut().zip(block.iter()) {
                    *out = T::from_sample(*sample);
                }
            }
            heartbeat.fetch_add(1, Ordering::Relaxed);
        },
        move |err: cpal::Error| {
            if is_fatal(err.kind()) {
                on_error(err.to_string());
            }
        },
        None,
    )
}

/// Errors after which the stream will not recover by itself.
fn is_fatal(kind: ErrorKind) -> bool {
    !matches!(
        kind,
        ErrorKind::Xrun | ErrorKind::DeviceChanged | ErrorKind::RealtimeDenied
    )
}

#[cfg(test)]
pub(crate) mod fake {
    //! A device-free output that runs the mixer on a thread, faster than
    //! real time, for engine tests.

    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    /// Lets a test break the fake device.
    #[derive(Clone, Default)]
    pub(crate) struct FakeControls {
        pub(crate) error_sink: Arc<Mutex<Option<ErrorSink>>>,
        pub(crate) stall: Arc<AtomicBool>,
        pub(crate) opened: Arc<AtomicU64>,
    }

    impl FakeControls {
        pub(crate) fn fail_device(&self, message: &str) {
            if let Some(sink) = self.error_sink.lock().unwrap().as_mut() {
                sink(message.to_string());
            }
        }
    }

    struct StopOnDrop(Arc<AtomicBool>, Option<thread::JoinHandle<()>>);

    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
            if let Some(handle) = self.1.take() {
                let _ = handle.join();
            }
        }
    }

    pub(crate) fn opener(
        sample_rate: u32,
        channels: usize,
        controls: FakeControls,
    ) -> OutputOpener {
        Box::new(move |on_error| {
            controls.opened.fetch_add(1, Ordering::Relaxed);
            *controls.error_sink.lock().unwrap() = Some(on_error);
            let (mut mixer, handles) = Mixer::new(sample_rate, channels);
            let heartbeat = Arc::new(AtomicU64::new(0));
            let beat = Arc::clone(&heartbeat);
            let stop = Arc::new(AtomicBool::new(false));
            let stop_flag = Arc::clone(&stop);
            let stall = Arc::clone(&controls.stall);
            let (ready_tx, ready_rx) = mpsc::channel();
            let handle = thread::spawn(move || {
                let _ = ready_tx.send(());
                let mut buffer = vec![0.0f32; 256 * channels];
                while !stop_flag.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(2));
                    if stall.load(Ordering::Relaxed) {
                        continue;
                    }
                    mixer.render(&mut buffer);
                    beat.fetch_add(1, Ordering::Relaxed);
                }
            });
            let _ = ready_rx.recv();
            Ok(OpenedOutput {
                sample_rate,
                channels,
                handles,
                heartbeat,
                _keepalive: Box::new(StopOnDrop(stop, Some(handle))),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_errors_are_not_fatal() {
        assert!(!is_fatal(ErrorKind::Xrun));
        assert!(!is_fatal(ErrorKind::DeviceChanged));
        assert!(is_fatal(ErrorKind::DeviceNotAvailable));
        assert!(is_fatal(ErrorKind::StreamInvalidated));
    }
}
