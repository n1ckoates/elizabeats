//! The Queue panel: Active Item pinned on top, Upcoming items below.

use elizabeats_core::format::format_duration;
use elizabeats_core::queue::{Direction, ItemKind, QueueItemRef, Readiness};
use elizabeats_core::{SourceId, Track};
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IconName, Sizable, StyledExt, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext, ClickEvent, ClipboardItem, Context, DragMoveEvent, Entity,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px, relative,
};

use gpui_kit::base::TestSupportExt;

use super::drag::{DragPayload, DragPreview, DropAction, drop_action};
use super::menus::{MenuAction, build_menu};
use super::{
    AppView, ClearSelection, ExtendSelectionDown, ExtendSelectionUp, MoveSelectedDown,
    MoveSelectedUp, QUEUE_ROW_CONTEXT, RemoveSelected, SelectAll, SelectNext, SelectPrevious,
    alert_icon, break_tile, file_manager_name, section_label, tabular, track_artwork,
    track_details,
};
use crate::station::Station;

const ROW_GROUP: &str = "queue-row";

/// A snapshot of one Queue row, so rendering doesn't hold a borrow of the Station.
#[derive(Clone)]
struct RowModel {
    index: usize,
    id: String,
    kind: ItemKind,
    role: Role,
    readiness: Readiness,
    progress: (f64, f64),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Active,
    Incoming,
    Upcoming,
}

impl RowModel {
    /// `played_since_report` moves Playing items' positions on smoothly
    /// between the engine's progress reports.
    fn from_item(index: usize, item: QueueItemRef<'_>, played_since_report: f64) -> Self {
        let playing_progress = |progress: elizabeats_core::PlaybackProgress| {
            (
                progress.current_time + played_since_report,
                progress.duration,
            )
        };
        let (role, readiness, progress) = match item {
            QueueItemRef::Active(playing) => (
                Role::Active,
                Readiness::Ready,
                playing_progress(playing.progress),
            ),
            QueueItemRef::Incoming(playing) => (
                Role::Incoming,
                Readiness::Ready,
                playing_progress(playing.progress),
            ),
            QueueItemRef::Upcoming(upcoming) => (Role::Upcoming, upcoming.readiness, (0.0, 0.0)),
        };
        Self {
            index,
            id: item.id().to_owned(),
            kind: item.kind().clone(),
            role,
            readiness,
            progress,
        }
    }

    fn size(&self) -> super::RowSize {
        if self.role == Role::Active {
            super::RowSize::Active
        } else {
            super::RowSize::Regular
        }
    }

    fn locked(&self) -> bool {
        self.role != Role::Upcoming
    }

    fn label(&self) -> String {
        match &self.kind {
            ItemKind::Track(track) => track.title.clone(),
            ItemKind::Break => "Break".to_owned(),
        }
    }
}

fn station_command(
    station: &Entity<Station>,
    cx: &mut App,
    run: impl FnOnce(&mut elizabeats_core::session::Session) -> Vec<elizabeats_core::session::Effect>,
) {
    station.update(cx, |station, cx| station.command(cx, run));
}

impl AppView {
    pub(super) fn render_queue_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        if !cx.has_active_drag() {
            self.drop_slot = None;
        }
        let station = self.station.read(cx);
        let session = station.session();
        let queue = session.queue();
        let played_since_report = station.progress_extrapolation();
        let rows: Vec<RowModel> = queue
            .items()
            .enumerate()
            .map(|(index, item)| RowModel::from_item(index, item, played_since_report))
            .collect();
        let crossfading = queue.is_crossfading();
        let error = session
            .playback_error()
            .map(|message| SharedString::from(message.to_owned()));
        let count = rows.len();

        let mut list = v_flex().size_full();
        let body: AnyElement = if rows.is_empty() {
            self.render_empty_queue(cx).into_any_element()
        } else {
            let pinned = rows.first().filter(|row| row.role == Role::Active).cloned();
            let scrolled = self.queue_scroll.offset().y < px(0.);
            let rest: Vec<RowModel> = rows
                .iter()
                .filter(|row| row.role != Role::Active)
                .cloned()
                .collect();
            if let Some(active) = pinned {
                list = list.child(
                    div()
                        .flex_none()
                        .when(scrolled, |el| el.shadow_md())
                        .child(self.render_row(&active, window, cx)),
                );
            }
            let rows_el: Vec<AnyElement> = rest
                .iter()
                .map(|row| self.render_row(row, window, cx))
                .collect();
            list.child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        v_flex()
                            .id("queue-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.queue_scroll)
                            .children(rows_el)
                            .child(self.render_queue_end(count, cx)),
                    )
                    .child(gpui_kit::component::scroll::Scrollbar::vertical(
                        &self.queue_scroll,
                    )),
            )
            .into_any_element()
        };

        v_flex()
            .size_full()
            .min_w_0()
            .on_action(cx.listener(Self::on_remove_selected))
            .on_action(cx.listener(Self::on_select_previous))
            .on_action(cx.listener(Self::on_select_next))
            .on_action(cx.listener(Self::on_extend_selection_up))
            .on_action(cx.listener(Self::on_extend_selection_down))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_clear_selection))
            .on_action(cx.listener(Self::on_move_selected_up))
            .on_action(cx.listener(Self::on_move_selected_down))
            // Clicking anywhere outside the Queue panel deselects.
            .on_mouse_down_out(cx.listener(|this, _: &MouseDownEvent, _, cx| {
                if !this.selection.is_empty() {
                    this.selection.clear();
                    cx.notify();
                }
            }))
            .children(error.map(|message| Alert::error("playback-error", message).banner()))
            .child(
                h_flex()
                    .flex_none()
                    .h(px(super::PANEL_HEADER_HEIGHT))
                    .px_5()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    // Same background as the Search/Log tab bar beside it.
                    .bg(cx.theme().tab_bar)
                    .child(
                        h_flex().gap_2().child(section_label("Queue", cx)).child(
                            div()
                                .text_xs()
                                .font_features(tabular())
                                .text_color(cx.theme().muted_foreground)
                                .child(SharedString::from(count.to_string())),
                        ),
                    )
                    .child(self.render_add_break(crossfading, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .bg(cx.theme().background)
                    // Each drop zone only marks itself while the cursor is over
                    // it; once the cursor leaves the Queue, nothing is targeted.
                    .on_drag_move(
                        cx.listener(|this, event: &DragMoveEvent<DragPayload>, _, cx| {
                            if !event.bounds.contains(&event.event.position)
                                && this.drop_slot.take().is_some()
                            {
                                cx.notify();
                            }
                        }),
                    )
                    .child(body),
            )
    }

    fn render_add_break(&self, crossfading: bool, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("add-break-drag")
            .test_support()
            .when(!crossfading, |el| {
                el.on_drag(DragPayload::NewBreak, |_, _, _, cx| {
                    cx.new(|_| DragPreview::new_break())
                })
            })
            .child(
                Button::new("add-break")
                    .outline()
                    .small()
                    .icon(Lucide::MicVocal)
                    .label("Add Break")
                    .tooltip("Drag to queue")
                    .disabled(crossfading)
                    .on_click(cx.listener(|this, _, _, cx| {
                        // Above the topmost selected item.
                        let selectable = this.selectable_ids(cx);
                        let selected = this.selection.ordered(&selectable).into_iter().next();
                        station_command(&this.station, cx, |session| {
                            session.insert_break(selected.as_deref())
                        });
                    })),
            )
    }

    fn render_empty_queue(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let targeted = cx.has_active_drag() && self.drop_slot == Some(0);
        let accent = cx.theme().success;
        div()
            .id("queue-empty")
            .test_support()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<DragPayload>, _, cx| {
                    if event.bounds.contains(&event.event.position) {
                        let payload = event.drag(cx).clone();
                        this.hover_drop_slot(&payload, 0, cx);
                    }
                }),
            )
            .on_drop(cx.listener(|this, payload: &DragPayload, window, cx| {
                this.drop_into_queue(payload, Some(0), window, cx)
            }))
            .when(targeted, |el| el.bg(accent.opacity(0.05)))
            .child(
                Empty::new().header(
                    EmptyHeader::new()
                        .media(
                            EmptyMedia::new()
                                .with_variant(EmptyMediaVariant::Icon)
                                .child(
                                    Icon::new(Lucide::ListMusic)
                                        .when(targeted, |icon| icon.text_color(accent)),
                                ),
                        )
                        .title(EmptyTitle::new().child(if targeted {
                            "Drop here"
                        } else {
                            "Your queue is empty"
                        }))
                        .when(!targeted, |header| {
                            header.description(
                                EmptyDescription::new()
                                    .child("Drag tracks or breaks here to start."),
                            )
                        }),
                ),
            )
    }

    fn render_queue_end(&mut self, count: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let targeted = cx.has_active_drag() && self.drop_slot == Some(count);
        let accent = cx.theme().success;
        div()
            .id("queue-end")
            .relative()
            .flex_1()
            .min_h(px(48.))
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DragPayload>, _, cx| {
                    if event.bounds.contains(&event.event.position) {
                        let payload = event.drag(cx).clone();
                        this.hover_drop_slot(&payload, count, cx);
                    }
                }),
            )
            .on_drop(cx.listener(move |this, payload: &DragPayload, window, cx| {
                this.drop_into_queue(payload, Some(count), window, cx);
            }))
            // Clicking the empty space below the rows deselects.
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.selection.clear();
                cx.notify();
            }))
            .when(targeted, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_3()
                        .right_3()
                        .h(px(2.))
                        .rounded_full()
                        .bg(accent),
                )
            })
    }

    /// Marks `slot` as the drop target if dropping there would change the
    /// Queue; otherwise clears the target so no insertion line is drawn.
    fn hover_drop_slot(&mut self, payload: &DragPayload, slot: usize, cx: &mut Context<Self>) {
        let (first_mutable, len) = {
            let queue = self.station.read(cx).session().queue();
            (queue.first_mutable_index(), queue.len())
        };
        let target = drop_action(payload, slot, first_mutable, len).map(|_| slot);
        if self.drop_slot != target {
            self.drop_slot = target;
            cx.notify();
        }
    }

    /// `fallback_slot` is used by the empty Queue and the end zone, whose slot
    /// is unambiguous; rows only drop where a valid target was highlighted.
    fn drop_into_queue(
        &mut self,
        payload: &DragPayload,
        fallback_slot: Option<usize>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(slot) = self.drop_slot.take().or(fallback_slot) else {
            cx.notify();
            return;
        };
        let (first_mutable, len) = {
            let queue = self.station.read(cx).session().queue();
            (queue.first_mutable_index(), queue.len())
        };
        let Some(action) = drop_action(payload, slot, first_mutable, len) else {
            cx.notify();
            return;
        };
        station_command(&self.station, cx, move |session| match action {
            DropAction::InsertTrack { track, at } => session.insert_track_at(track, at, None),
            DropAction::InsertBreak { at } => session.insert_break_at(at),
            DropAction::Move { item_ids, slot } => session.move_items(&item_ids, slot),
        });
    }

    /// The items a row's menu or drag acts on: the whole selection when the
    /// row is part of it, otherwise just the row. In display order.
    fn row_targets(&self, row: &RowModel, cx: &App) -> Vec<String> {
        if self.selection.contains(&row.id) {
            self.selection.ordered(&self.selectable_ids(cx))
        } else {
            vec![row.id.clone()]
        }
    }

    fn row_actions(&self, row: &RowModel, cx: &App) -> Vec<MenuAction> {
        let targets = self.row_targets(row, cx);
        if targets.len() > 1 {
            return self.selection_actions(targets, cx);
        }
        let station = self.station.clone();
        let id = row.id.clone();
        let preparing = row.readiness == Readiness::Preparing;
        let mut actions = vec![
            {
                let (station, id) = (station.clone(), id.clone());
                MenuAction::new("Play now", IconName::Play, move |_, cx| {
                    station_command(&station, cx, |session| session.play_item(&id));
                })
                .disabled(row.locked() || preparing)
            },
            {
                let (station, id) = (station.clone(), id.clone());
                MenuAction::new("Remove from queue", Lucide::Trash, move |_, cx| {
                    station_command(&station, cx, |session| {
                        session.remove_items(std::slice::from_ref(&id))
                    });
                })
                .disabled(row.locked())
            },
            {
                let (station, id) = (station.clone(), id.clone());
                MenuAction::new("Insert break after", IconName::Pause, move |_, cx| {
                    station_command(&station, cx, |session| session.insert_break_after(&id));
                })
                .disabled(self.station.read(cx).session().queue().is_crossfading())
            },
        ];
        if let ItemKind::Track(track) = &row.kind {
            actions.push(track_link_action(track, &self.station, cx));
        }
        actions
    }

    /// Menu for several selected items (`ids`, in display order).
    fn selection_actions(&self, ids: Vec<String>, cx: &App) -> Vec<MenuAction> {
        let queue = self.station.read(cx).session().queue();
        let links: Vec<String> = ids
            .iter()
            .filter_map(|id| queue.item(id)?.track()?.youtube_music_url())
            .collect();
        let last = ids.last().cloned().unwrap_or_default();
        let crossfading = queue.is_crossfading();
        let station = self.station.clone();
        let mut actions = vec![
            MenuAction::new("Play now", IconName::Play, |_, _| {}).disabled(true),
            {
                let (station, label) = (station.clone(), format!("Remove {} items", ids.len()));
                MenuAction::new(label, Lucide::Trash, move |_, cx| {
                    station_command(&station, cx, |session| session.remove_items(&ids));
                })
            },
            MenuAction::new("Insert break after", IconName::Pause, move |_, cx| {
                station_command(&station, cx, |session| session.insert_break_after(&last));
            })
            .disabled(crossfading),
        ];
        if !links.is_empty() {
            let label = if links.len() == 1 {
                "Copy YouTube link"
            } else {
                "Copy YouTube links"
            };
            actions.push(MenuAction::new(label, IconName::Copy, move |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(links.join("\n")));
            }));
        }
        actions
    }

    fn render_row(
        &mut self,
        row: &RowModel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focus = self
            .row_focus
            .entry(row.id.clone())
            .or_insert_with(|| cx.focus_handle())
            .clone();
        let index = row.index;
        let targeted = cx.has_active_drag() && self.drop_slot == Some(index);
        let accent = cx.theme().success;
        let actions = self.row_actions(row, cx);
        let context_actions = actions.clone();
        let menu_actions = actions;
        let label = row.label();
        let group = SharedString::from(format!("{ROW_GROUP}-{}", row.id));

        let menu_id = SharedString::from(format!("menu-{}", row.id));
        let menu_button = move || -> AnyElement {
            let actions = menu_actions.clone();
            Button::new(menu_id.clone())
                .ghost()
                .xsmall()
                .icon(IconName::EllipsisVertical)
                .tooltip(format!("More actions for {label}"))
                .dropdown_menu(move |menu, _, _| build_menu(menu, &actions, Some(3)))
                .into_any_element()
        };

        let station = self.station.clone();
        let skip = Button::new(SharedString::from(format!("skip-{}", row.id)))
            .ghost()
            .xsmall()
            .icon(Lucide::SkipForward)
            .tooltip("Skip")
            .on_click(move |_, _, cx| station_command(&station, cx, |session| session.skip()))
            .into_any_element();

        let content: AnyElement = match &row.kind {
            ItemKind::Track(track) => self
                .render_track_row(row, track, &group, &menu_button, skip, cx)
                .into_any_element(),
            ItemKind::Break => self
                .render_break_row(row, &group, &menu_button, skip, cx)
                .into_any_element(),
        };

        let preview_track = match &row.kind {
            ItemKind::Track(track) => Some(track.clone()),
            ItemKind::Break => None,
        };
        let preview_image = preview_track.as_ref().and_then(|track| {
            self.artwork
                .update(cx, |artwork, cx| artwork.get(track, cx))
        });
        let targets = self.row_targets(row, cx);
        let drag_count = targets.len();
        let drag_payload = {
            let queue = self.station.read(cx).session().queue();
            DragPayload::QueueItems {
                indices: targets.iter().filter_map(|id| queue.index_of(id)).collect(),
                item_ids: targets,
            }
        };
        // Only Upcoming items can be selected: the Active Item and the
        // Incoming Track can't be moved, removed or played, so selecting them
        // (or hover feedback on them) would offer nothing.
        let selectable = !row.locked();
        let selected = selectable && self.selection.contains(&row.id);
        let (row_bg, hover_bg) = if selected {
            (
                cx.theme().danger.opacity(0.08),
                cx.theme().danger.opacity(0.14),
            )
        } else {
            (cx.theme().background, cx.theme().accent)
        };

        div()
            .id(SharedString::from(format!("queue-item-{}", row.id)))
            .test_support()
            .group(group)
            .relative()
            .when(selectable, |el| {
                let id = row.id.clone();
                let click_id = row.id.clone();
                let right_id = row.id.clone();
                let click_focus = focus.clone();
                el.key_context(QUEUE_ROW_CONTEXT)
                    .track_focus(&focus)
                    // Shift and Cmd act on mouse down; a plain mouse down on an
                    // unselected row selects it now, so dragging it drags it alone.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            let modifiers = event.modifiers;
                            if modifiers.shift {
                                let selectable = this.selectable_ids(cx);
                                this.selection.select_range(&selectable, &id);
                            } else if modifiers.secondary() {
                                this.selection.toggle(&id);
                            } else if !this.selection.contains(&id) {
                                this.selection.select_only(&id);
                            }
                            click_focus.focus(window, cx);
                            cx.notify();
                        }),
                    )
                    // A plain click (no drag) on a selected row narrows the
                    // selection to that row.
                    .on_click(cx.listener(move |this, event: &ClickEvent, _, cx| {
                        let modifiers = event.modifiers();
                        if !modifiers.shift && !modifiers.secondary() {
                            this.selection.select_only(&click_id);
                            cx.notify();
                        }
                    }))
                    // Right-clicking outside the selection selects just that row.
                    .on_mouse_down(
                        MouseButton::Right,
                        cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                            if !this.selection.contains(&right_id) {
                                this.selection.select_only(&right_id);
                                cx.notify();
                            }
                        }),
                    )
                    .on_drag(drag_payload, move |_, _, _, cx| {
                        let preview = if drag_count > 1 {
                            DragPreview::items(
                                drag_count,
                                preview_track.clone(),
                                preview_image.clone(),
                            )
                        } else {
                            match &preview_track {
                                Some(track) => {
                                    DragPreview::track(track.clone(), preview_image.clone())
                                }
                                None => DragPreview::new_break(),
                            }
                        };
                        cx.new(|_| preview)
                    })
            })
            .on_drag_move(
                cx.listener(move |this, event: &DragMoveEvent<DragPayload>, _, cx| {
                    if !event.bounds.contains(&event.event.position) {
                        return;
                    }
                    let after = event.event.position.y > event.bounds.center().y;
                    let slot = if after { index + 1 } else { index };
                    let payload = event.drag(cx).clone();
                    this.hover_drop_slot(&payload, slot, cx);
                }),
            )
            .on_drop(cx.listener(move |this, payload: &DragPayload, window, cx| {
                this.drop_into_queue(payload, None, window, cx);
            }))
            .h(px(row.size().height()))
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(row_bg)
            .when(selectable, |el| el.hover(move |style| style.bg(hover_bg)))
            .when(targeted, |el| {
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .left_3()
                        .right_3()
                        .h(px(2.))
                        .rounded_full()
                        .bg(accent),
                )
            })
            .context_menu(move |menu, _, _| build_menu(menu, &context_actions, Some(3)))
            .child(content)
            .into_any_element()
    }

    fn render_track_row(
        &mut self,
        row: &RowModel,
        track: &Track,
        group: &SharedString,
        menu_button: &dyn Fn() -> AnyElement,
        skip: AnyElement,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = row.role == Role::Active;
        let image = self
            .artwork
            .update(cx, |artwork, cx| artwork.get(track, cx));
        let (current, reported) = row.progress;
        let duration = if active && reported > 0.0 {
            Some(reported)
        } else {
            track.duration_seconds
        };
        let progress = match (active, duration) {
            (true, Some(duration)) if duration > 0.0 => (current / duration).clamp(0.0, 1.0),
            _ => 0.0,
        };
        let time = if active {
            duration.map_or_else(
                || "—".to_owned(),
                |duration| {
                    format!(
                        "−{}",
                        format_duration(Some((duration - current).max(0.0).ceil()))
                    )
                },
            )
        } else {
            format_duration(track.duration_seconds)
        };

        let status: Option<AnyElement> = match (row.role, row.readiness) {
            (Role::Active, _) => Some(equalizer(cx.theme().danger).into_any_element()),
            (Role::Upcoming, Readiness::Preparing) => Some(
                div()
                    .id(SharedString::from(format!("status-{}", row.id)))
                    .tooltip(|window, cx| Tooltip::new("Preparing").build(window, cx))
                    .child(Spinner::new().xsmall())
                    .into_any_element(),
            ),
            (Role::Incoming, _) => Some(
                div()
                    .id(SharedString::from(format!("status-{}", row.id)))
                    .tooltip(|window, cx| Tooltip::new("Crossfading in").build(window, cx))
                    .child(
                        Icon::new(Lucide::Volume2)
                            .xsmall()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .into_any_element(),
            ),
            (Role::Upcoming, Readiness::Unavailable) => Some(
                div()
                    .id(SharedString::from(format!("status-{}", row.id)))
                    .tooltip(|window, cx| Tooltip::new("Track unavailable").build(window, cx))
                    .child(alert_icon(cx).xsmall())
                    .into_any_element(),
            ),
            _ => None,
        };

        let mut artwork = track_artwork(image, row.size().media(), cx);
        if row.role == Role::Upcoming {
            let station = self.station.clone();
            let id = row.id.clone();
            artwork = artwork.child(
                div()
                    .absolute()
                    .inset_0()
                    .invisible()
                    .group_hover(group.clone(), |style| style.visible())
                    .child(
                        Button::new(SharedString::from(format!("play-{}", row.id)))
                            .primary()
                            .size_full()
                            .icon(IconName::Play)
                            .tooltip(format!("Play {}", track.title))
                            .disabled(row.readiness == Readiness::Preparing)
                            .on_click(move |_, _, cx| {
                                station_command(&station, cx, |session| session.play_item(&id))
                            }),
                    ),
            );
        }

        // Fills the outer row, whose fixed height includes the divider.
        super::row(row.size())
            .h_full()
            .relative()
            .when(active, |el| {
                // Played portion of the Track: a faint red fill with a solid
                // red line along its top edge, growing left to right.
                el.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left_0()
                        .w(relative(progress as f32))
                        .bg(cx.theme().danger.opacity(0.08))
                        .child(div().h(px(2.)).w_full().bg(cx.theme().danger)),
                )
            })
            .child(artwork)
            .child(track_details(track, status.into_iter().collect(), cx))
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(6.))
                    .when(!active, |el| {
                        el.group_hover(group.clone(), |style| style.invisible())
                    })
                    .child(
                        div()
                            .text_xs()
                            .font_features(tabular())
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(time)),
                    )
                    .when(active, |el| el.child(skip).child(menu_button())),
            )
            .when(!active, |el| {
                el.child(
                    div()
                        .absolute()
                        .right_5()
                        .top_0()
                        .bottom_0()
                        .flex()
                        .items_center()
                        .invisible()
                        .group_hover(group.clone(), |style| style.visible())
                        .child(menu_button()),
                )
            })
    }

    fn render_break_row(
        &mut self,
        row: &RowModel,
        group: &SharedString,
        menu_button: &dyn Fn() -> AnyElement,
        skip: AnyElement,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let active = row.role == Role::Active;
        let mut tile = break_tile(row.size().media(), cx).relative();
        if !active {
            let station = self.station.clone();
            let id = row.id.clone();
            tile = tile.child(
                div()
                    .absolute()
                    .inset_0()
                    .invisible()
                    .group_hover(group.clone(), |style| style.visible())
                    .child(
                        Button::new(SharedString::from(format!("play-{}", row.id)))
                            .primary()
                            .size_full()
                            .icon(IconName::Play)
                            .tooltip("Play break")
                            .on_click(move |_, _, cx| {
                                station_command(&station, cx, |session| session.play_item(&id))
                            }),
                    ),
            );
        }
        super::row(row.size())
            .h_full()
            .child(tile)
            .child(div().flex_1().text_sm().font_medium().child("Break"))
            .child(
                h_flex()
                    .gap(px(6.))
                    .when(active, |el| el.child(skip))
                    .child(menu_button()),
            )
    }

    // ---- Keyboard ----------------------------------------------------------

    /// Focuses the row at `index` if it's an Upcoming item (others can't be selected).
    /// IDs of the items that can be selected (the Upcoming ones), in order.
    pub(super) fn selectable_ids(&self, cx: &App) -> Vec<String> {
        let queue = self.station.read(cx).session().queue();
        queue.queued().iter().map(|item| item.id.clone()).collect()
    }

    /// The selectable item one step up or down from the focused row.
    fn neighbour(&self, direction: Direction, window: &Window, cx: &App) -> Option<String> {
        let selectable = self.selectable_ids(cx);
        let focused = self.focused_row(window)?;
        let index = selectable.iter().position(|id| *id == focused)?;
        let next = match direction {
            Direction::Up => index.checked_sub(1)?,
            Direction::Down => index + 1,
        };
        selectable.get(next).cloned()
    }

    fn focus_row(&self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(handle) = self.row_focus.get(id) {
            handle.focus(window, cx);
        }
        cx.notify();
    }

    fn on_remove_selected(
        &mut self,
        _: &RemoveSelected,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let ids = self.selection.ordered(&self.selectable_ids(cx));
        if ids.is_empty() {
            return;
        }
        self.selection.clear();
        station_command(&self.station, cx, |session| session.remove_items(&ids));
        // Nothing is selected afterwards; keep keyboard focus in the window.
        window.focus(&self.focus, cx);
    }

    fn select_step(
        &mut self,
        direction: Direction,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(id) = self.neighbour(direction, window, cx) else {
            return;
        };
        if extend {
            let selectable = self.selectable_ids(cx);
            self.selection.select_range(&selectable, &id);
        } else {
            self.selection.select_only(&id);
        }
        self.focus_row(&id, window, cx);
    }

    fn on_select_previous(
        &mut self,
        _: &SelectPrevious,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_step(Direction::Up, false, window, cx);
    }

    fn on_select_next(&mut self, _: &SelectNext, window: &mut Window, cx: &mut Context<Self>) {
        self.select_step(Direction::Down, false, window, cx);
    }

    fn on_extend_selection_up(
        &mut self,
        _: &ExtendSelectionUp,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_step(Direction::Up, true, window, cx);
    }

    fn on_extend_selection_down(
        &mut self,
        _: &ExtendSelectionDown,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_step(Direction::Down, true, window, cx);
    }

    fn on_select_all(&mut self, _: &SelectAll, _: &mut Window, cx: &mut Context<Self>) {
        let selectable = self.selectable_ids(cx);
        self.selection.select_all(&selectable);
        cx.notify();
    }

    fn on_clear_selection(&mut self, _: &ClearSelection, _: &mut Window, cx: &mut Context<Self>) {
        self.selection.clear();
        cx.notify();
    }

    fn move_selected(&mut self, direction: Direction, cx: &mut Context<Self>) {
        let ids = self.selection.ordered(&self.selectable_ids(cx));
        if !ids.is_empty() {
            station_command(&self.station, cx, |session| {
                session.step_items(&ids, direction)
            });
        }
    }

    fn on_move_selected_up(&mut self, _: &MoveSelectedUp, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selected(Direction::Up, cx);
    }

    fn on_move_selected_down(
        &mut self,
        _: &MoveSelectedDown,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.move_selected(Direction::Down, cx);
    }
}

/// "Reveal in Finder" for local Tracks, "Copy YouTube link" for YouTube Tracks.
pub(super) fn track_link_action(track: &Track, station: &Entity<Station>, cx: &App) -> MenuAction {
    match track.source {
        SourceId::Local => {
            let path = station.read(cx).services().library.path_of(&track.id);
            MenuAction::new(
                format!("Reveal in {}", file_manager_name()),
                IconName::FolderOpen,
                move |_, cx| {
                    if let Some(path) = &path {
                        cx.reveal_path(path);
                    }
                },
            )
            .disabled(false)
        }
        SourceId::YoutubeMusic => {
            let url = track.youtube_music_url().unwrap_or_default();
            MenuAction::new("Copy YouTube link", IconName::Copy, move |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
            })
        }
    }
}

/// The "now playing" equalizer: three bars bouncing at different speeds.
///
/// Bar heights come from the clock each time the row is drawn; while a Track
/// plays the window redraws every display frame (see `AppView::render`).
fn equalizer(color: gpui_kit::Hsla) -> impl IntoElement {
    static START: std::sync::LazyLock<std::time::Instant> =
        std::sync::LazyLock::new(std::time::Instant::now);
    let t = START.elapsed().as_secs_f32();
    let bars = [(7.0_f32, 0.0_f32), (9.5, 1.3), (6.0, 2.6)].map(|(speed, phase)| {
        let level = (t * speed + phase).sin().abs();
        div()
            .w(px(2.))
            .h(px(3. + 9. * level))
            .rounded(px(1.))
            .bg(color)
    });
    h_flex().h(px(12.)).items_end().gap(px(1.5)).children(bars)
}
