//! Mapping decoded audio onto the output device's channel layout.
//!
//! Source channel order is assumed to follow the WAV/SMPTE convention that
//! Symphonia also uses: front-left, front-right, centre, LFE, back-left,
//! back-right, side-left, side-right.

/// −3 dB, the usual weight for centre and surround channels in a stereo downmix.
const MINUS_3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Converts interleaved `input` with `in_channels` into interleaved `output`
/// with `out_channels`. `output` is cleared and refilled.
///
/// Rules:
/// - same channel count: copied unchanged;
/// - mono source: duplicated onto left and right (other outputs silent);
/// - multichannel source on a device with at least as many channels: copied
///   channel-for-channel, extra outputs silent;
/// - otherwise the source is folded down to stereo (or mono) and written to
///   the first output channels, extra outputs silent.
pub(crate) fn map_channels(
    input: &[f32],
    in_channels: usize,
    output: &mut Vec<f32>,
    out_channels: usize,
) {
    output.clear();
    if in_channels == 0 || out_channels == 0 {
        return;
    }
    let frames = input.len() / in_channels;
    output.resize(frames * out_channels, 0.0);

    if in_channels == out_channels {
        output.copy_from_slice(&input[..frames * in_channels]);
        return;
    }

    let frame_pairs = input
        .chunks_exact(in_channels)
        .zip(output.chunks_exact_mut(out_channels));

    if in_channels > 2 && out_channels > in_channels {
        // e.g. 5.1 file on a 7.1 device: the layouts share their first channels.
        for (src, dst) in frame_pairs {
            dst[..in_channels].copy_from_slice(src);
        }
        return;
    }

    for (src, dst) in frame_pairs {
        let (left, right) = stereo_of(src);
        if out_channels == 1 {
            dst[0] = 0.5 * (left + right);
        } else {
            dst[0] = left;
            dst[1] = right;
        }
    }
}

/// Folds one source frame down to a (left, right) pair.
fn stereo_of(frame: &[f32]) -> (f32, f32) {
    let at = |index: usize| frame.get(index).copied().unwrap_or(0.0);
    match frame.len() {
        0 => (0.0, 0.0),
        1 => (frame[0], frame[0]),
        2 => (frame[0], frame[1]),
        _ => {
            // ITU-style downmix (LFE dropped), normalised so a full-scale
            // signal on every channel cannot exceed full scale.
            let centre = at(2) * MINUS_3DB;
            let left = frame[0] + centre + (at(4) + at(6)) * MINUS_3DB;
            let right = frame[1] + centre + (at(5) + at(7)) * MINUS_3DB;
            let has_back = frame.len() > 4;
            let has_side = frame.len() > 6;
            let norm = 1.0
                + MINUS_3DB
                + if has_back { MINUS_3DB } else { 0.0 }
                + if has_side { MINUS_3DB } else { 0.0 };
            (left / norm, right / norm)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(input: &[f32], in_ch: usize, out_ch: usize) -> Vec<f32> {
        let mut out = Vec::new();
        map_channels(input, in_ch, &mut out, out_ch);
        out
    }

    #[test]
    fn identical_layout_is_copied() {
        assert_eq!(map(&[0.1, 0.2, 0.3, 0.4], 2, 2), vec![0.1, 0.2, 0.3, 0.4]);
    }

    #[test]
    fn mono_is_duplicated_to_stereo() {
        assert_eq!(map(&[0.5, -0.25], 1, 2), vec![0.5, 0.5, -0.25, -0.25]);
    }

    #[test]
    fn mono_on_four_channel_device_uses_front_pair_only() {
        assert_eq!(map(&[0.5], 1, 4), vec![0.5, 0.5, 0.0, 0.0]);
    }

    #[test]
    fn stereo_to_mono_averages() {
        assert_eq!(map(&[1.0, 0.0, 0.2, 0.4], 2, 1), vec![0.5, 0.3]);
    }

    #[test]
    fn stereo_on_six_channel_device_leaves_others_silent() {
        assert_eq!(map(&[0.1, 0.2], 2, 6), vec![0.1, 0.2, 0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn five_one_downmixes_to_stereo_without_clipping() {
        let out = map(&[1.0; 6], 6, 2);
        assert_eq!(out.len(), 2);
        assert!((out[0] - 1.0).abs() < 1e-6, "{out:?}");
        assert!((out[1] - 1.0).abs() < 1e-6, "{out:?}");
        // Centre-only content lands equally in both sides.
        let out = map(&[0.0, 0.0, 1.0, 0.0, 0.0, 0.0], 6, 2);
        assert!(out[0] > 0.0 && (out[0] - out[1]).abs() < 1e-6);
    }

    #[test]
    fn five_one_on_eight_channel_device_is_copied_through() {
        let src = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let out = map(&src, 6, 8);
        assert_eq!(&out[..6], &src);
        assert_eq!(&out[6..], &[0.0, 0.0]);
    }

    #[test]
    fn partial_trailing_frame_is_ignored() {
        assert_eq!(map(&[0.1, 0.2, 0.3], 2, 2), vec![0.1, 0.2]);
    }
}
