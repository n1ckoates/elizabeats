//! Settings, built on GPUI Kit's Settings component: a sidebar of pages
//! (Library, Playback, Storage, Appearance, About), each with grouped items.

use elizabeats_core::format::format_bytes;
use elizabeats_core::{AppSettings, Theme};
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::dialog::{AlertDialog, Dialog};
use gpui_kit::component::setting::{
    NumberFieldOptions, SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::{ActiveTheme, Disableable, IconName, Sizable, WindowExt, h_flex};
use gpui_kit::{
    App, ClickEvent, Context, Entity, IntoElement, ParentElement, PathPromptOptions, Render,
    SharedString, Styled, Subscription, WeakEntity, Window, div, px,
};

use super::AppView;
use crate::station::Station;
use crate::updates::{REPOSITORY_URL, UpdateStatus, Updates, VERSION_LABEL, WEBSITE_URL};

#[derive(Default)]
struct Errors {
    library: Option<String>,
    play_log: Option<String>,
    settings: Option<String>,
}

pub struct SettingsView {
    station: Entity<Station>,
    updates: Entity<Updates>,
    app_view: WeakEntity<AppView>,
    busy: bool,
    errors: Errors,
    _subscriptions: Vec<Subscription>,
}

pub fn crossfade_label(seconds: u32) -> SharedString {
    match seconds {
        0 => "Off".into(),
        1 => "1 second".into(),
        seconds => format!("{seconds} seconds").into(),
    }
}

const THEMES: [(Theme, &str); 3] = [
    (Theme::System, "System"),
    (Theme::Light, "Light"),
    (Theme::Dark, "Dark"),
];

fn theme_value(theme: Theme) -> SharedString {
    THEMES
        .iter()
        .find(|(candidate, _)| *candidate == theme)
        .map_or("System", |(_, label)| label)
        .into()
}

impl SettingsView {
    pub fn new(
        station: Entity<Station>,
        updates: Entity<Updates>,
        app_view: WeakEntity<AppView>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            _subscriptions: vec![
                cx.observe(&station, |_, _, cx| cx.notify()),
                cx.observe(&updates, |_, _, cx| cx.notify()),
            ],
            station,
            updates,
            app_view,
            busy: false,
            errors: Errors::default(),
        }
    }

    /// Applies a settings change and saves it, showing any failure.
    fn update(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut AppSettings)) {
        let mut next = self.station.read(cx).settings().clone();
        change(&mut next);
        if &next == self.station.read(cx).settings() {
            return;
        }
        self.errors.settings = None;
        let saved = self
            .station
            .update(cx, |station, cx| station.update_settings(next, cx));
        cx.spawn(async move |this, cx| {
            let result = saved
                .await
                .unwrap_or_else(|_| Err("Settings could not be saved.".into()));
            let _ = this.update(cx, |this, cx| {
                this.errors.settings = result
                    .err()
                    .map(|error| format!("Could not save settings: {error}"));
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn choose_folder(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose Library Folder".into()),
        });
        self.errors.library = None;
        cx.spawn(async move |this, cx| {
            let chosen = paths.await;
            let _ = this.update(cx, |this, cx| match chosen {
                Ok(Ok(Some(paths))) => {
                    if let Some(folder) = paths.into_iter().next() {
                        let folder = folder.to_string_lossy().into_owned();
                        this.update(cx, move |settings| settings.library_folder = Some(folder));
                    }
                }
                Ok(Err(error)) => {
                    this.errors.library =
                        Some(format!("Could not choose a library folder: {error}"));
                    cx.notify();
                }
                _ => {}
            });
        })
        .detach();
    }

    fn rescan(&mut self, cx: &mut Context<Self>) {
        let services = self.station.read(cx).services().clone();
        let library = services.library.clone();
        let task = services
            .runtime
            .spawn(async move { library.rescan_and_wait().await });
        self.run_busy(cx, |errors| &mut errors.library, async move {
            task.await
                .map_err(|_| "Could not rescan the library.".to_owned())
        });
    }

    fn clear_log(&mut self, cx: &mut Context<Self>) {
        let cleared = self
            .station
            .update(cx, |station, cx| station.clear_play_log(cx));
        self.run_busy(cx, |errors| &mut errors.play_log, async move {
            cleared
                .await
                .unwrap_or_else(|_| Err("Play log could not be cleared.".into()))
                .map_err(|error| format!("Could not clear the play log: {error}"))
        });
    }

    fn clear_cache(&mut self, cx: &mut Context<Self>) {
        let cleared = self.station.read(cx).clear_download_cache();
        self.run_busy(cx, |errors| &mut errors.settings, async move {
            cleared
                .await
                .map_err(|error| format!("Could not clear the download cache: {error}"))
        });
    }

    /// Runs `work` with the buttons disabled, then shows its error (if any).
    /// `error` picks where the message goes, e.g. `|errors| &mut errors.library`.
    fn run_busy(
        &mut self,
        cx: &mut Context<Self>,
        error: fn(&mut Errors) -> &mut Option<String>,
        work: impl Future<Output = Result<(), String>> + 'static,
    ) {
        self.busy = true;
        *error(&mut self.errors) = None;
        cx.spawn(async move |this, cx| {
            let result = work.await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                *error(&mut this.errors) = result.err();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// Asks before a destructive action, then runs `action`.
    fn confirm(
        &mut self,
        confirmation: &Confirmation,
        action: fn(&mut Self, &mut Context<Self>),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let view = cx.entity().downgrade();
        let &Confirmation {
            title,
            description,
            ok_text,
        } = confirmation;
        window.open_alert_dialog(cx, move |alert: AlertDialog, _, _| {
            let view = view.clone();
            alert
                .title(title)
                .description(description)
                .confirm()
                .ok_text(ok_text)
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("Cancel")
                .on_ok(move |_, _, cx| {
                    let _ = view.update(cx, action);
                    true
                })
        });
    }
}

/// The wording of a "are you sure?" dialog.
struct Confirmation {
    title: &'static str,
    description: &'static str,
    ok_text: &'static str,
}

const CLEAR_LOG: Confirmation = Confirmation {
    title: "Clear the complete play log?",
    description: "This removes the saved history. The action cannot be undone.",
    ok_text: "Clear Play Log",
};

const CLEAR_CACHE: Confirmation = Confirmation {
    title: "Clear the download cache?",
    description: "Tracks currently playing or in the queue will be removed after they are no longer needed. Play counts will be kept.",
    ok_text: "Clear Download Cache",
};

/// A click handler that runs `action` on the Settings view.
fn on_view(
    view: &WeakEntity<SettingsView>,
    action: impl Fn(&mut SettingsView, &mut Window, &mut Context<SettingsView>) + 'static,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let view = view.clone();
    move |_, window, cx| {
        let _ = view.update(cx, |this, cx| action(this, window, cx));
    }
}

/// Calls `change` on the Settings view from a field's `set_value` callback.
fn change(view: &WeakEntity<SettingsView>, cx: &mut App, change: impl FnOnce(&mut AppSettings)) {
    let _ = view.update(cx, |view, cx| view.update(cx, change));
}

/// An inline error under a group, when there is one.
fn error_item(message: Option<String>) -> Option<SettingItem> {
    message.map(|message| {
        SettingItem::render(move |_, _, cx| {
            div()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(SharedString::from(message.clone()))
        })
    })
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let station = self.station.read(cx);
        let settings = station.settings().clone();
        let cache = station.cache_status();
        let log_count = station.session().play_log().len();
        let busy = self.busy;
        let view = cx.entity().downgrade();
        let get = self.station.downgrade();
        let read = move |cx: &App| {
            get.upgrade()
                .map(|station| station.read(cx).settings().clone())
                .unwrap_or_default()
        };

        // ---- Library ----
        let folder_text: SharedString = settings
            .library_folder
            .clone()
            .unwrap_or_else(|| "No folder selected".into())
            .into();
        let choose_view = view.clone();
        let rescan_view = view.clone();
        let has_folder = settings.library_folder.is_some();
        let library = SettingPage::new("Library")
            .icon(IconName::FolderOpen)
            .group(
                SettingGroup::new()
                    .item(
                        SettingItem::new(
                            "Library folder",
                            SettingField::render(move |_, _, cx| {
                                h_flex()
                                    .gap_2()
                                    .child(
                                        div()
                                            .max_w(px(260.))
                                            .truncate()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(folder_text.clone()),
                                    )
                                    .child(
                                        Button::new("choose-library")
                                            .outline()
                                            .small()
                                            .icon(IconName::FolderOpen)
                                            .label("Choose…")
                                            .disabled(busy)
                                            .on_click(on_view(&choose_view, Self::choose_folder)),
                                    )
                            }),
                        )
                        .description(if has_folder {
                            "MP3 and WAV tracks are indexed recursively."
                        } else {
                            "Choose a folder to index local tracks."
                        }),
                    )
                    .item(SettingItem::new(
                        "Rescan library",
                        SettingField::render(move |_, _, _| {
                            Button::new("rescan")
                                .outline()
                                .small()
                                .label("Rescan")
                                .loading(busy)
                                .disabled(!has_folder || busy)
                                .on_click(on_view(&rescan_view, |this, _, cx| this.rescan(cx)))
                        }),
                    ))
                    .items(error_item(self.errors.library.clone())),
            );

        // ---- Playback ----
        let crossfade_options = AppSettings::CROSSFADE_OPTIONS
            .iter()
            .map(|seconds| {
                (
                    SharedString::from(seconds.to_string()),
                    crossfade_label(*seconds),
                )
            })
            .collect();
        let crossfade_read = read.clone();
        let crossfade_view = view.clone();
        let playback = SettingPage::new("Playback").icon(IconName::Play).group(
            SettingGroup::new().item(
                SettingItem::new(
                    "Crossfade duration",
                    SettingField::dropdown(
                        crossfade_options,
                        move |cx| {
                            crossfade_read(cx)
                                .crossfade_duration_seconds
                                .to_string()
                                .into()
                        },
                        move |value: SharedString, cx| {
                            if let Ok(seconds) = value.parse::<u32>() {
                                change(&crossfade_view, cx, |settings| {
                                    settings.crossfade_duration_seconds = seconds
                                });
                            }
                        },
                    ),
                )
                .description("How long adjacent tracks overlap."),
            ),
        );

        // ---- Storage ----
        let log_view = view.clone();
        let cache_view = view.clone();
        let size_read = read.clone();
        let size_view = view.clone();
        let cache_used = format!(
            "{} of {} GiB used{}",
            format_bytes(cache.used_bytes),
            settings.download_cache_size_gib,
            if cache.pending_clear_count > 0 {
                format!(" · {} queued for removal", cache.pending_clear_count)
            } else {
                String::new()
            }
        );
        let storage = SettingPage::new("Storage")
            .icon(gpui_kit::assets::IconName::HardDrive)
            .group(
                SettingGroup::new()
                    .title("Play Log")
                    .item(
                        SettingItem::new(
                            "Saved history",
                            SettingField::render(move |_, _, _| {
                                Button::new("clear-log")
                                    .danger()
                                    .small()
                                    .label("Clear Log")
                                    .disabled(log_count == 0 || busy)
                                    .on_click(on_view(&log_view, |this, window, cx| {
                                        this.confirm(&CLEAR_LOG, Self::clear_log, window, cx)
                                    }))
                            }),
                        )
                        .description(format!(
                            "{log_count} saved {}",
                            if log_count == 1 { "entry" } else { "entries" }
                        )),
                    )
                    .items(error_item(self.errors.play_log.clone())),
            )
            .group(
                SettingGroup::new()
                    .title("Download Cache")
                    .item(
                        SettingItem::new(
                            "Downloaded audio",
                            SettingField::render(move |_, _, _| {
                                Button::new("clear-cache")
                                    .danger()
                                    .small()
                                    .label("Clear Cache")
                                    .disabled(cache.used_bytes == 0 || busy)
                                    .on_click(on_view(&cache_view, |this, window, cx| {
                                        this.confirm(&CLEAR_CACHE, Self::clear_cache, window, cx)
                                    }))
                            }),
                        )
                        .description(cache_used),
                    )
                    .item(
                        SettingItem::new(
                            "Cache size (GiB)",
                            SettingField::number_input(
                                NumberFieldOptions {
                                    min: f64::from(AppSettings::MIN_CACHE_GIB),
                                    max: f64::from(AppSettings::MAX_CACHE_GIB),
                                    step: 1.,
                                },
                                move |cx| f64::from(size_read(cx).download_cache_size_gib),
                                move |value: f64, cx| {
                                    let gib = value.round().clamp(
                                        f64::from(AppSettings::MIN_CACHE_GIB),
                                        f64::from(AppSettings::MAX_CACHE_GIB),
                                    );
                                    // Clamped to 1..=25 above, so the cast is exact.
                                    #[allow(
                                        clippy::cast_possible_truncation,
                                        clippy::cast_sign_loss
                                    )]
                                    let gib = gib as u32;
                                    change(&size_view, cx, |settings| {
                                        settings.download_cache_size_gib = gib
                                    });
                                },
                            ),
                        )
                        .description("Downloads are kept between sessions."),
                    ),
            );

        // ---- Appearance ----
        let theme_options = THEMES
            .iter()
            .map(|(_, label)| (SharedString::from(*label), SharedString::from(*label)))
            .collect();
        let theme_read = read;
        let theme_view = view;
        let appearance = SettingPage::new("Appearance").icon(IconName::Sun).group(
            SettingGroup::new()
                .item(SettingItem::new(
                    "Theme",
                    SettingField::dropdown(
                        theme_options,
                        move |cx| theme_value(theme_read(cx).theme),
                        move |value: SharedString, cx| {
                            if let Some((theme, _)) =
                                THEMES.iter().find(|(_, label)| *label == value.as_ref())
                            {
                                let theme = *theme;
                                change(&theme_view, cx, |settings| settings.theme = theme);
                            }
                        },
                    ),
                ))
                .items(error_item(self.errors.settings.clone())),
        );

        let about = self.render_about(cx);

        div()
            .size_full()
            .child(Settings::new("settings").pages([library, playback, storage, appearance, about]))
    }
}

impl SettingsView {
    fn render_about(&self, cx: &App) -> SettingPage {
        let status = self.updates.read(cx).status().clone();
        let status_text: SharedString = match &status {
            UpdateStatus::DevBuild => "Updates are off in development builds.".into(),
            UpdateStatus::NotInstalled => {
                "This copy doesn't update itself. The Microsoft Store version updates through the Store."
                    .into()
            }
            UpdateStatus::Idle => "Elizabeats checks for updates automatically.".into(),
            UpdateStatus::Checking => "Checking for updates…".into(),
            UpdateStatus::UpToDate => "Elizabeats is up to date.".into(),
            UpdateStatus::Downloading { version } => format!("Downloading version {version}…").into(),
            UpdateStatus::Ready(update) => format!(
                "Version {} is ready. It installs when you quit Elizabeats.",
                update.TargetFullRelease.Version
            )
            .into(),
            UpdateStatus::Failed(error) => format!("Couldn't check for updates: {error}").into(),
        };
        let ready = matches!(status, UpdateStatus::Ready { .. });
        let can_check = matches!(
            status,
            UpdateStatus::Idle | UpdateStatus::UpToDate | UpdateStatus::Failed(_)
        );
        let updates = self.updates.clone();
        let app_view = self.app_view.clone();

        let link = |id: &'static str, label: &'static str, icon: Lucide, url: &'static str| {
            SettingField::render(move |_, _, _| {
                Button::new(id)
                    .outline()
                    .small()
                    .icon(icon)
                    .label(label)
                    .on_click(move |_, _, cx| cx.open_url(url))
            })
        };

        SettingPage::new("About")
            .icon(Lucide::Info)
            .group(
                SettingGroup::new()
                    .item(SettingItem::new(
                        "Version",
                        SettingField::render(|_, _, _| div().text_sm().child(VERSION_LABEL)),
                    ))
                    .item(
                        SettingItem::new(
                            "Updates",
                            SettingField::render(move |_, _, _| {
                                let updates = updates.clone();
                                let app_view = app_view.clone();
                                if ready {
                                    Button::new("about-restart-to-update")
                                        .primary()
                                        .small()
                                        .label("Restart to Update")
                                        .on_click(move |_, window, cx| {
                                            let _ = app_view.update(cx, |view, cx| {
                                                view.restart_to_update(window, cx)
                                            });
                                        })
                                } else {
                                    Button::new("check-for-updates")
                                        .outline()
                                        .small()
                                        .label("Check for Updates")
                                        .disabled(!can_check)
                                        .on_click(move |_, _, cx| {
                                            updates.update(cx, |updates, cx| updates.check(cx));
                                        })
                                }
                            }),
                        )
                        .description(status_text),
                    ),
            )
            .group(
                SettingGroup::new()
                    .title("Links")
                    .item(SettingItem::new(
                        "Source code",
                        link("open-github", "GitHub", Lucide::Github, REPOSITORY_URL),
                    ))
                    .item(SettingItem::new(
                        "Website",
                        link(
                            "open-website",
                            "elizabeats.nickoates.com",
                            Lucide::Globe,
                            WEBSITE_URL,
                        ),
                    )),
            )
    }
}

impl AppView {
    pub(super) fn render_settings_button(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let settings_view = self.settings_view.clone();
        // Default size: a 16 px icon, as in the Electron app (`.large()` makes
        // the icon 24 px).
        Button::new("open-settings")
            .outline()
            .icon(IconName::Settings)
            .tooltip("Settings")
            .on_click(move |_, window, cx| {
                let view = settings_view.clone();
                // Settings is a full-page layout (sidebar + content), so it
                // fills the dialog edge to edge instead of sitting inside the
                // dialog's own title and padding.
                window.open_dialog(cx, move |dialog: Dialog, _, _| {
                    dialog
                        .p_0()
                        .w(px(820.))
                        .child(div().h(px(560.)).child(view.clone()))
                });
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossfade_labels() {
        assert_eq!(crossfade_label(0), "Off");
        assert_eq!(crossfade_label(1), "1 second");
        assert_eq!(crossfade_label(15), "15 seconds");
    }

    #[test]
    fn theme_values_round_trip() {
        for (theme, label) in THEMES {
            assert_eq!(theme_value(theme), label);
        }
    }
}
