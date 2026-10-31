//! Elizabeats: DJ-operated radio playout.

// Release builds on Windows are GUI apps without a console window.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod artwork;
mod assets;
mod persist;
mod search;
mod station;
mod ui;
mod updates;

use std::path::PathBuf;
use std::sync::Arc;

use elizabeats_audio::AudioEngine;
use elizabeats_library::LocalLibrary;
use elizabeats_youtube::{YoutubeConfig, YoutubeMusic};
use gpui_kit::component::TitleBar;
use gpui_kit::{
    AppContext, Bounds, QuitMode, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

use crate::persist::Persistence;
use crate::station::{Services, StationEvent, StationParts};

fn main() {
    // When the app launches itself as its BotGuard web view helper, act as
    // that helper and exit before any UI starts.
    if let Some(code) = elizabeats_botguard::helper_main_if_requested() {
        std::process::exit(code);
    }

    // Velopack's install/update hooks: the installer and updater launch the
    // app with special arguments, and a downloaded update that wasn't
    // installed yet gets installed here. After the helper check on purpose,
    // so the helper process never does this.
    velopack::VelopackApp::build().run();

    // ELIZABEATS_DATA_DIR points the app at another data folder (testing).
    let launch = Launch {
        data_dir: std::env::var_os("ELIZABEATS_DATA_DIR").map(PathBuf::from),
        ..Launch::default()
    };
    let parts = match bootstrap(launch) {
        Ok(parts) => parts,
        Err(error) => {
            eprintln!("Elizabeats could not start: {error}");
            std::process::exit(1);
        }
    };

    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .with_quit_mode(QuitMode::LastWindowClosed)
        .run(move |cx| {
            gpui_kit::init(cx);
            ui::bind_keys(cx);
            ui::set_menus(cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1440.), px(900.)),
                    cx,
                ))),
                window_min_size: Some(size(px(1000.), px(650.))),
                // Linux desktops match the window to `elizabeats.desktop` (from
                // the .deb) by this ID, for its dock/taskbar icon and grouping.
                app_id: Some("elizabeats".into()),
                // The app header is the title bar (gpui-kit's TitleBar).
                titlebar: Some(TitlebarOptions {
                    title: Some("Elizabeats".into()),
                    traffic_light_position: Some(ui::traffic_light_position()),
                    ..TitleBar::title_bar_options()
                }),
                ..TitleBar::window_options()
            };
            let opened = gpui_kit::open_window(options, cx, move |window, cx| {
                // X11 only takes the ID (as WM_CLASS) this way, not from the options.
                window.set_app_id("elizabeats");
                cx.new(|cx| ui::AppView::new(parts, window, cx))
            });
            if let Err(error) = opened {
                eprintln!("Elizabeats could not open its window: {error}");
                cx.quit();
                return;
            }
            cx.activate(true);
        });
}

/// Where to keep data and which optional services to start.
#[derive(Default)]
pub struct Launch {
    /// `None` uses the per-user application data folder.
    pub data_dir: Option<PathBuf>,
    /// Tests skip BotGuard (no helper process, no YouTube attestation).
    pub without_botguard: bool,
    /// Executable to launch as the BotGuard web view helper; `None` means this
    /// executable. Tests point it at the built app binary.
    pub helper_program: Option<PathBuf>,
}

/// Creates the data directory and all long-lived services.
pub fn bootstrap(launch: Launch) -> Result<StationParts, String> {
    let (data_dir, settings) = match launch.data_dir {
        Some(dir) => {
            std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
            let settings = elizabeats_store::load_settings(&dir);
            (dir, settings)
        }
        None => {
            let dir: PathBuf = elizabeats_store::data_dir().map_err(|error| error.to_string())?;
            let settings = elizabeats_store::load_settings(&dir);
            (dir, settings)
        }
    };
    elizabeats_store::remove_stale_temp_files(&data_dir);

    let diagnostics =
        elizabeats_store::DiagnosticsStore::new(data_dir.join(elizabeats_store::DIAGNOSTICS_FILE));
    let sink = diagnostics.sink();
    let (events, receiver) = station::event_channel();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("elizabeats-io")
        .enable_all()
        .build()
        .map_err(|error| format!("could not start background runtime: {error}"))?;

    let botguard = if launch.without_botguard {
        None
    } else {
        let started = match launch.helper_program {
            Some(program) => elizabeats_botguard::BotguardService::start_with_helper(
                elizabeats_botguard::HelperOptions::webview(program, &data_dir),
            ),
            None => elizabeats_botguard::BotguardService::start(&data_dir),
        };
        match started {
            Ok(service) => {
                // Start the web view helper (token minter + youtubei.js session)
                // now, in the background, so the first YouTube Track resolves in
                // ~0.2 s instead of ~1 s. If this fails (e.g. offline), the
                // first real request simply starts it again.
                service.prewarm();
                Some(service)
            }
            Err(error) => {
                sink(elizabeats_core::DiagnosticEvent::new("botguard").error(error.to_string()));
                None
            }
        }
    };

    let audio_events = events.clone();
    let audio = AudioEngine::new(move |event| {
        let _ = audio_events.unbounded_send(StationEvent::Audio(event));
    });

    let library_events = events.clone();
    let library = Arc::new(LocalLibrary::new(
        Arc::new(move || {
            let _ = library_events.unbounded_send(StationEvent::LibraryChanged);
        }),
        sink.clone(),
    ));

    let cache_events = events.clone();
    let youtube = {
        // YoutubeMusic starts its workers on the I/O runtime.
        let _guard = runtime.enter();
        Arc::new(
            YoutubeMusic::new(YoutubeConfig {
                data_dir: data_dir.clone(),
                botguard,
                diagnostics: sink,
                on_cache_change: Arc::new(move || {
                    let _ = cache_events.unbounded_send(StationEvent::CacheChanged);
                }),
            })
            .map_err(|error| error.to_string())?,
        )
    };
    {
        let _guard = runtime.enter();
        youtube.prewarm();
    }

    let persist_events = events.clone();
    let persistence = Persistence::start(data_dir.clone(), move |message| {
        let _ = persist_events.unbounded_send(StationEvent::Error(message));
    });

    let http = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36")
        .build()
        .map_err(|error| error.to_string())?;

    let services = Arc::new(Services {
        runtime,
        data_dir,
        audio,
        library,
        youtube,
        diagnostics,
        persistence,
        http,
    });
    Ok(StationParts {
        services,
        settings,
        events: (events, receiver),
    })
}
