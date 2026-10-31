//! Header: date, clock, next hour, Track remaining, next break.

use chrono::{DateTime, Local, Timelike};
use elizabeats_core::format::format_duration;
use elizabeats_core::queue::{ItemKind, Queue};
use elizabeats_core::session::Session;
use elizabeats_core::timing::{KnownDuration, estimate_until_next_break, queued_track_duration};
use gpui_kit::component::{ActiveTheme, StyledExt, TitleBar, h_flex};
use gpui_kit::{
    Context, Decorations, InteractiveElement, IntoElement, ParentElement, Pixels, Point,
    SharedString, Styled, Window, div, point, px,
};

use super::{AppView, tabular};

/// Header metric values, computed without any UI so they can be tested.
#[derive(Debug, PartialEq)]
pub struct Metrics {
    pub date: String,
    pub time: String,
    pub next_hour: String,
    pub current_label: &'static str,
    pub current_value: String,
    pub boundary_label: &'static str,
    pub boundary_value: String,
}

fn countdown(seconds: Option<f64>) -> String {
    format_duration(seconds.map(f64::ceil))
}

fn elapsed(seconds: Option<f64>) -> String {
    format_duration(seconds.map(f64::floor))
}

fn known_duration(value: KnownDuration) -> String {
    if value.seconds == 0.0 && !value.has_unknown {
        "—".to_owned()
    } else if value.seconds == 0.0 {
        "Unknown".to_owned()
    } else {
        format!(
            "{}{}",
            if value.has_unknown { "≥" } else { "" },
            format_duration(Some(value.seconds))
        )
    }
}

pub fn metrics(session: &Session, now: DateTime<Local>) -> Metrics {
    let seconds_into_hour =
        f64::from(now.minute() * 60 + now.second()) + f64::from(now.nanosecond()) / 1e9;
    let next_hour = 3600.0 - seconds_into_hour;
    let queue: &Queue = session.queue();
    let active = queue.active();
    let is_break = active.is_some_and(|item| matches!(item.kind, ItemKind::Break));

    let (current_label, current_value) = if is_break {
        let started = session
            .active_log_entry()
            .and_then(|entry| chrono::DateTime::parse_from_rfc3339(&entry.started_at).ok());
        let seconds = started.map(|started| {
            (now.timestamp_millis() - started.timestamp_millis()).max(0) as f64 / 1000.0
        });
        ("Break elapsed", elapsed(seconds))
    } else {
        let remaining = active.and_then(|item| match &item.kind {
            ItemKind::Track(track) => {
                if item.progress.duration > 0.0 {
                    Some((item.progress.duration - item.progress.current_time).max(0.0))
                } else {
                    track.duration_seconds
                }
            }
            ItemKind::Break => None,
        });
        ("Track remaining", countdown(remaining))
    };

    let (boundary_label, boundary_value) = if is_break {
        (
            "Queued track time",
            known_duration(queued_track_duration(queue.items().skip(1))),
        )
    } else {
        (
            "Next break",
            countdown(estimate_until_next_break(
                queue,
                f64::from(session.crossfade_seconds()),
            )),
        )
    };

    Metrics {
        date: now.format("%a, %b %-d").to_string(),
        time: now.format("%I:%M:%S %p").to_string(),
        next_hour: countdown(Some(next_hour)),
        current_label,
        current_value,
        boundary_label,
        boundary_value,
    }
}

/// The header doubles as the window's title bar: drag it to move the window,
/// double-click it to zoom, and on macOS the traffic lights sit at its left.
pub const HEADER_HEIGHT: f32 = 80.;

/// Space on either side of the macOS traffic lights.
const TRAFFIC_LIGHT_INSET: f32 = 24.;

/// Width of the three macOS traffic lights together (measured).
const TRAFFIC_LIGHTS_WIDTH: f32 = 60.;

/// Where macOS draws the traffic lights: vertically centered in the header
/// (gpui centers them in a bar `2 * y` taller than the buttons).
pub fn traffic_light_position() -> Point<Pixels> {
    const BUTTON_HEIGHT: f32 = 14.;
    point(
        px(TRAFFIC_LIGHT_INSET),
        px((HEADER_HEIGHT - BUTTON_HEIGHT) / 2.),
    )
}

/// Right padding after the settings button when gpui-kit draws minimize,
/// maximize and close itself (Windows, and Linux without server-side
/// decorations): each is a 34 px box around a 14 px icon, flush with the
/// window edge, so 10 px makes the gap from settings to minimize match the
/// gaps between those buttons. Elsewhere the button sits at the header's edge.
fn settings_button_padding(window: &Window) -> f32 {
    let draws_window_controls = cfg!(target_os = "windows")
        || (cfg!(target_os = "linux")
            && matches!(window.window_decorations(), Decorations::Client { .. }));
    if draws_window_controls { 10. } else { 24. }
}

/// Room left of the header's contents for the macOS traffic lights, with the
/// same gap after them as before them.
const TRAFFIC_LIGHT_SPACE: f32 = TRAFFIC_LIGHT_INSET + TRAFFIC_LIGHTS_WIDTH + TRAFFIC_LIGHT_INSET;

impl AppView {
    pub(super) fn render_header(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let metrics = metrics(self.station.read(cx).session(), self.now);
        let metric = |value: String, label: &'static str, cx: &Context<Self>| {
            div()
                .child(
                    div()
                        .text_xs()
                        .font_medium()
                        .font_features(tabular())
                        .child(SharedString::from(value)),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
        };
        TitleBar::new()
            .h(px(HEADER_HEIGHT))
            .pl(px(if cfg!(target_os = "macos") {
                TRAFFIC_LIGHT_SPACE
            } else {
                24.
            }))
            .border_color(cx.theme().border)
            .bg(cx
                .theme()
                .muted
                .opacity(if cx.theme().is_dark() { 0.0 } else { 0.35 }))
            .child(
                h_flex()
                    .gap_5()
                    .child(metric(metrics.date, "Today's date", cx))
                    .child(metric(metrics.time, "Current time", cx))
                    .child(metric(metrics.next_hour, "Next hour", cx))
                    .child(metric(metrics.current_value, metrics.current_label, cx))
                    .child(metric(metrics.boundary_value, metrics.boundary_label, cx)),
            )
            .child(
                div()
                    .pr(px(settings_button_padding(window)))
                    // On Windows the header is a native drag region, and
                    // Windows treats a click anywhere in it as the start of a
                    // window drag unless something occludes it there.
                    .occlude()
                    .child(self.render_settings_button(window, cx)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn idle_header() {
        let session = Session::default();
        let now = Local.with_ymd_and_hms(2026, 10, 7, 14, 59, 30).single();
        let Some(now) = now else { return };
        let metrics = metrics(&session, now);
        assert_eq!(metrics.time, "02:59:30 PM");
        assert_eq!(metrics.next_hour, "0:30");
        assert_eq!(metrics.current_label, "Track remaining");
        assert_eq!(metrics.current_value, "—");
        assert_eq!(metrics.boundary_value, "—");
        assert_eq!(metrics.date, "Wed, Oct 7");
    }

    #[test]
    fn known_duration_formats() {
        assert_eq!(
            known_duration(KnownDuration {
                seconds: 0.0,
                has_unknown: false
            }),
            "—"
        );
        assert_eq!(
            known_duration(KnownDuration {
                seconds: 0.0,
                has_unknown: true
            }),
            "Unknown"
        );
        assert_eq!(
            known_duration(KnownDuration {
                seconds: 125.0,
                has_unknown: true
            }),
            "≥2:05"
        );
    }
}
