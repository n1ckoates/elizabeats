//! Right panel: Search / Log tabs, and the Search results list.

use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use elizabeats_core::format::format_duration;
use elizabeats_core::{SourceId, Track};
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::alert::Alert;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tag::Tag;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable, h_flex, v_flex};
use gpui_kit::{
    AnyElement, AppContext, Context, InteractiveElement, IntoElement, Keystroke, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
    uniform_list,
};

use gpui_kit::base::TestSupportExt;

use super::drag::{DragPayload, DragPreview};
use super::menus::{MenuAction, build_menu};
use super::queue_panel::track_link_action;
use super::{
    AppView, PANEL_HEADER_HEIGHT, RowSize, row, tabular, track_artwork, track_details, with_tooltip,
};
use crate::search::SearchStatus;

const RESULT_GROUP: &str = "search-row";

impl AppView {
    pub(super) fn render_workspace_panel(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let log_controls = (self.tab == 1).then(|| self.render_log_controls(cx));
        v_flex()
            .size_full()
            .min_w_0()
            .child(
                h_flex().flex_none().h(px(PANEL_HEADER_HEIGHT)).child(
                    // Default tabs are exactly as tall as the header, and the
                    // bar's own bottom border is the panel's divider.
                    TabBar::new("workspace-tabs")
                        .w_full()
                        .selected_index(self.tab)
                        .when_some(log_controls, |bar, controls| bar.suffix(controls))
                        .children([Tab::new().label("Search"), Tab::new().label("Log")])
                        .on_click(cx.listener(|this, index: &usize, _, cx| {
                            this.tab = *index;
                            cx.notify();
                        })),
                ),
            )
            .child(div().flex_1().min_h_0().map(|el| {
                if self.tab == 0 {
                    el.child(self.render_search(window, cx).into_any_element())
                } else {
                    el.child(self.render_log(window, cx).into_any_element())
                }
            }))
    }

    fn render_search(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let search = self.search.read(cx);
        let tracks: Arc<Vec<Track>> = Arc::new(search.tracks().to_vec());
        let errors: Vec<String> = search.errors().to_vec();
        let status = search.status();
        let query_empty = search.query().trim().is_empty();
        let cached = self.station.read(cx).cached_video_ids();

        let body: AnyElement = if status == SearchStatus::Searching && tracks.is_empty() {
            skeleton_rows(cx).into_any_element()
        } else if status == SearchStatus::Complete
            && errors.is_empty()
            && !query_empty
            && tracks.is_empty()
        {
            div()
                .px_5()
                .py_10()
                .text_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("No tracks found.")
                .into_any_element()
        } else {
            let list_tracks = tracks.clone();
            // One extra, empty row at the end lets the last result scroll clear
            // of the floating search box. (Bottom padding would do the same
            // visually, but the list treats padding as off-screen and skips
            // drawing rows that sit mostly under the search box's gradient.)
            uniform_list(
                "search-results",
                tracks.len() + 1,
                cx.processor(move |this, range: Range<usize>, _window, cx| {
                    let tracks = list_tracks.clone();
                    range
                        .map(|index| match tracks.get(index) {
                            Some(track) => {
                                this.render_search_row(index, track, tracks.len(), &cached, cx)
                            }
                            // The list needs uniform rows, so the spacer is one row tall.
                            None => div().h(px(RowSize::Regular.height())).into_any_element(),
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .size_full()
            .into_any_element()
        };

        div()
            .relative()
            .size_full()
            .child(
                v_flex()
                    .size_full()
                    .children(errors.into_iter().enumerate().map(|(index, error)| {
                        Alert::error(("search-error", index), error).banner()
                    }))
                    .child(div().flex_1().min_h_0().child(body)),
            )
            .child(self.render_search_box(cx))
    }

    fn render_search_box(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let background = cx.theme().background;
        div()
            .absolute()
            .left_0()
            .right_0()
            .bottom_0()
            .h(px(80.))
            .child(div().absolute().inset_0().bg(gpui_kit::linear_gradient(
                180.,
                gpui_kit::linear_color_stop(background.opacity(0.), 0.),
                gpui_kit::linear_color_stop(background, 0.45),
            )))
            .child(
                // The input and the opaque strip around it swallow the mouse, so
                // pressing or dragging there never reaches the result row hidden
                // beneath (which would start a track drag instead of selecting
                // text). The faded top of the gradient stays click-through.
                div()
                    .id("search-box")
                    .test_support()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom_0()
                    .px_5()
                    .pb_4()
                    .occlude()
                    .child(
                        Input::new(&self.search_input)
                            .prefix(
                                Icon::new(IconName::Search)
                                    .small()
                                    .text_color(cx.theme().muted_foreground),
                            )
                            .suffix(
                                Kbd::new(Keystroke::parse("secondary-k").unwrap_or_default())
                                    .outline(),
                            )
                            .cleanable(false),
                    ),
            )
    }

    fn render_search_row(
        &mut self,
        index: usize,
        track: &Track,
        count: usize,
        cached: &Arc<HashSet<String>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let image = self
            .artwork
            .update(cx, |artwork, cx| artwork.get(track, cx));
        let group = SharedString::from(format!("{RESULT_GROUP}-{index}"));
        let key = track.key().to_string();
        let is_cached = track.source == SourceId::YoutubeMusic && cached.contains(&track.id);

        let actions = self.search_actions(track, cx);
        let context_actions = actions.clone();
        let menu_actions = actions;

        let mut badges: Vec<AnyElement> = Vec::new();
        if track.explicit {
            badges.push(with_tooltip(
                format!("explicit-{key}"),
                "Explicit",
                Tag::secondary().small().child("E"),
            ));
        }
        if is_cached {
            badges.push(with_tooltip(
                format!("cached-{key}"),
                "Downloaded",
                Icon::new(Lucide::Download)
                    .xsmall()
                    .text_color(cx.theme().muted_foreground),
            ));
        }

        let add_station = self.station.clone();
        let add_track = track.clone();
        let payload = DragPayload::SearchTrack(track.clone());
        let drag_image = image.clone();

        row(RowSize::Regular)
            .id(SharedString::from(format!("search-{key}")))
            .test_support()
            .group(group.clone())
            .relative()
            .when(index + 1 < count, |el| {
                el.border_b_1().border_color(cx.theme().border)
            })
            .hover(|style| style.bg(cx.theme().accent))
            .tooltip(|window, cx| Tooltip::new("Drag to queue").build(window, cx))
            .on_drag(payload, move |payload, _, _, cx| {
                let track = match payload {
                    DragPayload::SearchTrack(track) => Some(track.clone()),
                    _ => None,
                };
                let image = drag_image.clone();
                cx.new(|_| {
                    track.map_or_else(DragPreview::new_break, |track| {
                        DragPreview::track(track, image)
                    })
                })
            })
            .context_menu(move |menu, _, _| build_menu(menu, &context_actions, None))
            .child(track_artwork(image, RowSize::Regular.media(), cx))
            .child(track_details(track, badges, cx))
            .child(
                h_flex()
                    .flex_none()
                    .gap(px(6.))
                    .group_hover(group.clone(), |style| style.invisible())
                    .when(track.source == SourceId::Local, |el| {
                        el.child(
                            Icon::new(Lucide::FileMusic)
                                .xsmall()
                                .text_color(cx.theme().primary),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .font_features(tabular())
                            .text_color(cx.theme().muted_foreground)
                            .child(SharedString::from(format_duration(track.duration_seconds))),
                    ),
            )
            .child(
                h_flex()
                    .absolute()
                    .right_5()
                    .top_0()
                    .bottom_0()
                    .gap(px(6.))
                    .invisible()
                    .group_hover(group, |style| style.visible())
                    .child(
                        Button::new(SharedString::from(format!("add-{key}")))
                            .ghost()
                            .xsmall()
                            .icon(Lucide::ListEnd)
                            .tooltip("Add to queue")
                            .on_click(move |_, _, cx| {
                                let track = add_track.clone();
                                add_station.update(cx, |station, cx| {
                                    station.command(cx, |session| session.add_track_to_end(track));
                                });
                            }),
                    )
                    .child(
                        Button::new(SharedString::from(format!("more-{key}")))
                            .ghost()
                            .xsmall()
                            .icon(IconName::EllipsisVertical)
                            .tooltip(format!("More actions for {}", track.title))
                            .dropdown_menu(move |menu, _, _| build_menu(menu, &menu_actions, None)),
                    ),
            )
            .into_any_element()
    }

    fn search_actions(&self, track: &Track, cx: &gpui_kit::App) -> Vec<MenuAction> {
        let add_end = {
            let (station, track) = (self.station.clone(), track.clone());
            MenuAction::new("Add to queue", Lucide::ListEnd, move |_, cx| {
                let track = track.clone();
                station.update(cx, |station, cx| {
                    station.command(cx, |session| session.add_track_to_end(track))
                });
            })
        };
        let add_next = {
            let (station, track) = (self.station.clone(), track.clone());
            MenuAction::new("Play next", Lucide::ListStart, move |_, cx| {
                let track = track.clone();
                station.update(cx, |station, cx| {
                    station.command(cx, |session| session.add_track_next(track))
                });
            })
        };
        let link = match track.source {
            SourceId::YoutubeMusic => track_link_action(track, &self.station, cx),
            SourceId::Local => {
                MenuAction::new("Copy YouTube link", IconName::Copy, |_, _| {}).disabled(true)
            }
        };
        vec![add_end, add_next, link]
    }
}

fn skeleton_rows(cx: &mut Context<AppView>) -> impl IntoElement {
    v_flex().children((0..20).map(|index| {
        row(RowSize::Regular)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                Skeleton::new()
                    .size(px(RowSize::Regular.media()))
                    .rounded(px(8.)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .gap_2()
                    .child(
                        Skeleton::new()
                            .h_4()
                            .w(gpui_kit::relative(if index % 2 == 0 { 0.6 } else { 0.4 })),
                    )
                    .child(
                        Skeleton::new()
                            .h_3()
                            .w(gpui_kit::relative(if index % 2 == 0 { 0.4 } else { 0.33 })),
                    ),
            )
            .child(Skeleton::new().h_3().w(px(36.)))
    }))
}
