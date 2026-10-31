//! Headless UI tests: the real window, real services, temporary data folder.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::time::Duration;

use elizabeats_core::queue::ItemKind;
use elizabeats_core::{AppSettings, RecoverySnapshot, RecoveryStatus, StoredQueueItem};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AnyWindowHandle, AppContext, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};

use super::AppView;
use crate::{Launch, bootstrap};

fn open_app(cx: &mut TestAppContext, dir: &Path) -> (AnyWindowHandle, Entity<AppView>) {
    // Real background threads (I/O runtime, persistence, Library scanner) wake UI tasks.
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        super::bind_keys(cx);
    });
    let parts = bootstrap(Launch {
        data_dir: Some(dir.to_path_buf()),
        without_botguard: true,
        helper_program: None,
    })
    .expect("bootstrap");
    cx.update(|cx| {
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds {
                origin: Point::default(),
                size: size(px(1440.), px(900.)),
            })),
            ..Default::default()
        };
        let (window, view) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| AppView::new(parts, window, cx))
        })
        .expect("open window");
        (window, view)
    })
}

fn queue_len(view: &Entity<AppView>, cx: &mut TestAppContext) -> usize {
    view.read_with(cx, |view, cx| view.station.read(cx).session().queue().len())
}

/// A one-second silent mono WAV.
fn write_wav(path: &Path) {
    let rate: u32 = 8000;
    let samples = rate;
    let data_len = samples * 2;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes());
    bytes.extend_from_slice(&rate.to_le_bytes());
    bytes.extend_from_slice(&(rate * 2).to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    bytes.resize(bytes.len() + data_len as usize, 0);
    std::fs::write(path, bytes).unwrap();
}

#[gpui_kit::test]
async fn starts_empty_and_add_break_inserts_a_break(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open_app(cx, dir.path());
    assert_eq!(queue_len(&view, cx), 0);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("add-break", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(queue_len(&view, cx), 1);
    let is_break = view.read_with(cx, |view, cx| {
        let station = view.station.read(cx);
        matches!(station.session().queue().queued()[0].kind, ItemKind::Break)
    });
    assert!(is_break);
}

#[gpui_kit::test]
async fn keyboard_shortcut_inserts_a_break(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open_app(cx, dir.path());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.press("secondary-b", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(queue_len(&view, cx), 1);
}

#[gpui_kit::test]
async fn delete_key_removes_the_focused_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open_app(cx, dir.path());
    view.update(cx, |view, cx| {
        view.station.update(cx, |station, cx| {
            station.command(cx, |session| session.insert_break(None));
            station.command(cx, |session| session.insert_break(None));
        });
    });
    let first = view.read_with(cx, |view, cx| {
        view.station.read(cx).session().queue().queued()[0]
            .id
            .clone()
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::SharedString::from(format!("queue-item-{first}")),
            cx,
        );
        window.press("backspace", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(queue_len(&view, cx), 1);
    let remaining = view.read_with(cx, |view, cx| {
        view.station.read(cx).session().queue().queued()[0]
            .id
            .clone()
    });
    assert_ne!(remaining, first);
}

fn write_interrupted_recovery(dir: &Path) {
    elizabeats_store::save_recovery(
        dir,
        &RecoverySnapshot {
            version: 1,
            status: RecoveryStatus::Interrupted,
            queue: vec![
                StoredQueueItem::Break {
                    id: "saved-1".into(),
                },
                StoredQueueItem::Break {
                    id: "saved-2".into(),
                },
            ],
            session_started_at: Some("2026-10-06T20:00:00.000Z".into()),
            updated_at: "2026-10-06T21:00:00.000Z".into(),
        },
    )
    .unwrap();
}

#[gpui_kit::test]
async fn recovery_dialog_restores_the_saved_queue(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    write_interrupted_recovery(dir.path());
    let (window, view) = open_app(cx, dir.path());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        window.click("ok", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(queue_len(&view, cx), 2);
    let stopped = view.read_with(cx, |view, cx| {
        view.station.read(cx).session().queue().is_stopped()
    });
    assert!(stopped, "a recovered Queue never starts playing by itself");
}

#[gpui_kit::test]
async fn recovery_dialog_can_start_empty(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    write_interrupted_recovery(dir.path());
    let (window, view) = open_app(cx, dir.path());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.render_frame(cx);
        window.click("cancel", cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(queue_len(&view, cx), 0);
    // The discard is persisted, so the next launch offers nothing.
    view.read_with(cx, |view, cx| {
        view.station
            .read(cx)
            .services()
            .persistence
            .flush_blocking(Duration::from_secs(5));
    });
    assert!(
        elizabeats_store::read_recovery(dir.path())
            .unwrap()
            .is_none()
    );
}

#[gpui_kit::test]
async fn tabs_switch_between_search_and_log(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open_app(cx, dir.path());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.within("workspace-tabs").click(1usize, cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(view.read_with(cx, |view, _| view.tab), 1);
}

#[gpui_kit::test]
async fn local_track_can_be_dragged_from_search_into_the_queue(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    write_wav(&library.path().join("Station ID.wav"));
    elizabeats_store::save_settings(
        dir.path(),
        &AppSettings {
            library_folder: Some(library.path().to_string_lossy().into_owned()),
            ..AppSettings::default()
        },
    )
    .unwrap();
    let (window, view) = open_app(cx, dir.path());

    // The Library scans on a background thread; wait for the search results.
    let mut found = false;
    for _ in 0..100 {
        cx.run_until_parked();
        found = view.read_with(cx, |view, cx| !view.search.read(cx).tracks().is_empty());
        if found {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(found, "local Track never appeared in search results");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        // Search rows span the whole panel (the window is 1440 px, the panel about half).
        let row = window.find(gpui_kit::SharedString::from("search-local:Station ID.wav"));
        let width = f32::from(row.bounds().size.width);
        assert!(width > 600., "search row is only {width}px wide");
        window.drag_to(
            gpui_kit::SharedString::from("search-local:Station ID.wav"),
            "queue-empty",
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(queue_len(&view, cx), 1);
    let title = view.read_with(cx, |view, cx| {
        view.station.read(cx).session().queue().queued()[0]
            .kind
            .clone()
    });
    assert!(matches!(title, ItemKind::Track(track) if track.title == "Station ID"));

    // A queued Track's row is exactly as tall as its search result row.
    let id = view.read_with(cx, |view, cx| {
        view.station.read(cx).session().queue().queued()[0]
            .id
            .clone()
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let height = |id: String| {
            f32::from(
                window
                    .find(gpui_kit::SharedString::from(id))
                    .bounds()
                    .size
                    .height,
            )
        };
        let search = height("search-local:Station ID.wav".to_owned());
        let queue = height(format!("queue-item-{id}"));
        assert_eq!(search, super::RowSize::Regular.height());
        assert_eq!(queue, search, "queue row and search row differ in height");
    })
    .unwrap();
}

/// Plays two short silent Tracks through the real audio engine and checks the
/// Session advances on its own and logs both as completed.
#[gpui_kit::test]
async fn plays_local_tracks_end_to_end(cx: &mut TestAppContext) {
    use elizabeats_core::PlayLogReason;

    // Needs a real sound output; CI machines have none (GitHub sets `CI`).
    if std::env::var_os("CI").is_some() {
        eprintln!("skipped: no audio output device on CI");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let library = tempfile::tempdir().unwrap();
    write_wav(&library.path().join("A.wav"));
    write_wav(&library.path().join("B.wav"));
    elizabeats_store::save_settings(
        dir.path(),
        &AppSettings {
            library_folder: Some(library.path().to_string_lossy().into_owned()),
            crossfade_duration_seconds: 0,
            ..AppSettings::default()
        },
    )
    .unwrap();
    let (_window, view) = open_app(cx, dir.path());

    let wait = |cx: &mut TestAppContext,
                view: &Entity<AppView>,
                what: &str,
                done: &dyn Fn(&AppView, &gpui_kit::App) -> bool| {
        for _ in 0..200 {
            cx.run_until_parked();
            if view.read_with(cx, |view, cx| done(view, cx)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    };
    wait(cx, &view, "library scan", &|view, cx| {
        view.search.read(cx).tracks().len() == 2
    });

    let tracks: Vec<_> = view.read_with(cx, |view, cx| view.search.read(cx).tracks().to_vec());
    view.update(cx, |view, cx| {
        view.station.update(cx, |station, cx| {
            for track in tracks {
                station.command(cx, |session| session.add_track_to_end(track));
            }
        });
    });
    wait(cx, &view, "tracks ready", &|view, cx| {
        view.station
            .read(cx)
            .session()
            .queue()
            .queued()
            .iter()
            .all(|item| item.readiness == elizabeats_core::queue::Readiness::Ready)
    });
    let first = view.read_with(cx, |view, cx| {
        view.station.read(cx).session().queue().queued()[0]
            .id
            .clone()
    });
    view.update(cx, |view, cx| {
        view.station.update(cx, |station, cx| {
            station.command(cx, |session| session.play_item(&first))
        })
    });

    wait(cx, &view, "both tracks to finish", &|view, cx| {
        let session = view.station.read(cx).session();
        session.queue().is_empty()
            && session.play_log().len() == 2
            && session
                .play_log()
                .iter()
                .all(|entry| entry.reason == Some(PlayLogReason::Completed))
    });
    let error = view.read_with(cx, |view, cx| {
        view.station
            .read(cx)
            .session()
            .playback_error()
            .map(str::to_owned)
    });
    assert_eq!(error, None);
}

/// Live: YouTube Music search → Queue → download through youtubei.js with a
/// web view PO token → Ready. Needs `cargo build -p elizabeats` first (the
/// built app binary is the BotGuard helper). Doesn't play anything.
#[gpui_kit::test]
#[ignore = "needs network access to YouTube and a built app binary"]
async fn youtube_track_downloads_and_becomes_ready(cx: &mut TestAppContext) {
    use elizabeats_core::queue::Readiness;

    let dir = tempfile::tempdir().unwrap();
    let helper =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/elizabeats");
    assert!(
        helper.exists(),
        "build the app first: cargo build -p elizabeats"
    );
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        super::bind_keys(cx);
    });
    let parts = bootstrap(Launch {
        data_dir: Some(dir.path().to_path_buf()),
        without_botguard: false,
        helper_program: Some(helper),
    })
    .unwrap();
    let services = parts.services.clone();
    let station = cx.update(|cx| cx.new(|cx| crate::station::Station::new(parts, cx)));

    let youtube = services.youtube.clone();
    let tracks = services
        .runtime
        .block_on(async move { youtube.search("daft punk one more time").await })
        .unwrap();
    let track = tracks.into_iter().next().unwrap();
    println!("queueing {} – {}", track.artist, track.title);
    station.update(cx, |station, cx| {
        station.command(cx, |session| session.add_track_to_end(track))
    });

    let started = std::time::Instant::now();
    loop {
        cx.run_until_parked();
        let (readiness, error) = station.read_with(cx, |station, _| {
            let session = station.session();
            (
                session.queue().queued()[0].readiness,
                session.playback_error().map(str::to_owned),
            )
        });
        match readiness {
            Readiness::Ready => break,
            Readiness::Unavailable => panic!("track unavailable: {error:?}"),
            Readiness::Preparing => {}
        }
        assert!(
            started.elapsed() < Duration::from_secs(120),
            "timed out preparing"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("ready after {:?}", started.elapsed());
    let cached = std::fs::read_dir(dir.path().join("download-cache"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "m4a"))
        .map(|entry| entry.metadata().unwrap().len())
        .collect::<Vec<_>>();
    println!("cached files: {cached:?}");
    assert!(
        cached.iter().any(|len| *len > 1_000_000),
        "expected a full download, got {cached:?}"
    );
}

/// Dropping an Upcoming item above the Active Item is not a valid target:
/// nothing is highlighted and the Queue is unchanged.
#[gpui_kit::test]
async fn cannot_drop_above_the_active_item(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open_app(cx, dir.path());
    view.update(cx, |view, cx| {
        view.station.update(cx, |station, cx| {
            station.command(cx, |session| session.insert_break(None));
            station.command(cx, |session| session.insert_break(None));
        });
    });
    let (active, upcoming) = view.read_with(cx, |view, cx| {
        let queue = view.station.read(cx).session().queue();
        (queue.queued()[0].id.clone(), queue.queued()[1].id.clone())
    });
    // Playing a Break makes it the Active Item without any audio.
    view.update(cx, |view, cx| {
        view.station.update(cx, |station, cx| {
            station.command(cx, |session| session.play_item(&active))
        })
    });
    cx.run_until_parked();

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let target = gpui_kit::SharedString::from(format!("queue-item-{active}"));
        // Drag onto the top edge of the Active row ("above it").
        window.drag_to(
            gpui_kit::SharedString::from(format!("queue-item-{upcoming}")),
            target,
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();
    let (ids, slot) = view.read_with(cx, |view, cx| {
        let ids: Vec<String> = view
            .station
            .read(cx)
            .session()
            .queue()
            .items()
            .map(|item| item.id().to_owned())
            .collect();
        (ids, view.drop_slot)
    });
    assert_eq!(ids, vec![active, upcoming], "the Active Item stays first");
    assert_eq!(slot, None);
}

/// The empty Queue's "Drop here" state follows the cursor: it turns on over
/// the Queue and off again once the cursor leaves without dropping.
#[gpui_kit::test]
async fn drop_target_clears_when_the_cursor_leaves_the_queue(cx: &mut TestAppContext) {
    use gpui_kit::{InputEvent, MouseButton, MouseDownEvent, MouseMoveEvent};
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open_app(cx, dir.path());
    let slot = |cx: &mut TestAppContext| view.read_with(cx, |view, _| view.drop_slot);
    let move_to = |position, window: &mut gpui_kit::Window, cx: &mut gpui_kit::App| {
        let event = MouseMoveEvent {
            position,
            pressed_button: Some(MouseButton::Left),
            modifiers: Default::default(),
        };
        window.dispatch_event(event.to_platform_input(), cx);
        window.render_frame(cx);
    };

    let (start, queue, outside) = cx
        .update_window(window, |_, window, cx| {
            window.render_frame(cx);
            let start = window
                .find(gpui_kit::SharedString::from("add-break-drag"))
                .bounds()
                .center();
            let queue = window
                .find(gpui_kit::SharedString::from("queue-empty"))
                .bounds();
            let outside = gpui_kit::point(queue.right() + gpui_kit::px(200.), queue.center().y);
            let down = MouseDownEvent {
                button: MouseButton::Left,
                position: start,
                modifiers: Default::default(),
                click_count: 1,
                first_mouse: false,
            };
            window.dispatch_event(down.to_platform_input(), cx);
            (start, queue.center(), outside)
        })
        .unwrap();
    cx.update_window(window, |_, window, cx| {
        move_to(
            gpui_kit::point(start.x - gpui_kit::px(20.), start.y + gpui_kit::px(20.)),
            window,
            cx,
        );
        move_to(queue, window, cx);
    })
    .unwrap();
    assert_eq!(slot(cx), Some(0), "over the empty Queue: drop here");

    cx.update_window(window, |_, window, cx| move_to(outside, window, cx))
        .unwrap();
    assert_eq!(slot(cx), None, "left the Queue: no drop target");
}

/// The Add Break button puts the Break right above the selected row, or at
/// the end of the Queue when nothing is selected.
#[gpui_kit::test]
async fn add_break_goes_above_the_selected_row(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open_app(cx, dir.path());
    view.update(cx, |view, cx| {
        view.station.update(cx, |station, cx| {
            station.command(cx, |session| session.insert_break(None));
            station.command(cx, |session| session.insert_break(None));
        });
    });
    let ids = |cx: &mut TestAppContext| {
        view.read_with(cx, |view, cx| {
            let queue = view.station.read(cx).session().queue();
            queue
                .items()
                .map(|item| item.id().to_owned())
                .collect::<Vec<_>>()
        })
    };
    let before = ids(cx);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::SharedString::from(format!("queue-item-{}", before[1])),
            cx,
        );
        window.click("add-break", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let after = ids(cx);
    assert_eq!(after.len(), 3);
    assert_eq!(
        (&after[0], &after[2]),
        (&before[0], &before[1]),
        "new Break above the selection"
    );

    // With nothing selected it goes to the end.
    cx.update_window(window, |_, window, cx| {
        window.press("escape", cx);
        window.render_frame(cx);
        window.click("add-break", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let last = ids(cx);
    assert_eq!(last[..3], after[..], "appended at the end");
}

/// Clicks a Queue row with modifier keys held (Shift, Cmd/Ctrl).
fn click_row(
    window: &mut gpui_kit::Window,
    id: &str,
    modifiers: gpui_kit::Modifiers,
    cx: &mut gpui_kit::App,
) {
    use gpui_kit::{InputEvent, MouseButton, MouseDownEvent, MouseUpEvent};
    window.render_frame(cx);
    let position = window
        .find(gpui_kit::SharedString::from(format!("queue-item-{id}")))
        .bounds()
        .center();
    let down = MouseDownEvent {
        button: MouseButton::Left,
        position,
        modifiers,
        click_count: 1,
        first_mouse: false,
    };
    window.dispatch_event(down.to_platform_input(), cx);
    let up = MouseUpEvent {
        button: MouseButton::Left,
        position,
        modifiers,
        click_count: 1,
    };
    window.dispatch_event(up.to_platform_input(), cx);
    window.render_frame(cx);
}

/// Multi-select: Cmd-click, Shift-click, keyboard moves, dragging together,
/// select all, Escape, Delete, and clicking outside the Queue.
#[gpui_kit::test]
async fn several_queue_items_can_be_selected_and_edited_together(cx: &mut TestAppContext) {
    use gpui_kit::Modifiers;
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open_app(cx, dir.path());
    view.update(cx, |view, cx| {
        view.station.update(cx, |station, cx| {
            for _ in 0..5 {
                station.command(cx, |session| session.insert_break(None));
            }
        });
    });
    let ids = |cx: &mut TestAppContext| {
        view.read_with(cx, |view, cx| {
            let queue = view.station.read(cx).session().queue();
            queue
                .items()
                .map(|item| item.id().to_owned())
                .collect::<Vec<_>>()
        })
    };
    let selected = |cx: &mut TestAppContext| {
        view.read_with(cx, |view, cx| {
            view.selection.ordered(&view.selectable_ids(cx))
        })
    };
    let q = ids(cx);
    let pick = |indices: &[usize]| indices.iter().map(|&i| q[i].clone()).collect::<Vec<_>>();

    // Click, Cmd-click, then Shift-click ranges from the last Cmd-click.
    cx.update_window(window, |_, window, cx| {
        click_row(window, &q[0], Modifiers::none(), cx);
        click_row(window, &q[2], Modifiers::secondary_key(), cx);
    })
    .unwrap();
    assert_eq!(selected(cx), pick(&[0, 2]));
    cx.update_window(window, |_, window, cx| {
        click_row(window, &q[3], Modifiers::shift(), cx)
    })
    .unwrap();
    assert_eq!(selected(cx), pick(&[2, 3]));

    // Cmd-Up moves both one place; again moves them on; at the top, nothing.
    cx.update_window(window, |_, window, cx| window.press("secondary-up", cx))
        .unwrap();
    assert_eq!(ids(cx), pick(&[0, 2, 3, 1, 4]));
    cx.update_window(window, |_, window, cx| window.press("secondary-up", cx))
        .unwrap();
    assert_eq!(ids(cx), pick(&[2, 3, 0, 1, 4]));
    cx.update_window(window, |_, window, cx| window.press("secondary-up", cx))
        .unwrap();
    assert_eq!(ids(cx), pick(&[2, 3, 0, 1, 4]), "blocked at the top");
    assert_eq!(selected(cx), pick(&[2, 3]), "moved items stay selected");

    // Dragging one selected row drags them all, together.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.drag_to(
            gpui_kit::SharedString::from(format!("queue-item-{}", q[3])),
            gpui_kit::SharedString::from(format!("queue-item-{}", q[4])),
            cx,
        );
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(ids(cx), pick(&[0, 1, 2, 3, 4]));

    // Cmd-A selects everything, Escape clears, Shift-Down extends.
    cx.update_window(window, |_, window, cx| {
        click_row(window, &q[1], Modifiers::none(), cx);
        window.press("secondary-a", cx);
    })
    .unwrap();
    assert_eq!(selected(cx), q);
    cx.update_window(window, |_, window, cx| window.press("escape", cx))
        .unwrap();
    assert_eq!(selected(cx), Vec::<String>::new());
    cx.update_window(window, |_, window, cx| {
        click_row(window, &q[1], Modifiers::none(), cx);
        window.press("shift-down", cx);
        window.press("shift-down", cx);
    })
    .unwrap();
    assert_eq!(selected(cx), pick(&[1, 2, 3]));

    // Delete removes them all and leaves nothing selected.
    cx.update_window(window, |_, window, cx| window.press("backspace", cx))
        .unwrap();
    assert_eq!(ids(cx), pick(&[0, 4]));
    assert_eq!(selected(cx), Vec::<String>::new());

    // Right-clicking inside the selection keeps it; outside, selects just that row.
    let right_click = |window: &mut gpui_kit::Window, id: &str, cx: &mut gpui_kit::App| {
        use gpui_kit::{InputEvent, MouseButton, MouseDownEvent};
        window.render_frame(cx);
        let position = window
            .find(gpui_kit::SharedString::from(format!("queue-item-{id}")))
            .bounds()
            .center();
        let down = MouseDownEvent {
            button: MouseButton::Right,
            position,
            modifiers: Modifiers::none(),
            click_count: 1,
            first_mouse: false,
        };
        window.dispatch_event(down.to_platform_input(), cx);
        window.render_frame(cx);
    };
    cx.update_window(window, |_, window, cx| {
        click_row(window, &q[0], Modifiers::none(), cx);
        click_row(window, &q[4], Modifiers::secondary_key(), cx);
        right_click(window, &q[4], cx);
    })
    .unwrap();
    assert_eq!(selected(cx), pick(&[0, 4]));
    cx.update_window(window, |_, window, cx| {
        window.press("escape", cx);
        click_row(window, &q[0], Modifiers::none(), cx);
        right_click(window, &q[4], cx);
    })
    .unwrap();
    assert_eq!(selected(cx), pick(&[4]));
    cx.update_window(window, |_, window, cx| window.press("escape", cx))
        .unwrap();

    // Clicking outside the Queue (here, the Search panel) deselects.
    cx.update_window(window, |_, window, cx| {
        click_row(window, &q[0], Modifiers::none(), cx);
        let elsewhere = gpui_kit::point(gpui_kit::px(1100.), gpui_kit::px(500.));
        window.drag(elsewhere, elsewhere, cx);
    })
    .unwrap();
    assert_eq!(selected(cx), Vec::<String>::new());
}

#[gpui_kit::test]
async fn settings_button_opens_settings(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, _view) = open_app(cx, dir.path());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        // The Library page is shown first, with its folder picker.
        assert!(
            window.try_find("choose-library").is_some(),
            "Settings did not open"
        );
    })
    .unwrap();
}

/// Settings' About page shows the version and, in a dev build, a disabled
/// "Check for Updates" (dev builds never update themselves).
#[gpui_kit::test]
async fn about_page_shows_version_and_dev_builds_dont_update(cx: &mut TestAppContext) {
    use crate::updates::{UpdateStatus, VERSION_LABEL};
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open_app(cx, dir.path());
    assert!(VERSION_LABEL.contains("-dev ("), "{VERSION_LABEL}");
    let status = view.read_with(cx, |view, cx| view.updates.read(cx).status().clone());
    assert_eq!(status, UpdateStatus::DevBuild);

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("open-settings", cx);
    })
    .unwrap();
    cx.run_until_parked();
    // gpui-kit names the sidebar's pages "0-<index>"; About is the fifth.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("0-4", cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("check-for-updates").is_some(),
            "About page not shown"
        );
        // Disabled in a dev build: clicking does nothing.
        window.click("check-for-updates", cx);
    })
    .unwrap();
    cx.run_until_parked();
    let status = view.read_with(cx, |view, cx| view.updates.read(cx).status().clone());
    assert_eq!(status, UpdateStatus::DevBuild);
}

/// Only Upcoming items can be selected: clicking the Active Item does
/// nothing, and the up arrow stops at the first Upcoming item.
#[gpui_kit::test]
async fn only_upcoming_items_can_be_selected(cx: &mut TestAppContext) {
    let dir = tempfile::tempdir().unwrap();
    let (window, view) = open_app(cx, dir.path());
    view.update(cx, |view, cx| {
        view.station.update(cx, |station, cx| {
            station.command(cx, |session| session.insert_break(None));
            station.command(cx, |session| session.insert_break(None));
        });
    });
    let (active, upcoming) = view.read_with(cx, |view, cx| {
        let queue = view.station.read(cx).session().queue();
        (queue.queued()[0].id.clone(), queue.queued()[1].id.clone())
    });
    // Playing a Break makes it the Active Item without any audio.
    view.update(cx, |view, cx| {
        view.station.update(cx, |station, cx| {
            station.command(cx, |session| session.play_item(&active))
        })
    });
    cx.run_until_parked();

    let selected = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, window, cx| view.read(cx).focused_row(window))
            .unwrap()
    };
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::SharedString::from(format!("queue-item-{active}")),
            cx,
        );
    })
    .unwrap();
    assert_eq!(selected(cx), None, "the Active Item can't be selected");

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click(
            gpui_kit::SharedString::from(format!("queue-item-{upcoming}")),
            cx,
        );
        window.press("up", cx);
    })
    .unwrap();
    assert_eq!(
        selected(cx),
        Some(upcoming.clone()),
        "up arrow stays on the first Upcoming item"
    );

    // Only the Active Item's row is a different height.
    cx.update_window(window, |_, window, _cx| {
        let height = |id: &str| {
            f32::from(
                window
                    .find(gpui_kit::SharedString::from(format!("queue-item-{id}")))
                    .bounds()
                    .size
                    .height,
            )
        };
        assert_eq!(height(&active), super::RowSize::Active.height());
        assert_eq!(height(&upcoming), super::RowSize::Regular.height());
    })
    .unwrap();
}

/// gpui-kit dialogs only animate when reduce motion is off, so it is held
/// on exactly while a dialog is open and released afterwards.
#[gpui_kit::test]
async fn reduce_motion_is_only_on_while_a_dialog_is_open(cx: &mut TestAppContext) {
    use gpui_kit::component::WindowExt;

    let dir = tempfile::tempdir().unwrap();
    let (window, _view) = open_app(cx, dir.path());
    let reduced = |cx: &mut TestAppContext| cx.update(|cx| cx.reduce_motion());
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    assert!(!reduced(cx), "no dialog: animations run normally");

    cx.update_window(window, |_, window, cx| {
        window.click("open-settings", cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(reduced(cx), "dialog open: its animation is suppressed");

    cx.update_window(window, |_, window, cx| {
        window.close_dialog(cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(!reduced(cx), "dialog closed: back to normal");
}
