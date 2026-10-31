//! Drag and drop into the Queue: Search results, new Breaks, and reordering.

use std::sync::Arc;

use elizabeats_core::Track;
use elizabeats_core::format::format_duration;
use gpui_kit::component::{ActiveTheme, StyledExt};
use gpui_kit::{
    Context, Image, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, px,
};

use super::{RowSize, break_tile, row, tabular, track_artwork, track_details};

/// What is being dragged.
#[derive(Clone, Debug)]
pub enum DragPayload {
    SearchTrack(Track),
    NewBreak,
    /// Queue items being reordered, with their display indices, in order.
    QueueItems {
        item_ids: Vec<String>,
        indices: Vec<usize>,
    },
}

/// Where a drop at display slot `slot` should land, as a Session command.
#[derive(Clone, Debug, PartialEq)]
pub enum DropAction {
    InsertTrack { track: Track, at: usize },
    InsertBreak { at: usize },
    Move { item_ids: Vec<String>, slot: usize },
}

/// Maps a drop between rows (`slot` = index of the row it lands before) to a
/// command, or `None` when dropping there would do nothing: above the Active
/// Item or Incoming Track (they keep their places), or an item onto its own
/// position. The UI only draws an insertion line where this returns `Some`.
/// Several dragged items land together, in Queue order.
pub fn drop_action(
    payload: &DragPayload,
    slot: usize,
    first_mutable: usize,
    len: usize,
) -> Option<DropAction> {
    if slot < first_mutable || slot > len {
        return None;
    }
    match payload {
        DragPayload::SearchTrack(track) => Some(DropAction::InsertTrack {
            track: track.clone(),
            at: slot,
        }),
        DragPayload::NewBreak => Some(DropAction::InsertBreak { at: slot }),
        DragPayload::QueueItems { item_ids, indices } => {
            // Items already together, dropped just above, inside or just below
            // themselves, would stay where they are.
            let together = indices.windows(2).all(|pair| pair[1] == pair[0] + 1);
            let (first, last) = (*indices.first()?, *indices.last()?);
            let stays = together && (first..=last + 1).contains(&slot);
            (!stays).then(|| DropAction::Move {
                item_ids: item_ids.clone(),
                slot,
            })
        }
    }
}

/// The floating row under the pointer while dragging: looks like the row
/// being dragged (artwork, title, artist, duration) or a Break, or says how
/// many items are being dragged.
pub struct DragPreview {
    track: Option<Track>,
    image: Option<Arc<Image>>,
    count: usize,
}

impl DragPreview {
    pub fn track(track: Track, image: Option<Arc<Image>>) -> Self {
        Self {
            track: Some(track),
            image,
            count: 1,
        }
    }

    pub fn new_break() -> Self {
        Self {
            track: None,
            image: None,
            count: 1,
        }
    }

    /// Several Queue items, shown as "3 items" with the first one's artwork.
    pub fn items(count: usize, track: Option<Track>, image: Option<Arc<Image>>) -> Self {
        Self {
            track,
            image,
            count,
        }
    }
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row = row(RowSize::Regular)
            .w(px(360.))
            .rounded(px(8.))
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .shadow_lg();
        if self.count > 1 {
            let media = match &self.track {
                Some(_) => track_artwork(self.image.clone(), RowSize::Regular.media(), cx)
                    .into_any_element(),
                None => break_tile(RowSize::Regular.media(), cx).into_any_element(),
            };
            return row.child(media).child(
                div()
                    .flex_1()
                    .text_sm()
                    .font_medium()
                    .child(SharedString::from(format!("{} items", self.count))),
            );
        }
        match &self.track {
            Some(track) => row
                .child(track_artwork(
                    self.image.clone(),
                    RowSize::Regular.media(),
                    cx,
                ))
                .child(track_details(track, Vec::new(), cx))
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .font_features(tabular())
                        .text_color(cx.theme().muted_foreground)
                        .child(SharedString::from(format_duration(track.duration_seconds))),
                ),
            None => row
                .child(break_tile(RowSize::Regular.media(), cx))
                .child(div().flex_1().text_sm().font_medium().child("Break")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use elizabeats_core::SourceId;

    fn track() -> Track {
        Track {
            id: "t".into(),
            source: SourceId::Local,
            title: "t".into(),
            artist: "a".into(),
            album: None,
            duration_seconds: None,
            artwork_url: None,
            explicit: false,
        }
    }

    #[test]
    fn nothing_can_be_dropped_above_the_active_item() {
        // Slot 0 is above the Active Item; slot 1 is right below it.
        assert_eq!(drop_action(&DragPayload::NewBreak, 0, 1, 3), None);
        assert_eq!(
            drop_action(&DragPayload::NewBreak, 1, 1, 3),
            Some(DropAction::InsertBreak { at: 1 })
        );
        // While crossfading, the Incoming Track is locked too.
        let moving = items(&[2]);
        assert_eq!(drop_action(&moving, 1, 2, 4), None);
        assert_eq!(
            drop_action(&DragPayload::SearchTrack(track()), 9, 0, 3),
            None
        );
        assert_eq!(
            drop_action(&DragPayload::SearchTrack(track()), 3, 0, 3),
            Some(DropAction::InsertTrack {
                track: track(),
                at: 3
            })
        );
    }

    fn items(indices: &[usize]) -> DragPayload {
        DragPayload::QueueItems {
            item_ids: indices.iter().map(|index| format!("item{index}")).collect(),
            indices: indices.to_vec(),
        }
    }

    fn moved(indices: &[usize], slot: usize) -> Option<DropAction> {
        Some(DropAction::Move {
            item_ids: indices.iter().map(|index| format!("item{index}")).collect(),
            slot,
        })
    }

    #[test]
    fn dropping_items_onto_themselves_does_nothing() {
        // One item: just above or just below itself is "stay put".
        assert_eq!(drop_action(&items(&[1]), 1, 0, 4), None);
        assert_eq!(drop_action(&items(&[1]), 2, 0, 4), None);
        assert_eq!(drop_action(&items(&[1]), 4, 0, 4), moved(&[1], 4));
        assert_eq!(drop_action(&items(&[1]), 0, 0, 4), moved(&[1], 0));
        // A block of items: anywhere from its top to just below it.
        assert_eq!(drop_action(&items(&[1, 2]), 2, 0, 4), None);
        assert_eq!(drop_action(&items(&[1, 2]), 3, 0, 4), None);
        assert_eq!(drop_action(&items(&[1, 2]), 4, 0, 4), moved(&[1, 2], 4));
        // Scattered items gather, even dropped among themselves.
        assert_eq!(drop_action(&items(&[0, 2]), 1, 0, 4), moved(&[0, 2], 1));
    }
}
