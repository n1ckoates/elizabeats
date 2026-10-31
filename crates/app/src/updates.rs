//! Automatic updates through Velopack, from the GitHub releases.
//!
//! Only release builds installed with Velopack (its Setup or Portable
//! downloads) update themselves. Dev builds, `cargo run` and the Microsoft
//! Store version (which the Store updates) report updates as unavailable.
//!
//! Updates are checked at launch and every few hours and downloaded in the
//! background. A downloaded update installs when the app quits, or right away
//! through "Restart to Update"; the app never restarts on its own.

use std::time::Duration;

use gpui_kit::{Context, Task};
use velopack::sources::GithubSource;
use velopack::{UpdateCheck, UpdateInfo, UpdateManager};

pub const REPOSITORY_URL: &str = "https://github.com/n1ckoates/elizabeats";
pub const WEBSITE_URL: &str = "https://elizabeats.nickoates.com";

const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// The version this build shows, e.g. `1.0.0` or `1.0.0-dev (a1b2c3d)`.
pub const VERSION_LABEL: &str = env!("ELIZABEATS_VERSION_LABEL");

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateStatus {
    /// Updates are off in builds other than releases.
    DevBuild,
    /// A release that Velopack didn't install, e.g. the Microsoft Store version.
    NotInstalled,
    Idle,
    Checking,
    UpToDate,
    Downloading {
        version: String,
    },
    /// Downloaded; installs when the app quits.
    Ready {
        version: String,
    },
    Failed(String),
}

pub struct Updates {
    manager: Option<UpdateManager>,
    status: UpdateStatus,
    ready: Option<UpdateInfo>,
    /// Restart into the new version once the update is installed on quit.
    restart_after_quit: bool,
    task: Option<Task<()>>,
    _periodic: Option<Task<()>>,
}

impl Updates {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (manager, status) = if !cfg!(elizabeats_release) {
            (None, UpdateStatus::DevBuild)
        } else {
            // Fails when this copy wasn't installed by Velopack.
            match UpdateManager::new(GithubSource::new(REPOSITORY_URL, None, false), None, None) {
                Ok(manager) => (Some(manager), UpdateStatus::Idle),
                Err(error) => {
                    tracing::info!("updates unavailable: {error}");
                    (None, UpdateStatus::NotInstalled)
                }
            }
        };
        let periodic = manager.is_some().then(|| {
            cx.spawn(async move |this, cx| {
                loop {
                    if this.update(cx, |this, cx| this.check(cx)).is_err() {
                        break;
                    }
                    cx.background_executor().timer(CHECK_INTERVAL).await;
                }
            })
        });
        cx.on_app_quit(|this, _| {
            this.install_on_quit();
            async {}
        })
        .detach();
        Self {
            manager,
            status,
            ready: None,
            restart_after_quit: false,
            task: None,
            _periodic: periodic,
        }
    }

    pub fn status(&self) -> &UpdateStatus {
        &self.status
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.status, UpdateStatus::Ready { .. })
    }

    /// Checks for an update and downloads it. Does nothing while a check or
    /// download is running, or once an update is ready.
    pub fn check(&mut self, cx: &mut Context<Self>) {
        let Some(manager) = self.manager.clone() else {
            return;
        };
        if matches!(
            self.status,
            UpdateStatus::Checking | UpdateStatus::Downloading { .. } | UpdateStatus::Ready { .. }
        ) {
            return;
        }
        self.status = UpdateStatus::Checking;
        cx.notify();
        self.task = Some(cx.spawn(async move |this, cx| {
            let check_manager = manager.clone();
            let checked = cx
                .background_executor()
                .spawn(async move { check_manager.check_for_updates() })
                .await;
            let update = match checked {
                Ok(UpdateCheck::UpdateAvailable(update)) => *update,
                Ok(UpdateCheck::NoUpdateAvailable | UpdateCheck::RemoteIsEmpty) => {
                    let _ = this.update(cx, |this, cx| this.finish(UpdateStatus::UpToDate, cx));
                    return;
                }
                Err(error) => {
                    let _ = this.update(cx, |this, cx| this.fail(&error, cx));
                    return;
                }
            };
            let version = update.TargetFullRelease.Version.clone();
            let _ = this.update(cx, |this, cx| {
                this.finish(
                    UpdateStatus::Downloading {
                        version: version.clone(),
                    },
                    cx,
                )
            });
            let download = update.clone();
            let downloaded = cx
                .background_executor()
                .spawn(async move { manager.download_updates(&download, None) })
                .await;
            let _ = this.update(cx, |this, cx| match downloaded {
                Ok(()) => {
                    tracing::info!("update {version} downloaded");
                    this.ready = Some(update);
                    this.finish(UpdateStatus::Ready { version }, cx);
                }
                Err(error) => this.fail(&error, cx),
            });
        }));
    }

    /// Marks the update to restart into once the app has quit and installed it.
    pub fn set_restart_after_quit(&mut self, restart: bool) {
        self.restart_after_quit = restart;
    }

    fn finish(&mut self, status: UpdateStatus, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    fn fail(&mut self, error: &velopack::Error, cx: &mut Context<Self>) {
        tracing::warn!("update check failed: {error}");
        self.finish(UpdateStatus::Failed(error.to_string()), cx);
    }

    /// Hands a downloaded update to Velopack's updater, which installs it
    /// once this process has exited (and restarts the app if asked to).
    fn install_on_quit(&mut self) {
        let (Some(manager), Some(update)) = (&self.manager, self.ready.take()) else {
            return;
        };
        let restart = self.restart_after_quit;
        if let Err(error) =
            manager.wait_exit_then_apply_updates(&update, true, restart, Vec::<String>::new())
        {
            tracing::warn!("could not start installing the update: {error}");
        }
    }
}
