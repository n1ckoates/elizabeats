//! The root view: layout, dialogs, keyboard shortcuts, and closing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{Local, NaiveDate};
use gpui_kit::component::date_picker::{DatePickerEvent, DatePickerState};
use gpui_kit::component::dialog::AlertDialog;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::resizable::{ResizableState, h_resizable, resizable_panel};
use gpui_kit::component::{ActiveTheme, Theme, ThemeMode, WindowExt, v_flex};
use gpui_kit::{
    AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, ScrollHandle, SharedString, Styled, Subscription, Task, Window,
    WindowAppearance, div, px,
};

use super::selection::Selection;
use super::settings::SettingsView;
use super::{APP_CONTEXT, FocusSearch, InsertBreak, Minimize, Quit, SkipActive, Zoom};
use crate::artwork::ArtworkCache;
use crate::search::Search;
use crate::station::{Station, StationParts};
use crate::updates::Updates;

pub struct AppView {
    pub(super) station: Entity<Station>,
    pub(super) search: Entity<Search>,
    pub(super) updates: Entity<Updates>,
    pub(super) artwork: Entity<ArtworkCache>,
    pub(super) split: Entity<ResizableState>,
    pub(super) search_input: Entity<InputState>,
    pub(super) settings_view: Entity<SettingsView>,
    pub(super) log_date_picker: Entity<DatePickerState>,
    /// Dates that have Play Log entries (the date picker disables the rest).
    pub(super) log_dates: Arc<RwLock<HashSet<NaiveDate>>>,
    pub(super) log_date: Option<NaiveDate>,
    pub(super) log_copied: bool,
    pub(super) tab: usize,
    pub(super) focus: FocusHandle,
    pub(super) row_focus: HashMap<String, FocusHandle>,
    /// Selected Queue items. Cleared by clicking outside the Queue.
    pub(super) selection: Selection,
    pub(super) queue_scroll: ScrollHandle,
    /// Display slot an in-progress drag would drop into.
    pub(super) drop_slot: Option<usize>,
    pub(super) now: chrono::DateTime<Local>,
    theme: elizabeats_core::Theme,
    /// The system's "Reduce motion" accessibility setting, read at launch.
    system_reduce_motion: bool,
    library_revision: u64,
    recovery_prompted: bool,
    close_prompt_open: bool,
    _tasks: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy)]
enum CloseReason {
    Close,
    RestartToUpdate,
}

impl AppView {
    pub fn new(parts: StationParts, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let services = parts.services.clone();
        let station = cx.new(|cx| Station::new(parts, cx));
        let search = cx.new(|cx| Search::new(station.clone(), cx));
        let artwork = cx.new(|_| ArtworkCache::new(services));
        let split = cx.new(|_| ResizableState::default());
        let search_input = cx.new(|cx| InputState::new(window, cx).placeholder("Search..."));
        let updates = cx.new(Updates::new);
        let app_view = cx.entity().downgrade();
        let settings_view =
            cx.new(|cx| SettingsView::new(station.clone(), updates.clone(), app_view, window, cx));
        let log_dates: Arc<RwLock<HashSet<NaiveDate>>> = Arc::default();
        let matcher_dates = log_dates.clone();
        let log_date_picker = cx.new(|cx| {
            DatePickerState::new(window, cx).disabled_matcher(move |date: &NaiveDate| {
                matcher_dates
                    .read()
                    .map(|dates| !dates.contains(date))
                    .unwrap_or(true)
            })
        });

        let mut subscriptions = vec![
            cx.observe_in(&station, window, |this, station, window, cx| {
                let revision = station.read(cx).library_revision();
                if revision != this.library_revision {
                    this.library_revision = revision;
                    this.artwork.update(cx, |artwork, _| artwork.forget_local());
                }
                this.after_station_change(cx);
                let theme = station.read(cx).settings().theme;
                if theme != this.theme {
                    this.theme = theme;
                    this.apply_theme(window, cx);
                }
                cx.notify();
            }),
            cx.observe(&search, |_, _, cx| cx.notify()),
            cx.observe(&updates, |_, _, cx| cx.notify()),
            cx.observe(&artwork, |_, _, cx| cx.notify()),
            cx.subscribe_in(
                &search_input,
                window,
                |this, input, event: &InputEvent, _window, cx| match event {
                    InputEvent::Change => {
                        let query = input.read(cx).value().to_string();
                        this.search
                            .update(cx, |search, cx| search.set_query(query, cx));
                    }
                    InputEvent::PressEnter { .. } => {
                        this.search.update(cx, |search, cx| search.submit(cx));
                    }
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &log_date_picker,
                window,
                |this, _, event: &DatePickerEvent, _, cx| {
                    let DatePickerEvent::Change(value) = event;
                    this.log_date = value.start().map(|start| start.date());
                    cx.notify();
                },
            ),
        ];

        // Follow the system appearance when the theme setting is "System".
        subscriptions.push(cx.observe_window_appearance(window, |this, window, cx| {
            this.apply_theme(window, cx);
        }));

        // A one-second clock for the header.
        let clock = cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let now = Local::now();
                let result = this.update(cx, |this, cx| {
                    if now.timestamp() != this.now.timestamp() {
                        this.now = now;
                        cx.notify();
                    }
                });
                if result.is_err() {
                    break;
                }
            }
        });

        let view = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            view.update(cx, |this, cx| {
                this.request_close(CloseReason::Close, window, cx)
            })
            .unwrap_or(true)
        });

        let mut this = Self {
            station,
            search,
            updates,
            artwork,
            split,
            search_input,
            settings_view,
            log_date_picker,
            log_dates,
            log_date: None,
            log_copied: false,
            tab: 0,
            focus: cx.focus_handle(),
            row_focus: HashMap::new(),
            selection: Selection::default(),
            queue_scroll: ScrollHandle::new(),
            drop_slot: None,
            now: Local::now(),
            theme: elizabeats_core::Theme::System,
            system_reduce_motion: cx.reduce_motion(),
            library_revision: 0,
            recovery_prompted: false,
            close_prompt_open: false,
            _tasks: vec![clock],
            _subscriptions: subscriptions,
        };
        this.theme = this.station.read(cx).settings().theme;
        this.apply_theme(window, cx);
        this.focus.focus(window, cx);
        this.after_station_change(cx);
        this
    }

    pub(super) fn apply_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let theme = self.station.read(cx).settings().theme;
        match theme {
            elizabeats_core::Theme::System => Theme::sync_system_appearance(Some(window), cx),
            elizabeats_core::Theme::Light => Theme::change(ThemeMode::Light, Some(window), cx),
            elizabeats_core::Theme::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
        }
        // Native window chrome (border, traffic lights, menus) follows the
        // app's theme too; `None` lets it follow the system again.
        cx.set_window_appearance(match theme {
            elizabeats_core::Theme::System => None,
            elizabeats_core::Theme::Light => Some(WindowAppearance::Light),
            elizabeats_core::Theme::Dark => Some(WindowAppearance::Dark),
        });
    }

    fn after_station_change(&mut self, cx: &mut Context<Self>) {
        let station = self.station.read(cx);
        let dates: HashSet<NaiveDate> = station
            .session()
            .play_log()
            .iter()
            .filter_map(|entry| super::log_panel::local_date(&entry.started_at))
            .collect();
        if let Ok(mut log_dates) = self.log_dates.write() {
            *log_dates = dates;
        }
        // Forget focus handles for rows that no longer exist.
        let ids: HashSet<String> = station
            .session()
            .queue()
            .items()
            .map(|item| item.id().to_owned())
            .collect();
        self.row_focus.retain(|id, _| ids.contains(id));
        // Items that were removed or started Playing drop out of the selection.
        let selectable = self.selectable_ids(cx);
        self.selection.retain(&selectable);
    }

    // ---- Dialogs -----------------------------------------------------------

    fn show_recovery_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snapshot) = self.station.read(cx).session().pending_recovery() else {
            return;
        };
        self.recovery_prompted = true;
        let count = snapshot.queue.len();
        let first = snapshot.queue.first().map_or("Queue", |item| item.title());
        let summary: SharedString = format!(
            "{count} queue {} saved, starting with “{first}”. Playback will remain stopped.",
            if count == 1 { "item" } else { "items" }
        )
        .into();
        let station = self.station.clone();
        window.open_alert_dialog(cx, move |alert: AlertDialog, _, cx| {
            let recover = station.clone();
            let start_empty = station.clone();
            alert
                .title("Recover interrupted queue?")
                .description("Elizabeats did not close cleanly. Recover the newest saved queue, or begin with an empty session.")
                .child(
                    div()
                        .rounded(px(12.))
                        .bg(cx.theme().muted)
                        .p_3()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(summary.clone()),
                )
                .confirm()
                .keyboard(false)
                .ok_text("Recover Queue")
                .cancel_text("Start Empty")
                .on_ok(move |_, _, cx| {
                    recover.update(cx, |station, cx| station.command(cx, |session| session.restore_recovery()));
                    true
                })
                .on_cancel(move |_, _, cx| {
                    start_empty.update(cx, |station, cx| station.command(cx, |session| session.discard_recovery()));
                    true
                })
        });
    }

    // ---- Closing -----------------------------------------------------------

    /// Window close / Cmd+Q, or restarting into a downloaded update. Returns
    /// whether the window may close right now; while something plays, it
    /// asks first.
    fn request_close(
        &mut self,
        reason: CloseReason,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.station.read(cx).needs_close_confirmation() {
            self.station.update(cx, |station, _| station.shutdown());
            return true;
        }
        if self.close_prompt_open {
            return false;
        }
        self.close_prompt_open = true;
        let (title, description, ok_text) = match reason {
            CloseReason::Close => (
                "Close Elizabeats?",
                "Closing will stop the audio and save this queue as a cleanly closed session.",
                "Close and Stop Playback",
            ),
            CloseReason::RestartToUpdate => (
                "Restart to update?",
                "Restarting will stop the audio and save this queue. Elizabeats reopens with the update installed.",
                "Restart and Stop Playback",
            ),
        };
        let view = cx.entity().downgrade();
        let cancel_view = view.clone();
        window.open_alert_dialog(cx, move |alert: AlertDialog, _, _| {
            let view = view.clone();
            let cancel_view = cancel_view.clone();
            alert
                .title(title)
                .description(description)
                .confirm()
                .ok_text(ok_text)
                .cancel_text("Cancel")
                .on_ok(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        this.close_prompt_open = false;
                        this.station.update(cx, |station, _| station.shutdown());
                    });
                    window.remove_window();
                    true
                })
                .on_cancel(move |_, _, cx| {
                    let _ = cancel_view.update(cx, |this, cx| {
                        this.close_prompt_open = false;
                        this.updates
                            .update(cx, |updates, _| updates.set_restart_after_quit(false));
                    });
                    true
                })
        });
        false
    }

    /// "Restart to Update": quits like closing the window (asking first while
    /// something plays); the update installs once the app has quit, and the
    /// app reopens.
    pub(super) fn restart_to_update(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.updates
            .update(cx, |updates, _| updates.set_restart_after_quit(true));
        if self.request_close(CloseReason::RestartToUpdate, window, cx) {
            window.remove_window();
        }
    }

    // ---- Actions -----------------------------------------------------------

    fn on_quit(&mut self, _: &Quit, window: &mut Window, cx: &mut Context<Self>) {
        if self.request_close(CloseReason::Close, window, cx) {
            window.remove_window();
        }
    }

    fn on_minimize(&mut self, _: &Minimize, window: &mut Window, _: &mut Context<Self>) {
        window.minimize_window();
    }

    fn on_zoom(&mut self, _: &Zoom, window: &mut Window, _: &mut Context<Self>) {
        window.zoom_window();
    }

    fn on_skip(&mut self, _: &SkipActive, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_focused_input(cx) {
            cx.propagate();
            return;
        }
        self.station.update(cx, |station, cx| {
            station.command(cx, |session| session.skip())
        });
    }

    fn on_insert_break(&mut self, _: &InsertBreak, window: &mut Window, cx: &mut Context<Self>) {
        if window.has_focused_input(cx) {
            cx.propagate();
            return;
        }
        // Above the topmost selected item.
        let selected = self
            .selection
            .ordered(&self.selectable_ids(cx))
            .into_iter()
            .next();
        self.station.update(cx, |station, cx| {
            station.command(cx, |session| session.insert_break(selected.as_deref()));
        });
    }

    fn on_focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.tab = 0;
        self.selection.clear();
        self.search_input
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    pub(super) fn focused_row(&self, window: &Window) -> Option<String> {
        self.row_focus
            .iter()
            .find(|(_, handle)| handle.is_focused(window))
            .map(|(id, _)| id.clone())
    }
}

impl Focusable for AppView {
    fn focus_handle(&self, _: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.recovery_prompted && self.station.read(cx).session().pending_recovery().is_some() {
            self.show_recovery_dialog(window, cx);
        }
        // gpui-kit's dialogs fade and slide in, with no option to turn that
        // off; the only switch is gpui's reduce-motion flag. Hold it on only
        // while a dialog is open (and otherwise follow the system setting).
        let reduce_motion = self.system_reduce_motion || window.has_active_dialog(cx);
        if cx.reduce_motion() != reduce_motion {
            cx.set_reduce_motion(reduce_motion);
        }
        // While a Track plays, redraw every display frame so its progress
        // and the equalizer move smoothly (the engine reports ~10x a second).
        let track_playing = self
            .station
            .read(cx)
            .session()
            .queue()
            .active()
            .is_some_and(|active| active.item.as_track().is_some());
        if track_playing {
            window.request_animation_frame();
        }
        let min_panel: Pixels = px(360.);
        v_flex()
            .id("elizabeats")
            .key_context(APP_CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_minimize))
            .on_action(cx.listener(Self::on_zoom))
            .on_action(cx.listener(Self::on_skip))
            .on_action(cx.listener(Self::on_insert_break))
            .on_action(cx.listener(Self::on_focus_search))
            .size_full()
            .overflow_hidden()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_header(window, cx))
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("workspace")
                        .with_state(&self.split)
                        .child(
                            resizable_panel()
                                .size_range(min_panel..Pixels::MAX)
                                .child(self.render_queue_panel(window, cx)),
                        )
                        .child(
                            resizable_panel()
                                .size_range(min_panel..Pixels::MAX)
                                .child(self.render_workspace_panel(window, cx)),
                        ),
                ),
            )
    }
}
