//! Header estimates: time until the next Break, and queued Track time.

use crate::domain::PlaybackProgress;
use crate::queue::{ItemKind, Queue, QueueItemRef};

fn track_duration(kind: &ItemKind, progress: Option<PlaybackProgress>) -> Option<f64> {
    let ItemKind::Track(track) = kind else {
        return None;
    };
    match progress {
        Some(progress) if progress.duration > 0.0 => Some(progress.duration),
        _ => track.duration_seconds,
    }
}

/// Seconds until the next Break starts, assuming crossfades between adjacent
/// Tracks. `None` while stopped, during a Break, when no Break is queued, or
/// when a Track's duration before the Break is unknown.
pub fn estimate_until_next_break(queue: &Queue, crossfade_seconds: f64) -> Option<f64> {
    let active = queue.active()?;
    let duration = track_duration(&active.kind, Some(active.progress))?;
    let mut estimate = (duration - active.progress.current_time).max(0.0);
    for item in queue.items().skip(1) {
        let progress = match item {
            QueueItemRef::Incoming(incoming) => Some(incoming.progress),
            _ => None,
        };
        if matches!(item.kind(), ItemKind::Break) {
            return Some(estimate.max(0.0));
        }
        estimate += track_duration(item.kind(), progress)? - crossfade_seconds;
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
    use crate::domain::{SourceId, Track};
    use crate::queue::{PlayingItem, UpcomingItem};

    fn track(id: &str, duration: Option<f64>) -> Track {
        Track {
            id: id.into(),
            source: SourceId::Local,
            title: id.into(),
            artist: "a".into(),
            album: None,
            duration_seconds: duration,
            artwork_url: None,
            explicit: false,
        }
    }

    fn active(duration: f64, current: f64) -> PlayingItem {
        PlayingItem {
            id: "a".into(),
            kind: ItemKind::Track(track("a", Some(999.0))),
            progress: PlaybackProgress {
                current_time: current,
                duration,
            },
        }
    }

    #[test]
    fn stops_at_break() {
        let queue = Queue::Playing {
            active: active(100.0, 40.0),
            queued: vec![
                UpcomingItem::track("b".into(), track("b", Some(50.0))),
                UpcomingItem::new_break("br".into()),
                UpcomingItem::track("c".into(), track("c", Some(50.0))),
            ],
        };
        assert_eq!(estimate_until_next_break(&queue, 3.0), Some(60.0 + 47.0));
    }

    #[test]
    fn no_break_queued_has_no_estimate() {
        let queue = Queue::Playing {
            active: active(100.0, 40.0),
            queued: vec![UpcomingItem::track("b".into(), track("b", Some(50.0)))],
        };
        assert_eq!(estimate_until_next_break(&queue, 3.0), None);
    }

    #[test]
    fn unknown_duration_is_unknown() {
        let queue = Queue::Playing {
            active: active(100.0, 0.0),
            queued: vec![
                UpcomingItem::track("b".into(), track("b", None)),
                UpcomingItem::new_break("br".into()),
            ],
        };
        assert_eq!(estimate_until_next_break(&queue, 3.0), None);
        assert_eq!(estimate_until_next_break(&Queue::default(), 3.0), None);
    }

    #[test]
    fn break_active_has_no_estimate() {
        let queue = Queue::Playing {
            active: PlayingItem {
                id: "br".into(),
                kind: ItemKind::Break,
                progress: PlaybackProgress::default(),
            },
            queued: vec![],
        };
        assert_eq!(estimate_until_next_break(&queue, 3.0), None);
    }

    #[test]
    fn queued_duration_flags_unknown() {
        let queue = Queue::Stopped {
            queued: vec![
                UpcomingItem::track("b".into(), track("b", Some(50.0))),
                UpcomingItem::new_break("br".into()),
                UpcomingItem::track("c".into(), track("c", None)),
            ],
        };
        let total = queued_track_duration(queue.items());
        assert_eq!(total.seconds, 50.0);
        assert!(total.has_unknown);
    }
}
