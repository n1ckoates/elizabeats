//! Which Queue items are selected, and how clicks and keys change that.
//!
//! Only Upcoming items can be selected. Callers pass `order`: the IDs of the
//! selectable items in display order, so ranges and "select all" never reach
//! the Active Item or the Incoming Track.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    ids: Vec<String>,
    /// Where Shift-click and Shift-arrow ranges start: the last item clicked
    /// without Shift.
    anchor: Option<String>,
}

impl Selection {
    pub fn contains(&self, id: &str) -> bool {
        self.ids.iter().any(|selected| selected == id)
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    pub fn clear(&mut self) {
        self.ids.clear();
        self.anchor = None;
    }

    /// Plain click: just this item.
    pub fn select_only(&mut self, id: &str) {
        self.ids = vec![id.to_owned()];
        self.anchor = Some(id.to_owned());
    }

    /// Cmd-click: add or remove this item.
    pub fn toggle(&mut self, id: &str) {
        if self.contains(id) {
            self.ids.retain(|selected| selected != id);
        } else {
            self.ids.push(id.to_owned());
        }
        self.anchor = Some(id.to_owned());
    }

    /// Shift-click or Shift-arrow: everything from the anchor to `id`,
    /// replacing the rest of the selection. Without an anchor, just `id`.
    pub fn select_range(&mut self, order: &[String], id: &str) {
        let anchor = self
            .anchor
            .as_deref()
            .and_then(|anchor| position(order, anchor));
        let (Some(from), Some(to)) = (anchor, position(order, id)) else {
            self.select_only(id);
            return;
        };
        self.ids = order[from.min(to)..=from.max(to)].to_vec();
    }

    pub fn select_all(&mut self, order: &[String]) {
        self.ids = order.to_vec();
        self.anchor = order.first().cloned();
    }

    /// Drops items that can no longer be selected (removed, or now Playing).
    pub fn retain(&mut self, order: &[String]) {
        self.ids.retain(|id| order.contains(id));
        if self
            .anchor
            .as_ref()
            .is_some_and(|anchor| !order.contains(anchor))
        {
            self.anchor = None;
        }
    }

    /// The selected IDs in display order.
    pub fn ordered(&self, order: &[String]) -> Vec<String> {
        order
            .iter()
            .filter(|id| self.contains(id))
            .cloned()
            .collect()
    }
}

fn position(order: &[String], id: &str) -> Option<usize> {
    order.iter().position(|item| item == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order() -> Vec<String> {
        ["b", "c", "d", "e", "f"].map(String::from).to_vec()
    }

    #[test]
    fn shift_selects_a_range_from_the_last_plain_or_cmd_click() {
        let mut selection = Selection::default();
        selection.select_only("c");
        selection.select_range(&order(), "e");
        assert_eq!(selection.ordered(&order()), ["c", "d", "e"]);
        // A new range from the same anchor replaces the old one.
        selection.select_range(&order(), "b");
        assert_eq!(selection.ordered(&order()), ["b", "c"]);
    }

    #[test]
    fn cmd_click_toggles_and_shift_then_replaces_scattered_picks() {
        let mut selection = Selection::default();
        selection.select_only("b");
        selection.toggle("d");
        selection.toggle("f");
        assert_eq!(selection.ordered(&order()), ["b", "d", "f"]);
        selection.toggle("d");
        assert_eq!(selection.ordered(&order()), ["b", "f"]);
        // Like Finder: the range starts at the last Cmd-clicked item ("d").
        selection.select_range(&order(), "e");
        assert_eq!(selection.ordered(&order()), ["d", "e"]);
    }

    #[test]
    fn items_that_stop_being_selectable_drop_out() {
        let mut selection = Selection::default();
        selection.select_all(&order());
        assert_eq!(selection.ordered(&order()).len(), 5);
        // "b" started Playing; "f" was removed.
        let now = ["c", "d", "e"].map(String::from).to_vec();
        selection.retain(&now);
        assert_eq!(selection.ordered(&now), ["c", "d", "e"]);
        // The anchor ("b") went too, so Shift-click starts fresh.
        selection.select_range(&now, "d");
        assert_eq!(selection.ordered(&now), ["d"]);
    }
}
