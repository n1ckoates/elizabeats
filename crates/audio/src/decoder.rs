//! File decoding on a background thread.
//!
//! Each loaded file gets its own decoder thread. It decodes with Symphonia,
//! maps channels to the device layout, resamples to the device rate and
//! keeps the track's ring buffer (about one second) topped up.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rtrb::Producer;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::channels::map_channels;
use crate::mixer::{Track, TrackKey, TrackShared};

/// How long the decoder sleeps when the ring buffer is full.
const FULL_BUFFER_WAIT: Duration = Duration::from_millis(5);
/// Give up after this many consecutive undecodable packets.
const MAX_CONSECUTIVE_DECODE_ERRORS: u32 = 100;
/// Resampler input chunk size in frames.
const RESAMPLER_CHUNK: usize = 1024;

/// Decodes a file into interleaved `f32` at a fixed output rate and
/// channel count.
pub(crate) struct SourceDecoder {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    out_rate: u32,
    out_channels: usize,
    duration_secs: Option<f64>,
    decoded: Vec<f32>,
    mapped: Vec<f32>,
    resampler: Option<StreamResampler>,
    source_rate: Option<u32>,
    consecutive_errors: u32,
    finished: bool,
}

impl SourceDecoder {
    pub(crate) fn open(path: &Path, out_rate: u32, out_channels: usize) -> Result<Self, String> {
        let file =
            File::open(path).map_err(|err| format!("Could not open {}: {err}", path.display()))?;
        let stream = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
            hint.with_extension(extension);
        }
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                stream,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|err| format!("Unsupported or unreadable audio file: {err}"))?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| "The file has no audio track.".to_string())?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or_else(|| "The audio track has no codec parameters.".to_string())?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(|err| format!("Unsupported audio codec: {err}"))?;

        let duration_secs = match (track.num_frames, params.sample_rate) {
            (Some(frames), Some(rate)) if rate > 0 => Some(frames as f64 / f64::from(rate)),
            _ => track
                .time_base
                .zip(track.duration)
                .and_then(|(base, duration)| base.calc_duration(duration))
                .map(|time| time.as_secs_f64()),
        };
        let track_id = track.id;

        Ok(Self {
            format,
            decoder,
            track_id,
            out_rate: out_rate.max(1),
            out_channels: out_channels.max(1),
            duration_secs,
            decoded: Vec::new(),
            mapped: Vec::new(),
            resampler: None,
            source_rate: None,
            consecutive_errors: 0,
            finished: false,
        })
    }

    /// Duration from the container, if it declares one.
    pub(crate) fn duration_secs(&self) -> Option<f64> {
        self.duration_secs
    }

    /// Replaces `out` with the next batch of output samples (interleaved,
    /// device rate and channels). Returns `Ok(false)` once the stream is
    /// complete; `out` may still hold final samples in that case.
    pub(crate) fn read(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        out.clear();
        while out.is_empty() && !self.finished {
            match self.format.next_packet() {
                Ok(Some(packet)) => {
                    if packet.track_id != self.track_id {
                        continue;
                    }
                    self.decode_packet(&packet, out)?;
                }
                Ok(None) => self.finish(out)?,
                // Chained streams need a decoder reset; treat as the end.
                Err(SymphoniaError::ResetRequired) => self.finish(out)?,
                // A truncated file: play what we have.
                Err(SymphoniaError::IoError(err))
                    if err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    self.finish(out)?
                }
                Err(err) => return Err(format!("Error reading audio file: {err}")),
            }
        }
        Ok(!self.finished)
    }

    fn decode_packet(
        &mut self,
        packet: &symphonia::core::packet::Packet,
        out: &mut Vec<f32>,
    ) -> Result<(), String> {
        let buffer = match self.decoder.decode(packet) {
            Ok(buffer) => {
                self.consecutive_errors = 0;
                buffer
            }
            // Corrupt packet: skip it, like every player does.
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => {
                self.consecutive_errors += 1;
                if self.consecutive_errors > MAX_CONSECUTIVE_DECODE_ERRORS {
                    return Err("The audio file is corrupt (too many decode errors).".into());
                }
                return Ok(());
            }
            Err(err) => return Err(format!("Error decoding audio: {err}")),
        };
        if buffer.frames() == 0 {
            return Ok(());
        }
        let rate = buffer.spec().rate();
        let channels = buffer.num_planes();
        buffer.copy_to_vec_interleaved(&mut self.decoded);
        map_channels(&self.decoded, channels, &mut self.mapped, self.out_channels);

        match self.source_rate {
            None => {
                self.source_rate = Some(rate);
                if rate != self.out_rate {
                    self.resampler = Some(StreamResampler::new(
                        rate,
                        self.out_rate,
                        self.out_channels,
                    )?);
                }
            }
            Some(previous) if previous != rate => {
                return Err("The audio file changes sample rate mid-stream.".into());
            }
            Some(_) => {}
        }

        match self.resampler.as_mut() {
            Some(resampler) => resampler.process(&self.mapped, out),
            None => {
                out.extend_from_slice(&self.mapped);
                Ok(())
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<f32>) -> Result<(), String> {
        self.finished = true;
        match self.resampler.as_mut() {
            Some(resampler) => resampler.flush(out),
            None => Ok(()),
        }
    }
}

/// Streaming wrapper around a rubato sinc resampler that hides its chunking
/// and start-up delay: output is time-aligned with the input and has exactly
/// `input_frames * out_rate / in_rate` frames (rounded) once flushed.
pub(crate) struct StreamResampler {
    inner: Async<f32>,
    channels: usize,
    ratio: f64,
    /// Interleaved input waiting for a full chunk.
    staged: Vec<f32>,
    /// Scratch output of one `process_into_buffer` call.
    chunk_out: Vec<f32>,
    /// Leading output frames still to drop (the filter delay).
    delay_left: usize,
    input_frames: u64,
    output_frames: u64,
}

impl StreamResampler {
    pub(crate) fn new(in_rate: u32, out_rate: u32, channels: usize) -> Result<Self, String> {
        let ratio = f64::from(out_rate) / f64::from(in_rate.max(1));
        let inner = Async::<f32>::new_sinc(
            ratio,
            1.1,
            &SincInterpolationParameters::default(),
            RESAMPLER_CHUNK,
            channels,
            FixedAsync::Input,
        )
        .map_err(|err| format!("Could not create resampler: {err}"))?;
        let chunk_out = vec![0.0; inner.output_frames_max() * channels];
        let delay_left = inner.output_delay();
        Ok(Self {
            inner,
            channels,
            ratio,
            staged: Vec::new(),
            chunk_out,
            delay_left,
            input_frames: 0,
            output_frames: 0,
        })
    }

    /// Feeds interleaved input and appends whatever output is ready.
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<(), String> {
        self.staged.extend_from_slice(input);
        self.input_frames += (input.len() / self.channels) as u64;
        let mut consumed = 0;
        loop {
            let staged_frames = self.staged.len() / self.channels - consumed;
            if staged_frames < self.inner.input_frames_next() {
                break;
            }
            consumed += self.run_chunk(consumed, None, out, u64::MAX)?;
        }
        self.staged.drain(..consumed * self.channels);
        Ok(())
    }

    /// Pushes the remaining input (padded with silence) through the filter
    /// and appends the tail of the output.
    pub(crate) fn flush(&mut self, out: &mut Vec<f32>) -> Result<(), String> {
        let expected = (self.input_frames as f64 * self.ratio).round() as u64;
        let mut consumed = 0;
        // Bounded: each pass produces at least one chunk of output.
        let max_passes = 4 + (self.delay_left + RESAMPLER_CHUNK * 4) / RESAMPLER_CHUNK;
        for _ in 0..max_passes {
            if self.output_frames >= expected {
                break;
            }
            let remaining = self.staged.len() / self.channels - consumed;
            let partial = remaining.min(self.inner.input_frames_next());
            consumed += self.run_chunk(consumed, Some(partial), out, expected)?;
        }
        self.staged.clear();
        Ok(())
    }

    /// Runs one resampler chunk starting at frame `offset` of `staged`.
    /// Returns the number of input frames consumed.
    fn run_chunk(
        &mut self,
        offset: usize,
        partial: Option<usize>,
        out: &mut Vec<f32>,
        limit: u64,
    ) -> Result<usize, String> {
        let channels = self.channels;
        let staged_frames = self.staged.len() / channels;
        let input = InterleavedSlice::new(&self.staged[..], channels, staged_frames)
            .map_err(|err| format!("Resampler input error: {err}"))?;
        let out_capacity = self.chunk_out.len() / channels;
        let mut output = InterleavedSlice::new_mut(&mut self.chunk_out[..], channels, out_capacity)
            .map_err(|err| format!("Resampler output error: {err}"))?;
        let mut indexing = Indexing::new();
        indexing.input_offset = offset;
        indexing.partial_len = partial;
        let (frames_in, frames_out) = self
            .inner
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .map_err(|err| format!("Resampling failed: {err}"))?;

        let skip = self.delay_left.min(frames_out);
        self.delay_left -= skip;
        let available = (frames_out - skip) as u64;
        let keep = available.min(limit.saturating_sub(self.output_frames)) as usize;
        out.extend_from_slice(&self.chunk_out[skip * channels..(skip + keep) * channels]);
        self.output_frames += keep as u64;
        // With `partial_len` the resampler reports the full chunk as read;
        // only the real frames count as consumed.
        Ok(partial.map_or(frames_in, |partial| partial.min(frames_in)))
    }
}

/// Creates the track for `key` and starts a decoder thread filling it from
/// `path`. The thread marks the track finished after pushing its final
/// sample, stops early once the mixer has dropped the track, and reports
/// failure through `on_failure`.
pub(crate) fn spawn_decoder(
    key: TrackKey,
    path: PathBuf,
    out_rate: u32,
    out_channels: usize,
    on_failure: impl FnOnce(String) + Send + 'static,
) -> std::io::Result<(Track, JoinHandle<()>)> {
    let (track, producer, shared) = Track::new(key, out_rate, out_channels);
    let handle = thread::Builder::new()
        .name("elizabeats-decoder".into())
        .spawn(move || {
            // Symphonia can panic on malformed streams; never let that take
            // anything else down.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                pump(&path, out_rate, out_channels, producer, &shared)
            }));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(message)) => on_failure(message),
                Err(_) => on_failure("The audio decoder crashed on this file.".to_string()),
            }
        })?;
    Ok((track, handle))
}

/// Decodes the whole file into `producer`, waiting whenever it is full.
fn pump(
    path: &Path,
    out_rate: u32,
    out_channels: usize,
    mut producer: Producer<f32>,
    shared: &TrackShared,
) -> Result<(), String> {
    let mut decoder = SourceDecoder::open(path, out_rate, out_channels)?;
    shared.set_duration(decoder.duration_secs());
    let mut batch = Vec::new();
    loop {
        let more = decoder.read(&mut batch)?;
        if !push_all(&batch, out_channels, &mut producer) {
            return Ok(()); // the track was unloaded
        }
        if !more {
            shared.mark_finished();
            return Ok(());
        }
    }
}

/// Pushes whole frames into the ring buffer, sleeping while it is full.
/// Returns false if the mixer dropped the track.
fn push_all(samples: &[f32], channels: usize, producer: &mut Producer<f32>) -> bool {
    let mut rest = samples;
    while !rest.is_empty() {
        if producer.is_abandoned() {
            return false;
        }
        let count = (producer.slots() / channels * channels).min(rest.len());
        if count == 0 {
            thread::sleep(FULL_BUFFER_WAIT);
            continue;
        }
        if producer.push_entire_slice(&rest[..count]).is_err() {
            return false;
        }
        rest = &rest[count..];
    }
    true
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// Decoding speed check (decode + resample to 48 kHz stereo), for
    /// comparing build settings: `ELIZABEATS_BENCH_FILE=song.m4a cargo test
    /// --release -p elizabeats-audio decode_speed -- --ignored --nocapture`.
    #[test]
    #[ignore = "benchmark; needs ELIZABEATS_BENCH_FILE"]
    fn decode_speed() {
        let Some(path) = std::env::var_os("ELIZABEATS_BENCH_FILE") else {
            return;
        };
        let started = std::time::Instant::now();
        let (samples, _) = decode_all(Path::new(&path), 48_000, 2);
        let audio_secs = samples.len() as f64 / 2.0 / 48_000.0;
        let took = started.elapsed().as_secs_f64();
        println!(
            "decoded {audio_secs:.0} s of audio in {took:.3} s = {:.0}x real time",
            audio_secs / took
        );
    }

    /// Writes a 16-bit PCM WAV file with a sine tone on every channel.
    pub(crate) fn write_wav(path: &Path, rate: u32, channels: u16, frames: usize, freq: f32) {
        let data_len = frames * channels as usize * 2;
        let mut bytes = Vec::with_capacity(44 + data_len);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&rate.to_le_bytes());
        bytes.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
        bytes.extend_from_slice(&(channels * 2).to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
        for n in 0..frames {
            let value = (n as f32 * freq * std::f32::consts::TAU / rate as f32).sin() * 0.5;
            let sample = (value * i16::MAX as f32) as i16;
            for _ in 0..channels {
                bytes.extend_from_slice(&sample.to_le_bytes());
            }
        }
        let mut file = File::create(path).unwrap();
        file.write_all(&bytes).unwrap();
    }

    fn decode_all(path: &Path, rate: u32, channels: usize) -> (Vec<f32>, Option<f64>) {
        let mut decoder = SourceDecoder::open(path, rate, channels).unwrap();
        let duration = decoder.duration_secs();
        let mut all = Vec::new();
        let mut batch = Vec::new();
        loop {
            let more = decoder.read(&mut batch).unwrap();
            all.extend_from_slice(&batch);
            if !more {
                break;
            }
        }
        (all, duration)
    }

    #[test]
    fn decodes_wav_at_native_rate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tone.wav");
        write_wav(&path, 48_000, 2, 12_345, 440.0);
        let (samples, duration) = decode_all(&path, 48_000, 2);
        assert_eq!(samples.len(), 12_345 * 2);
        let expected = 12_345.0 / 48_000.0;
        assert!((duration.unwrap() - expected).abs() < 1e-9);
        // 16-bit 0.5 amplitude sine survives decoding.
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        assert!((peak - 0.5).abs() < 0.01, "{peak}");
    }

    #[test]
    fn mono_wav_maps_to_stereo() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mono.wav");
        write_wav(&path, 44_100, 1, 1000, 440.0);
        let (samples, _) = decode_all(&path, 44_100, 2);
        assert_eq!(samples.len(), 2000);
        assert!(samples.chunks(2).all(|f| f[0] == f[1]));
    }

    #[test]
    fn resamples_to_exact_length_and_keeps_timing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cd.wav");
        // 1 s of 441 Hz at 44.1 kHz: exactly 441 cycles.
        write_wav(&path, 44_100, 2, 44_100, 441.0);
        let (samples, duration) = decode_all(&path, 48_000, 2);
        assert_eq!(samples.len(), 48_000 * 2);
        assert!((duration.unwrap() - 1.0).abs() < 1e-9);

        // Compare against an ideal 441 Hz sine at 48 kHz, away from the edges,
        // allowing for a sub-sample timing offset (the filter delay is
        // trimmed in whole frames).
        let deviation = |lag: f32| {
            (2_000..46_000)
                .map(|n| {
                    let t = (n as f32 - lag) * 441.0 * std::f32::consts::TAU / 48_000.0;
                    (samples[n * 2] - t.sin() * 0.5).abs()
                })
                .fold(0.0f32, f32::max)
        };
        let best = (-10..=10)
            .map(|tenths| deviation(tenths as f32 / 10.0))
            .fold(f32::MAX, f32::min);
        assert!(best < 0.002, "resampled signal deviates by {best}");
    }

    #[test]
    fn missing_file_is_an_error() {
        let result = SourceDecoder::open(Path::new("/definitely/not/here.mp3"), 48_000, 2);
        let message = result.err().unwrap();
        assert!(message.contains("Could not open"), "{message}");
    }

    #[test]
    fn garbage_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("junk.mp3");
        std::fs::write(&path, vec![0x42u8; 4096]).unwrap();
        assert!(SourceDecoder::open(&path, 48_000, 2).is_err());
    }

    #[test]
    fn resampler_stream_lengths_for_common_ratios() {
        for (from, to) in [
            (44_100, 48_000),
            (48_000, 44_100),
            (22_050, 48_000),
            (96_000, 48_000),
        ] {
            let mut resampler = StreamResampler::new(from, to, 1).unwrap();
            let mut out = Vec::new();
            let input = vec![0.25f32; 3_333];
            for chunk in input.chunks(700) {
                resampler.process(chunk, &mut out).unwrap();
            }
            resampler.flush(&mut out).unwrap();
            let expected = (3_333.0 * to as f64 / from as f64).round() as usize;
            assert_eq!(out.len(), expected, "{from} -> {to}");
            // DC level is preserved in the middle.
            let mid = out[out.len() / 2];
            assert!((mid - 0.25).abs() < 0.01, "{from} -> {to}: {mid}");
        }
    }
}
