//! Header estimates: time until the next Break, and queued Track time.

use crate::queue::{Queue, QueueItemRef};

/// A Track's duration: as reported by the engine while it plays, else as listed.
fn track_duration(item: QueueItemRef<'_>) -> Option<f64> {
    let track = item.track()?;
    match item.progress() {
        Some(progress) if progress.duration > 0.0 => Some(progress.duration),
        _ => track.duration_seconds,
    }
}

/// Seconds until the next Break starts, assuming crossfades between adjacent
/// Tracks. `None` while stopped, during a Break, when no Break is queued, or
/// when a Track's duration before the Break is unknown.
pub fn estimate_until_next_break(queue: &Queue, crossfade_seconds: f64) -> Option<f64> {
    let mut items = queue.items();
    let active = items.next()?;
    let current = active.progress()?.current_time;
    let mut estimate = (track_duration(active)? - current).max(0.0);
    for item in items {
        if item.track().is_none() {
            return Some(estimate.max(0.0));
        }
        estimate += track_duration(item)? - crossfade_seconds;
    }
    // Reached the end of the Queue without a Break.
    None
}

/// Total duration of the Tracks in `items`, noting whether any are unknown.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KnownDuration {
    pub seconds: f64,
    pub has_unknown: bool,
}

pub fn queued_track_duration<'a>(
    items: impl IntoIterator<Item = QueueItemRef<'a>>,
) -> KnownDuration {
    let mut total = KnownDuration {
        seconds: 0.0,
        has_unknown: false,
    };
    for item in items {
        if let Some(track) = item.track() {
            match track.duration_seconds {
                Some(seconds) => total.seconds += seconds,
                None => total.has_unknown = true,
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{PlaybackProgress, QueueItem, SourceId, Track};
    use crate::queue::{Playback, PlayingItem};

    fn track(id: &str, duration: Option<f64>) -> QueueItem {
        QueueItem::track(
            id,
            Track {
                id: id.into(),
                source: SourceId::Local,
                title: id.into(),
                artist: "a".into(),
                album: None,
                duration_seconds: duration,
                artwork_url: None,
                explicit: false,
            },
        )
    }

    fn playing(item: QueueItem, duration: f64, current: f64, upcoming: Vec<QueueItem>) -> Queue {
        let mut active = PlayingItem::new(item, Some(1));
        active.progress = PlaybackProgress {
            current_time: current,
            duration,
        };
        Queue {
            playback: Playback::playing(active),
            upcoming,
        }
    }

    #[test]
    fn stops_at_break() {
        let queue = playing(
            track("a", Some(999.0)),
            100.0,
            40.0,
            vec![
                track("b", Some(50.0)),
                QueueItem::new_break("br"),
                track("c", Some(50.0)),
            ],
        );
        assert_eq!(estimate_until_next_break(&queue, 3.0), Some(60.0 + 47.0));
    }

    #[test]
    fn no_estimate_without_a_break_or_a_known_duration() {
        let a = || track("a", Some(999.0));
        let no_break = playing(a(), 100.0, 40.0, vec![track("b", Some(50.0))]);
        assert_eq!(estimate_until_next_break(&no_break, 3.0), None);
        let unknown = playing(
            a(),
            100.0,
            0.0,
            vec![track("b", None), QueueItem::new_break("br")],
        );
        assert_eq!(estimate_until_next_break(&unknown, 3.0), None);
        assert_eq!(estimate_until_next_break(&Queue::default(), 3.0), None);
        let on_break = playing(QueueItem::new_break("br"), 0.0, 0.0, vec![]);
        assert_eq!(estimate_until_next_break(&on_break, 3.0), None);
    }

    #[test]
    fn queued_duration_flags_unknown() {
        let queue = Queue::stopped(vec![
            track("b", Some(50.0)),
            QueueItem::new_break("br"),
            track("c", None),
        ]);
        let total = queued_track_duration(queue.items());
        assert_eq!(total.seconds, 50.0);
        assert!(total.has_unknown);
    }
}
