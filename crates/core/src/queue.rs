//! The Queue: DJ-authored Tracks and Breaks, plus the pure editing rules.
//!
//! Edits change Upcoming Items only. The Active Item and an Incoming Track
//! keep their roles (and positions) until playback moves them.

use crate::domain::{ItemKind, PlaybackProgress, QueueItem, Track};

/// Identifies one file loaded into the audio engine.
pub type LoadId = u64;

/// A Playing item (the Active Item or the Incoming Track).
#[derive(Clone, Debug, PartialEq)]
pub struct PlayingItem {
    pub item: QueueItem,
    pub progress: PlaybackProgress,
    /// The engine load playing it; `None` for a Break.
    pub load: Option<LoadId>,
}

impl PlayingItem {
    pub fn new(item: QueueItem, load: Option<LoadId>) -> Self {
        let duration = item
            .as_track()
            .and_then(|track| track.duration_seconds)
            .unwrap_or(0.0);
        Self {
            item,
            progress: PlaybackProgress {
                current_time: 0.0,
                duration,
            },
            load,
        }
    }
}

/// What is Playing, and any start the Session is waiting on.
#[derive(Clone, Debug, PartialEq)]
pub enum Playback {
    Stopped {
        /// An Upcoming item that starts once its Track is prepared.
        starting: Option<String>,
    },
    Playing {
        active: PlayingItem,
        /// An Upcoming Track to crossfade into once it is prepared (a manual
        /// skip or "Play now").
        crossfade_to: Option<String>,
        /// The automatic crossfade near the end has already been attempted.
        auto_crossfade_tried: bool,
    },
    /// Both outgoing and incoming are Tracks.
    Crossfading {
        active: PlayingItem,
        incoming: PlayingItem,
        /// Started by the DJ: the outgoing Track leaves as soon as the fade completes.
        manual: bool,
    },
}

impl Default for Playback {
    fn default() -> Self {
        Self::Stopped { starting: None }
    }
}

impl Playback {
    pub fn playing(active: PlayingItem) -> Self {
        Self::Playing {
            active,
            crossfade_to: None,
            auto_crossfade_tried: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Queue {
    pub playback: Playback,
    /// Items that have not begun Playing, in order.
    pub upcoming: Vec<QueueItem>,
}

/// A borrowed view of any Queue item, in display order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum QueueItemRef<'a> {
    Active(&'a PlayingItem),
    Incoming(&'a PlayingItem),
    Upcoming(&'a QueueItem),
}

impl<'a> QueueItemRef<'a> {
    pub fn item(self) -> &'a QueueItem {
        match self {
            Self::Active(playing) | Self::Incoming(playing) => &playing.item,
            Self::Upcoming(item) => item,
        }
    }

    pub fn id(self) -> &'a str {
        &self.item().id
    }

    pub fn kind(self) -> &'a ItemKind {
        &self.item().kind
    }

    pub fn track(self) -> Option<&'a Track> {
        self.item().as_track()
    }

    pub fn progress(self) -> Option<PlaybackProgress> {
        match self {
            Self::Active(playing) | Self::Incoming(playing) => Some(playing.progress),
            Self::Upcoming(_) => None,
        }
    }
}

/// Which way [`Queue::step`] moves items.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

impl Queue {
    pub fn stopped(upcoming: Vec<QueueItem>) -> Self {
        Self {
            playback: Playback::default(),
            upcoming,
        }
    }

    pub fn active(&self) -> Option<&PlayingItem> {
        match &self.playback {
            Playback::Stopped { .. } => None,
            Playback::Playing { active, .. } | Playback::Crossfading { active, .. } => Some(active),
        }
    }

    pub fn incoming(&self) -> Option<&PlayingItem> {
        match &self.playback {
            Playback::Crossfading { incoming, .. } => Some(incoming),
            _ => None,
        }
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self.playback, Playback::Stopped { .. })
    }

    pub fn is_crossfading(&self) -> bool {
        matches!(self.playback, Playback::Crossfading { .. })
    }

    /// All items in display order: Active, Incoming, then Upcoming.
    pub fn items(&self) -> impl Iterator<Item = QueueItemRef<'_>> {
        self.active()
            .map(QueueItemRef::Active)
            .into_iter()
            .chain(self.incoming().map(QueueItemRef::Incoming))
            .chain(self.upcoming.iter().map(QueueItemRef::Upcoming))
    }

    pub fn len(&self) -> usize {
        self.first_mutable_index() + self.upcoming.len()
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

    /// The first display index that edits may change.
    pub fn first_mutable_index(&self) -> usize {
        match self.playback {
            Playback::Stopped { .. } => 0,
            Playback::Playing { .. } => 1,
            Playback::Crossfading { .. } => 2,
        }
    }

    pub fn tracks(&self) -> impl Iterator<Item = &Track> {
        self.items().filter_map(QueueItemRef::track)
    }

    /// Every item, in display order, as it is saved.
    pub fn stored_items(&self) -> Vec<QueueItem> {
        self.items().map(|item| item.item().clone()).collect()
    }

    // ---- Edits (display indices; Upcoming items only) ------------------------

    /// Inserts at display index `at` (clamped). Ignores a duplicate id, and a
    /// Break while crossfading: it may not separate the outgoing and incoming Tracks.
    pub fn insert(&mut self, at: usize, item: QueueItem) {
        let is_break = item.kind == ItemKind::Break;
        if self.item(&item.id).is_some() || (is_break && self.is_crossfading()) {
            return;
        }
        let at = self.upcoming_index(at);
        self.upcoming.insert(at, item);
    }

    pub fn remove(&mut self, ids: &[String]) {
        self.upcoming.retain(|item| !ids.contains(&item.id));
    }

    /// Moves these items, together and in Queue order, to display slot `slot`
    /// (the gap just before the item currently at that index).
    pub fn move_to(&mut self, ids: &[String], slot: usize) {
        let slot = self.upcoming_index(slot);
        // Taking the moved items out shifts the slot up by those above it.
        let above = self.upcoming[..slot]
            .iter()
            .filter(|item| ids.contains(&item.id))
            .count();
        let (moved, mut rest): (Vec<_>, Vec<_>) = self
            .upcoming
            .drain(..)
            .partition(|item| ids.contains(&item.id));
        let at = slot - above;
        rest.splice(at..at, moved);
        self.upcoming = rest;
    }

    /// Moves each of these items one place, keeping the gaps between them.
    /// Does nothing if any of them is already at the end it moves toward.
    pub fn step(&mut self, ids: &[String], direction: Direction) {
        let len = self.upcoming.len();
        let positions: Vec<usize> = (0..len)
            .filter(|&i| ids.contains(&self.upcoming[i].id))
            .collect();
        // Each selected item swaps with its neighbour, starting from the end
        // it moves toward, so adjacent selected items move along as a block.
        match direction {
            Direction::Up => {
                if positions.first() == Some(&0) {
                    return;
                }
                for i in positions {
                    self.upcoming.swap(i - 1, i);
                }
            }
            Direction::Down => {
                if positions.last().is_some_and(|&i| i + 1 == len) {
                    return;
                }
                for i in positions.into_iter().rev() {
                    self.upcoming.swap(i, i + 1);
                }
            }
        }
    }

    fn upcoming_index(&self, display_index: usize) -> usize {
        display_index
            .saturating_sub(self.first_mutable_index())
            .min(self.upcoming.len())
    }
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

    fn upcoming(id: &str) -> QueueItem {
        QueueItem::track(id, track(id))
    }

    fn playing(id: &str) -> PlayingItem {
        PlayingItem::new(upcoming(id), Some(1))
    }

    fn queue(active: &[&str], upcoming_ids: &[&str]) -> Queue {
        let mut playing = active.iter().map(|id| playing(id));
        let playback = match (playing.next(), playing.next()) {
            (None, _) => Playback::default(),
            (Some(active), None) => Playback::playing(active),
            (Some(active), Some(incoming)) => Playback::Crossfading {
                active,
                incoming,
                manual: false,
            },
        };
        Queue {
            playback,
            upcoming: upcoming_ids.iter().map(|id| upcoming(id)).collect(),
        }
    }

    fn ids(queue: &Queue) -> Vec<&str> {
        queue.items().map(QueueItemRef::id).collect()
    }

    fn strings(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    fn edited(queue: &Queue, edit: impl FnOnce(&mut Queue)) -> Queue {
        let mut queue = queue.clone();
        edit(&mut queue);
        queue
    }

    #[test]
    fn items_are_in_display_order() {
        let queue = queue(&["a", "b"], &["c"]);
        assert_eq!(ids(&queue), ["a", "b", "c"]);
        assert_eq!(queue.first_mutable_index(), 2);
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.index_of("c"), Some(2));
    }

    #[test]
    fn insert_respects_locked_positions_and_ids() {
        let q = queue(&["a"], &["b"]);
        assert_eq!(
            ids(&edited(&q, |q| q.insert(0, upcoming("x")))),
            ["a", "x", "b"]
        );
        assert_eq!(
            ids(&edited(&q, |q| q.insert(99, upcoming("y")))),
            ["a", "b", "y"]
        );
        // Duplicate ids are rejected.
        assert_eq!(
            edited(&q, |q| q.insert(0, QueueItem::track("b", track("z")))),
            q
        );
    }

    #[test]
    fn breaks_cannot_be_inserted_while_crossfading() {
        let q = queue(&["a", "b"], &[]);
        assert_eq!(edited(&q, |q| q.insert(2, QueueItem::new_break("br"))), q);
        // Tracks may still be added while crossfading.
        assert_eq!(
            ids(&edited(&q, |q| q.insert(0, upcoming("c")))),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn move_clamps_and_ignores_locked_items() {
        let q = queue(&["a"], &["b", "c", "d"]);
        let mv = |ids: &[&str], slot| edited(&q, |q| q.move_to(&strings(ids), slot));
        assert_eq!(ids(&mv(&["d"], 0)), ["a", "d", "b", "c"]);
        assert_eq!(ids(&mv(&["b"], 10)), ["a", "c", "d", "b"]);
        assert_eq!(mv(&["a"], 3), q);
        // Onto its own position: unchanged.
        assert_eq!(mv(&["c"], 3), q);
    }

    #[test]
    fn moving_several_items_groups_them_in_queue_order() {
        let q = queue(&["a"], &["b", "c", "d", "e", "f"]);
        let mv = |ids: &[&str], slot| edited(&q, |q| q.move_to(&strings(ids), slot));
        // Selection order doesn't matter; Queue order is kept.
        assert_eq!(ids(&mv(&["e", "b"], 6)), ["a", "c", "d", "f", "b", "e"]);
        assert_eq!(ids(&mv(&["f", "c"], 1)), ["a", "c", "f", "b", "d", "e"]);
        // Into the middle of the selection: gathered around that point.
        assert_eq!(ids(&mv(&["b", "f"], 3)), ["a", "c", "b", "f", "d", "e"]);
    }

    #[test]
    fn stepping_keeps_gaps_and_is_all_or_nothing() {
        let q = queue(&["a"], &["b", "c", "d", "e", "f"]);
        let step = |ids: &[&str], direction| edited(&q, |q| q.step(&strings(ids), direction));
        assert_eq!(
            ids(&step(&["c", "e"], Direction::Up)),
            ["a", "c", "b", "e", "d", "f"]
        );
        assert_eq!(
            ids(&step(&["c", "d"], Direction::Up)),
            ["a", "c", "d", "b", "e", "f"]
        );
        assert_eq!(
            ids(&step(&["c", "d"], Direction::Down)),
            ["a", "b", "e", "c", "d", "f"]
        );
        // The first Upcoming item can't go above the Active Item, so nothing moves.
        assert_eq!(step(&["b", "e"], Direction::Up), q);
        assert_eq!(step(&["c", "f"], Direction::Down), q);
        assert_eq!(step(&["a"], Direction::Down), q);
    }

    #[test]
    fn remove_never_touches_active() {
        let q = queue(&["a"], &["b"]);
        assert_eq!(edited(&q, |q| q.remove(&strings(&["a"]))), q);
        assert_eq!(
            ids(&edited(&q, |q| q.remove(&strings(&["b", "missing"])))),
            ["a"]
        );
    }
}
