//! Text formatting shared by the UI and clipboard exports.

/// `m:ss` or `h:mm:ss`; `—` when unknown.
pub fn format_duration(seconds: Option<f64>) -> String {
    let Some(seconds) = seconds.filter(|value| value.is_finite()) else {
        return "—".to_owned();
    };
    // Rounded and clamped; the cast is safe because the value is finite and non-negative.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let value = seconds.max(0.0).round() as u64;
    let hours = value / 3600;
    let minutes = (value % 3600) / 60;
    let remainder = value % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{remainder:02}")
    } else {
        format!("{minutes}:{remainder:02}")
    }
}

/// `512 KiB` below 1 MiB, otherwise GiB with one decimal under 10 GiB.
pub fn format_bytes(bytes: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    let bytes = bytes as f64;
    if bytes < 1024.0 * 1024.0 {
        return format!("{:.0} KiB", bytes / 1024.0);
    }
    let gibibytes = bytes / (1024.0 * 1024.0 * 1024.0);
    if gibibytes >= 10.0 {
        format!("{gibibytes:.0} GiB")
    } else {
        let text = format!("{gibibytes:.1}");
        format!("{} GiB", text.strip_suffix(".0").unwrap_or(&text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(format_duration(None), "—");
        assert_eq!(format_duration(Some(f64::NAN)), "—");
        assert_eq!(format_duration(Some(-5.0)), "0:00");
        assert_eq!(format_duration(Some(59.4)), "0:59");
        assert_eq!(format_duration(Some(59.6)), "1:00");
        assert_eq!(format_duration(Some(3725.0)), "1:02:05");
    }

    #[test]
    fn bytes() {
        assert_eq!(format_bytes(0), "0 KiB");
        assert_eq!(format_bytes(512 * 1024), "512 KiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1 GiB");
        assert_eq!(format_bytes(1536 * 1024 * 1024), "1.5 GiB");
        assert_eq!(format_bytes(12 * 1024 * 1024 * 1024), "12 GiB");
        assert_eq!(format_bytes(200 * 1024 * 1024), "0.2 GiB");
    }
}
