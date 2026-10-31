//! The Log tab: what actually entered Playing, by Session or by date.

use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDate};
use elizabeats_core::{PlayLogEntry, PlayLogReason, StoredQueueItem};
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::Button;
use gpui_kit::component::date_picker::DatePicker;
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, ClipboardItem, Context, IntoElement, ParentElement, SharedString, Styled, Window,
    div, px, uniform_list,
};

use super::{AppView, RowSize, break_tile, row, track_artwork, track_details};

pub fn parse_time(value: &str) -> Option<DateTime<Local>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Local))
}

pub fn local_date(value: &str) -> Option<NaiveDate> {
    parse_time(value).map(|time| time.date_naive())
}

fn status_label(reason: Option<PlayLogReason>) -> &'static str {
    match reason {
        Some(PlayLogReason::Completed) => "Completed",
        Some(PlayLogReason::Skipped) => "Skipped",
        Some(PlayLogReason::Error) => "Error",
        None => "Playing",
    }
}

/// Entries for a date, or for the current Session when no date is chosen.
pub fn filter_entries(
    entries: &[PlayLogEntry],
    date: Option<NaiveDate>,
    session_started_at: DateTime<chrono::Utc>,
) -> Vec<PlayLogEntry> {
    entries
        .iter()
        .filter(|entry| match date {
            Some(date) => local_date(&entry.started_at) == Some(date),
            None => DateTime::parse_from_rfc3339(&entry.started_at)
                .is_ok_and(|started| started >= session_started_at),
        })
        .cloned()
        .collect()
}

/// Oldest first, 24-hour times: `Artist - Title - 14:03` or `[Break - 14:07]`.
pub fn clipboard_text(entries: &[PlayLogEntry]) -> String {
    let mut sorted: Vec<&PlayLogEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    sorted
        .into_iter()
        .map(|entry| {
            let time = parse_time(&entry.started_at)
                .map(|time| time.format("%H:%M").to_string())
                .unwrap_or_default();
            match &entry.item {
                StoredQueueItem::Break { .. } => format!("[Break - {time}]"),
                StoredQueueItem::Track { track, .. } => {
                    format!("{} - {} - {time}", track.artist, track.title)
                }
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl AppView {
    pub(super) fn render_log(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let station = self.station.read(cx);
        let session = station.session();
        let all_count = session.play_log().len();
        let entries: Arc<Vec<PlayLogEntry>> = Arc::new(filter_entries(
            session.play_log(),
            self.log_date,
            session.session_started_at(),
        ));

        let body: AnyElement = if entries.is_empty() {
            let (title, description) = match (all_count, self.log_date) {
                (0, _) => (
                    "No play log entries yet",
                    "Start a track to see it recorded here.",
                ),
                (_, Some(_)) => (
                    "No entries for this date",
                    "Choose another date to view its history.",
                ),
                (_, None) => (
                    "No entries in this session",
                    "Choose a date to view earlier history.",
                ),
            };
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    Empty::new().header(
                        EmptyHeader::new()
                            .media(
                                EmptyMedia::new()
                                    .with_variant(EmptyMediaVariant::Icon)
                                    .child(Icon::new(Lucide::Clock)),
                            )
                            .title(EmptyTitle::new().child(title))
                            .description(EmptyDescription::new().child(description)),
                    ),
                )
                .into_any_element()
        } else {
            let list_entries = entries.clone();
            uniform_list(
                "play-log",
                entries.len(),
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    range
                        .filter_map(|index| list_entries.get(index))
                        .map(|entry| this.render_log_row(entry, cx))
                        .collect::<Vec<_>>()
                }),
            )
            .size_full()
            .into_any_element()
        };

        div().size_full().child(body)
    }

    /// The Log's date filter and Copy button, shown in the panel header
    /// (to the right of the tabs) while the Log tab is open.
    pub(super) fn render_log_controls(
        &mut self,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let session = self.station.read(cx).session();
        let entries = filter_entries(
            session.play_log(),
            self.log_date,
            session.session_started_at(),
        );
        let is_empty = entries.is_empty();
        h_flex()
            .flex_1()
            .justify_end()
            .gap_2()
            .pr_5()
            .child(
                div().w(px(170.)).child(
                    DatePicker::new(&self.log_date_picker)
                        .small()
                        .placeholder("Current session")
                        .cleanable(true),
                ),
            )
            .child(
                Button::new("copy-log")
                    .outline()
                    .small()
                    .icon(if self.log_copied {
                        IconName::Check
                    } else {
                        IconName::Copy
                    })
                    .label("Copy")
                    .disabled(is_empty)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(clipboard_text(&entries)));
                        this.log_copied = true;
                        cx.notify();
                        cx.spawn(async move |this, cx| {
                            cx.background_executor().timer(Duration::from_secs(3)).await;
                            let _ = this.update(cx, |this, cx| {
                                this.log_copied = false;
                                cx.notify();
                            });
                        })
                        .detach();
                    })),
            )
    }

    fn render_log_row(&mut self, entry: &PlayLogEntry, cx: &mut Context<Self>) -> AnyElement {
        let time = parse_time(&entry.started_at)
            .map(|time| time.format("%I:%M %p").to_string())
            .unwrap_or_default();
        let (media, details): (AnyElement, AnyElement) = match &entry.item {
            StoredQueueItem::Track { track, .. } => {
                let image = self
                    .artwork
                    .update(cx, |artwork, cx| artwork.get(track, cx));
                (
                    track_artwork(image, RowSize::Regular.media(), cx).into_any_element(),
                    track_details(track, Vec::new(), cx).into_any_element(),
                )
            }
            StoredQueueItem::Break { .. } => (
                break_tile(RowSize::Regular.media(), cx).into_any_element(),
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().font_medium().child("Break"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Manual stop"),
                    )
                    .into_any_element(),
            ),
        };
        row(RowSize::Regular)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(media)
            .child(details)
            .child(
                v_flex()
                    .flex_none()
                    .items_end()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(status_label(entry.reason))
                    .child(SharedString::from(time)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use elizabeats_core::{SourceId, Track};

    fn entry(id: &str, started_at: &str, item: StoredQueueItem) -> PlayLogEntry {
        PlayLogEntry {
            id: id.into(),
            item,
            started_at: started_at.into(),
            ended_at: None,
            reason: None,
        }
    }

    fn track_item() -> StoredQueueItem {
        StoredQueueItem::Track {
            id: "q".into(),
            track: Track {
                id: "t".into(),
                source: SourceId::Local,
                title: "Song".into(),
                artist: "Band".into(),
                album: None,
                duration_seconds: None,
                artwork_url: None,
                explicit: false,
            },
        }
    }

    #[test]
    fn clipboard_is_oldest_first() {
        let local = |h: u32, m: u32| {
            chrono::TimeZone::with_ymd_and_hms(&Local, 2026, 10, 7, h, m, 0)
                .single()
                .map(|time| time.to_rfc3339())
                .unwrap_or_default()
        };
        let entries = vec![
            entry(
                "2",
                &local(14, 7),
                StoredQueueItem::Break { id: "b".into() },
            ),
            entry("1", &local(14, 3), track_item()),
        ];
        assert_eq!(
            clipboard_text(&entries),
            "Band - Song - 14:03\n[Break - 14:07]"
        );
    }

    #[test]
    fn filters_by_session_or_date() {
        let start = chrono::DateTime::parse_from_rfc3339("2026-10-07T12:00:00Z")
            .map(|time| time.with_timezone(&chrono::Utc))
            .unwrap_or_default();
        let entries = vec![
            entry("old", "2026-10-07T11:00:00Z", track_item()),
            entry("new", "2026-10-07T13:00:00Z", track_item()),
        ];
        let current = filter_entries(&entries, None, start);
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].id, "new");
        let date = local_date("2026-10-07T11:00:00Z");
        assert_eq!(filter_entries(&entries, date, start).len(), 2);
    }
}
