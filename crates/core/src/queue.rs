//! The Queue: DJ-authored Tracks and Breaks, plus the pure editing rules.
//!
//! Commands can change Upcoming Items only. The Active Item and an Incoming
//! Track keep their roles (and positions) until playback moves them.

use crate::domain::{PlaybackProgress, StoredQueueItem, Track};

/// Whether an Upcoming Track's audio is ready to play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    Preparing,
    Ready,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ItemKind {
    Track(Track),
    Break,
}

/// A Queue item that has not begun Playing.
#[derive(Clone, Debug, PartialEq)]
pub struct UpcomingItem {
    pub id: String,
    pub kind: ItemKind,
    /// Always `Ready` for Breaks.
    pub readiness: Readiness,
}

impl UpcomingItem {
    pub fn track(id: String, track: Track) -> Self {
        Self {
            id,
            kind: ItemKind::Track(track),
            readiness: Readiness::Preparing,
        }
    }

    pub fn new_break(id: String) -> Self {
        Self {
            id,
            kind: ItemKind::Break,
            readiness: Readiness::Ready,
        }
    }
}

/// A Playing item (the Active Item or the Incoming Track).
#[derive(Clone, Debug, PartialEq)]
pub struct PlayingItem {
    pub id: String,
    pub kind: ItemKind,
    pub progress: PlaybackProgress,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Queue {
    Stopped {
        queued: Vec<UpcomingItem>,
    },
    Playing {
        active: PlayingItem,
        queued: Vec<UpcomingItem>,
    },
    /// Both outgoing and incoming are Tracks.
    Crossfading {
        active: PlayingItem,
        incoming: PlayingItem,
        queued: Vec<UpcomingItem>,
    },
}

impl Default for Queue {
    fn default() -> Self {
        Self::Stopped { queued: Vec::new() }
    }
}

/// A borrowed view of any Queue item, in display order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QueueItemRef<'a> {
    Active(&'a PlayingItem),
    Incoming(&'a PlayingItem),
    Upcoming(&'a UpcomingItem),
}

impl<'a> QueueItemRef<'a> {
    pub fn id(self) -> &'a str {
        match self {
            Self::Active(item) | Self::Incoming(item) => &item.id,
            Self::Upcoming(item) => &item.id,
        }
    }

    pub fn kind(self) -> &'a ItemKind {
        match self {
            Self::Active(item) | Self::Incoming(item) => &item.kind,
            Self::Upcoming(item) => &item.kind,
        }
    }

    pub fn track(self) -> Option<&'a Track> {
        match self.kind() {
            ItemKind::Track(track) => Some(track),
            ItemKind::Break => None,
        }
    }

    pub fn is_upcoming(self) -> bool {
        matches!(self, Self::Upcoming(_))
    }

    pub fn stored(self) -> StoredQueueItem {
        stored_item(self.id(), self.kind())
    }
}

pub fn stored_item(id: &str, kind: &ItemKind) -> StoredQueueItem {
    match kind {
        ItemKind::Track(track) => StoredQueueItem::Track {
            id: id.to_owned(),
            track: track.clone(),
        },
        ItemKind::Break => StoredQueueItem::Break { id: id.to_owned() },
    }
}

impl Queue {
    pub fn queued(&self) -> &[UpcomingItem] {
        match self {
            Self::Stopped { queued }
            | Self::Playing { queued, .. }
            | Self::Crossfading { queued, .. } => queued,
        }
    }

    pub fn queued_mut(&mut self) -> &mut Vec<UpcomingItem> {
        match self {
            Self::Stopped { queued }
            | Self::Playing { queued, .. }
            | Self::Crossfading { queued, .. } => queued,
        }
    }

    pub fn active(&self) -> Option<&PlayingItem> {
        match self {
            Self::Stopped { .. } => None,
            Self::Playing { active, .. } | Self::Crossfading { active, .. } => Some(active),
        }
    }

    pub fn incoming(&self) -> Option<&PlayingItem> {
        match self {
            Self::Crossfading { incoming, .. } => Some(incoming),
            _ => None,
        }
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self, Self::Stopped { .. })
    }

    pub fn is_crossfading(&self) -> bool {
        matches!(self, Self::Crossfading { .. })
    }

    /// All items in display order: Active, Incoming, then Upcoming.
    pub fn items(&self) -> impl Iterator<Item = QueueItemRef<'_>> {
        self.active()
            .map(QueueItemRef::Active)
            .into_iter()
            .chain(self.incoming().map(QueueItemRef::Incoming))
            .chain(self.queued().iter().map(QueueItemRef::Upcoming))
    }

    pub fn len(&self) -> usize {
        self.first_mutable_index() + self.queued().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn item(&self, id: &str) -> Option<QueueItemRef<'_>> {
        self.items().find(|item| item.id() == id)
    }

    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.items().position(|item| item.id() == id)
    }

    /// The first display index that commands may change.
    pub fn first_mutable_index(&self) -> usize {
        match self {
            Self::Stopped { .. } => 0,
            Self::Playing { .. } => 1,
            Self::Crossfading { .. } => 2,
        }
    }

    pub fn stored_items(&self) -> Vec<StoredQueueItem> {
        self.items().map(QueueItemRef::stored).collect()
    }

    pub fn tracks(&self) -> impl Iterator<Item = &Track> {
        self.items().filter_map(QueueItemRef::track)
    }
}

/// An edit the DJ makes to the Queue. Indices are display indices.
#[derive(Clone, Debug, PartialEq)]
pub enum QueueCommand {
    InsertTrack {
        at: usize,
        item_id: String,
        track: Track,
    },
    InsertBreak {
        at: usize,
        item_id: String,
    },
    InsertBreakAfter {
        after_id: String,
        item_id: String,
    },
    /// Removes these Upcoming items.
    Remove {
        item_ids: Vec<String>,
    },
    /// Moves these Upcoming items, together and in Queue order, to display
    /// slot `slot` (the gap just before the item currently at that index).
    Move {
        item_ids: Vec<String>,
        slot: usize,
    },
    /// Moves each of these Upcoming items one place, keeping the gaps between
    /// them. Does nothing if any of them is already at the end it moves toward.
    Step {
        item_ids: Vec<String>,
        direction: Direction,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// Applies `command`, returning the edited Queue (unchanged when the command
/// does not apply). Never touches the Active Item or Incoming Track.
pub fn edit_queue(queue: &Queue, command: QueueCommand) -> Queue {
    let offset = queue.first_mutable_index();
    let mut next = queue.clone();
    let queued = next.queued_mut();
    match command {
        QueueCommand::Remove { item_ids } => {
            queued.retain(|item| !item_ids.contains(&item.id));
        }
        QueueCommand::Move { item_ids, slot } => {
            let slot = slot.saturating_sub(offset).min(queued.len());
            // Taking the moved items out shifts the slot up by those above it.
            let above = queued[..slot]
                .iter()
                .filter(|item| item_ids.contains(&item.id))
                .count();
            let (moved, mut rest): (Vec<_>, Vec<_>) = queued
                .drain(..)
                .partition(|item| item_ids.contains(&item.id));
            let at = slot - above;
            rest.splice(at..at, moved);
            *queued = rest;
        }
        QueueCommand::Step {
            item_ids,
            direction,
        } => {
            let selected: Vec<bool> = queued
                .iter()
                .map(|item| item_ids.contains(&item.id))
                .collect();
            let blocked = match direction {
                Direction::Up => selected.first() == Some(&true),
                Direction::Down => selected.last() == Some(&true),
            };
            if blocked {
                return queue.clone();
            }
            // Each selected item swaps with its neighbour; walking from the
            // leading end moves adjacent selected items along as a block.
            match direction {
                Direction::Up => (1..selected.len())
                    .filter(|&i| selected[i])
                    .for_each(|i| queued.swap(i - 1, i)),
                Direction::Down => (0..selected.len().saturating_sub(1))
                    .rev()
                    .filter(|&i| selected[i])
                    .for_each(|i| queued.swap(i, i + 1)),
            }
        }
        QueueCommand::InsertTrack { at, item_id, track } => {
            if queue.item(&item_id).is_some() {
                return queue.clone();
            }
            let at = at.saturating_sub(offset).min(queued.len());
            queued.insert(at, UpcomingItem::track(item_id, track));
        }
        QueueCommand::InsertBreak { at, item_id } => {
            // A Break may not separate the outgoing and incoming Tracks.
            if queue.is_crossfading() || queue.item(&item_id).is_some() {
                return queue.clone();
            }
            let at = at.saturating_sub(offset).min(queued.len());
            queued.insert(at, UpcomingItem::new_break(item_id));
        }
        QueueCommand::InsertBreakAfter { after_id, item_id } => {
            if queue.is_crossfading() || queue.item(&item_id).is_some() {
                return queue.clone();
            }
            let at = if let Some(index) = queued.iter().position(|item| item.id == after_id) {
                index + 1
            } else if queue.active().is_some_and(|active| active.id == after_id) {
                0
            } else {
                return queue.clone();
            };
            queued.insert(at, UpcomingItem::new_break(item_id));
        }
    }
    next
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::SourceId;

    fn track(id: &str) -> Track {
        Track {
            id: id.into(),
            source: SourceId::Local,
            title: id.into(),
            artist: "a".into(),
            album: None,
            duration_seconds: Some(100.0),
            artwork_url: None,
            explicit: false,
        }
    }

    fn upcoming(id: &str) -> UpcomingItem {
        UpcomingItem::track(id.into(), track(id))
    }

    fn playing(id: &str) -> PlayingItem {
        PlayingItem {
            id: id.into(),
            kind: ItemKind::Track(track(id)),
            progress: PlaybackProgress::default(),
        }
    }

    fn ids(queue: &Queue) -> Vec<&str> {
        queue.items().map(QueueItemRef::id).collect()
    }

    #[test]
    fn items_are_in_display_order() {
        let queue = Queue::Crossfading {
            active: playing("a"),
            incoming: playing("b"),
            queued: vec![upcoming("c")],
        };
        assert_eq!(ids(&queue), ["a", "b", "c"]);
        assert_eq!(queue.first_mutable_index(), 2);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.index_of("c"), Some(2));
    }

    #[test]
    fn insert_track_respects_locked_positions() {
        let queue = Queue::Playing {
            active: playing("a"),
            queued: vec![upcoming("b")],
        };
        let edited = edit_queue(
            &queue,
            QueueCommand::InsertTrack {
                at: 0,
                item_id: "x".into(),
                track: track("x"),
            },
        );
        assert_eq!(ids(&edited), ["a", "x", "b"]);
        let edited = edit_queue(
            &queue,
            QueueCommand::InsertTrack {
                at: 99,
                item_id: "y".into(),
                track: track("y"),
            },
        );
        assert_eq!(ids(&edited), ["a", "b", "y"]);
    }

    #[test]
    fn duplicate_item_ids_are_rejected() {
        let queue = Queue::Stopped {
            queued: vec![upcoming("a")],
        };
        let edited = edit_queue(
            &queue,
            QueueCommand::InsertTrack {
                at: 0,
                item_id: "a".into(),
                track: track("z"),
            },
        );
        assert_eq!(edited, queue);
    }

    #[test]
    fn breaks_cannot_be_inserted_while_crossfading() {
        let queue = Queue::Crossfading {
            active: playing("a"),
            incoming: playing("b"),
            queued: vec![],
        };
        let edited = edit_queue(
            &queue,
            QueueCommand::InsertBreak {
                at: 2,
                item_id: "br".into(),
            },
        );
        assert_eq!(edited, queue);
        let edited = edit_queue(
            &queue,
            QueueCommand::InsertBreakAfter {
                after_id: "a".into(),
                item_id: "br".into(),
            },
        );
        assert_eq!(edited, queue);
        // Tracks may still be added while crossfading.
        let edited = edit_queue(
            &queue,
            QueueCommand::InsertTrack {
                at: 0,
                item_id: "c".into(),
                track: track("c"),
            },
        );
        assert_eq!(ids(&edited), ["a", "b", "c"]);
    }

    #[test]
    fn insert_break_after_active_goes_first() {
        let queue = Queue::Playing {
            active: playing("a"),
            queued: vec![upcoming("b")],
        };
        let edited = edit_queue(
            &queue,
            QueueCommand::InsertBreakAfter {
                after_id: "a".into(),
                item_id: "br".into(),
            },
        );
        assert_eq!(ids(&edited), ["a", "br", "b"]);
        let edited = edit_queue(
            &queue,
            QueueCommand::InsertBreakAfter {
                after_id: "b".into(),
                item_id: "br".into(),
            },
        );
        assert_eq!(ids(&edited), ["a", "b", "br"]);
        let unknown = edit_queue(
            &queue,
            QueueCommand::InsertBreakAfter {
                after_id: "nope".into(),
                item_id: "br".into(),
            },
        );
        assert_eq!(unknown, queue);
    }

    #[test]
    fn move_clamps_and_ignores_locked_items() {
        let queue = Queue::Playing {
            active: playing("a"),
            queued: vec![upcoming("b"), upcoming("c"), upcoming("d")],
        };
        let mv = |item_ids: &[&str], slot| QueueCommand::Move {
            item_ids: item_ids.iter().map(|id| (*id).to_owned()).collect(),
            slot,
        };
        assert_eq!(
            ids(&edit_queue(&queue, mv(&["d"], 0))),
            ["a", "d", "b", "c"]
        );
        assert_eq!(
            ids(&edit_queue(&queue, mv(&["b"], 10))),
            ["a", "c", "d", "b"]
        );
        assert_eq!(edit_queue(&queue, mv(&["a"], 3)), queue);
        // Onto its own position: unchanged.
        assert_eq!(edit_queue(&queue, mv(&["c"], 3)), queue);
    }

    #[test]
    fn moving_several_items_groups_them_in_queue_order() {
        let queue = Queue::Playing {
            active: playing("a"),
            queued: vec![
                upcoming("b"),
                upcoming("c"),
                upcoming("d"),
                upcoming("e"),
                upcoming("f"),
            ],
        };
        let mv = |item_ids: &[&str], slot| QueueCommand::Move {
            item_ids: item_ids.iter().map(|id| (*id).to_owned()).collect(),
            slot,
        };
        // Selection order doesn't matter; Queue order is kept.
        assert_eq!(
            ids(&edit_queue(&queue, mv(&["e", "b"], 6))),
            ["a", "c", "d", "f", "b", "e"]
        );
        assert_eq!(
            ids(&edit_queue(&queue, mv(&["f", "c"], 1))),
            ["a", "c", "f", "b", "d", "e"]
        );
        // Into the middle of the selection: gathered around that point.
        assert_eq!(
            ids(&edit_queue(&queue, mv(&["b", "f"], 3))),
            ["a", "c", "b", "f", "d", "e"]
        );
    }

    #[test]
    fn stepping_keeps_gaps_and_is_all_or_nothing() {
        let queue = Queue::Playing {
            active: playing("a"),
            queued: vec![
                upcoming("b"),
                upcoming("c"),
                upcoming("d"),
                upcoming("e"),
                upcoming("f"),
            ],
        };
        let step = |item_ids: &[&str], direction| QueueCommand::Step {
            item_ids: item_ids.iter().map(|id| (*id).to_owned()).collect(),
            direction,
        };
        assert_eq!(
            ids(&edit_queue(&queue, step(&["c", "e"], Direction::Up))),
            ["a", "c", "b", "e", "d", "f"]
        );
        assert_eq!(
            ids(&edit_queue(&queue, step(&["c", "d"], Direction::Up))),
            ["a", "c", "d", "b", "e", "f"]
        );
        assert_eq!(
            ids(&edit_queue(&queue, step(&["c", "d"], Direction::Down))),
            ["a", "b", "e", "c", "d", "f"]
        );
        // The first Upcoming item can't go above the Active Item, so nothing moves.
        assert_eq!(edit_queue(&queue, step(&["b", "e"], Direction::Up)), queue);
        assert_eq!(
            edit_queue(&queue, step(&["c", "f"], Direction::Down)),
            queue
        );
        assert_eq!(edit_queue(&queue, step(&["a"], Direction::Down)), queue);
    }

    #[test]
    fn remove_never_touches_active() {
        let queue = Queue::Playing {
            active: playing("a"),
            queued: vec![upcoming("b")],
        };
        assert_eq!(
            edit_queue(
                &queue,
                QueueCommand::Remove {
                    item_ids: vec!["a".into()]
                }
            ),
            queue
        );
        assert_eq!(
            ids(&edit_queue(
                &queue,
                QueueCommand::Remove {
                    item_ids: vec!["b".into(), "missing".into()]
                }
            )),
            ["a"]
        );
    }
}
