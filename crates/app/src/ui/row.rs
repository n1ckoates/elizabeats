//! The one row layout shared by every Track and Break list: the Queue, Search
//! results, the Play Log, the drag preview and the loading skeleton.
//!
//! Rows differ in size only when they are the Active Item. Everything else
//! (height, side padding, spacing, artwork size) comes from here, so the lists
//! can't drift apart.

use gpui_kit::component::h_flex;
use gpui_kit::{Div, Styled, px};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowSize {
    /// Every row except the one Playing as the Active Item.
    Regular,
    /// The Active Item's row: taller, with bigger artwork.
    Active,
}

impl RowSize {
    /// Total row height in pixels, including its 1 px bottom divider.
    pub const fn height(self) -> f32 {
        match self {
            Self::Regular => 57.,
            Self::Active => 77.,
        }
    }

    /// Artwork (or Break tile) width and height in pixels.
    pub const fn media(self) -> f32 {
        match self {
            Self::Regular => 36.,
            Self::Active => 44.,
        }
    }

    const fn gap(self) -> f32 {
        match self {
            Self::Regular => 10.,
            Self::Active => 12.,
        }
    }
}

/// A row's frame: fixed height, side padding and spacing, content centered
/// vertically. Callers add the divider (`border_b_1`), background and content.
pub fn row(size: RowSize) -> Div {
    h_flex()
        .flex_none()
        .w_full()
        .h(px(size.height()))
        .px_5()
        .gap(px(size.gap()))
}
