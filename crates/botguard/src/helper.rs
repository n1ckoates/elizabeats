//! Helper mode: the app's own executable, started by the app as
//! `<exe> --elizabeats-webview-minter [<webview data dir>]`, mints PO tokens
//! and resolves audio stream URLs in the operating system's webview
//! (WKWebView on macOS, WebView2 on Windows, WebKitGTK on Linux).
//!
//! YouTube only accepts PO tokens minted by a real browser engine, which is
//! why this runs in a separate process: a webview needs the process's main
//! thread and its own event loop, which the app's UI already owns.
//!
//! How it works:
//! - An invisible window hosts a webview showing
//!   `https://www.youtube.com/robots.txt` (a tiny youtube.com page). Three
//!   scripts are injected at document start: js/bgutils.browser.js,
//!   js/youtube.browser.js (youtubei.js) and js/src/helper-page.js, which
//!   keeps a warm `WebPoMinter` and youtubei.js session.
//! - A thread reads request lines from stdin (see `crate::protocol`) and hands
//!   them to the event loop, which calls `window.__elizabeatsMint(...)` or
//!   `window.__elizabeatsResolve(...)` in the page. The page answers through
//!   wry's IPC channel (`window.ipc.postMessage`) and the reply is written to
//!   stdout.
//! - When stdin closes (the app quit or dropped us) the helper exits, so it
//!   never outlives the app. The app kills it if it stops answering.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::platform::run_return::EventLoopExtRunReturn;
use tao::window::{Window, WindowBuilder};
use wry::{WebContext, WebView, WebViewBuilder};

use crate::protocol::{
    HelperReply, HelperRequest, ReplyOutcome, RequestAction, decode_line, encode_line,
    for_each_line, is_video_id,
};

/// First argument that switches the executable into helper mode.
pub const HELPER_FLAG: &str = "--elizabeats-webview-minter";

const PAGE_URL: &str = "https://www.youtube.com/robots.txt";
// Built from js/ by build.rs into Cargo's output directory.
const BGUTILS_SCRIPT: &str = include_str!(concat!(env!("OUT_DIR"), "/bgutils.browser.js"));
const YOUTUBE_SCRIPT: &str = include_str!(concat!(env!("OUT_DIR"), "/youtube.browser.js"));
const PAGE_SCRIPT: &str = include_str!("../js/src/helper-page.js");
/// Give up (answer every waiting request with an error and exit) when the
/// page has not loaded and set itself up by then, e.g. when offline.
const PAGE_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Runs the webview minter when this process was started as the helper and
/// returns its exit code. Returns `None` for a normal application launch.
///
/// Call this first thing in `main`, before any UI is created: the helper
/// needs the main thread for its own event loop. The flag only counts as the
/// first argument; the second, if any, is the webview's data folder.
pub fn helper_main_if_requested() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != HELPER_FLAG {
        return None;
    }
    Some(run(args.next().map(PathBuf::from)))
}

/// Messages the page sends through `window.ipc.postMessage`.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum PageMessage {
    /// The page has loaded and its script is installed.
    Ready,
    /// The answer to one request, forwarded to the app as is.
    Reply(HelperReply),
    Log {
        message: String,
    },
}

enum UserEvent {
    Request(HelperRequest),
    StdinClosed,
    /// A raw IPC message from the page.
    Page(String),
}

/// The helper page in its webview. When the window or webview could not be
/// created there is none (`Err` with the reason) and every request fails.
struct Page {
    webview: WebView,
    /// Requests that arrived before the page was ready; `None` once it is.
    waiting: Option<Vec<HelperRequest>>,
}

fn run(data_dir: Option<PathBuf>) -> i32 {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        // No Dock icon, no menu bar, never steal focus from the app.
        event_loop.set_activation_policy(ActivationPolicy::Prohibited);
        event_loop.set_dock_visibility(false);
        event_loop.set_activate_ignoring_other_apps(false);
    }
    spawn_stdin_reader(event_loop.create_proxy());

    // The window and context must outlive the webview, so they live here.
    let mut web_context = WebContext::new(data_dir);
    let window = WindowBuilder::new()
        .with_title("Elizabeats PO token minter")
        .with_visible(false)
        .with_focused(false)
        .build(&event_loop)
        .map_err(|error| format!("could not create the helper window: {error}"));
    let mut page = window
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|window| create_webview(window, &mut web_context, event_loop.create_proxy()))
        .map(|webview| Page {
            webview,
            waiting: Some(Vec::new()),
        });
    if let Err(failure) = &page {
        eprintln!("{failure}");
    }

    let ready_deadline = Instant::now() + PAGE_READY_TIMEOUT;
    let code = event_loop.run_return(move |event, _, control_flow| {
        *control_flow = match handle(&mut page, event) {
            Some(code) => ControlFlow::ExitWithCode(code),
            None if matches!(page, Ok(Page { waiting: None, .. })) => ControlFlow::Wait,
            None => ControlFlow::WaitUntil(ready_deadline),
        };
    });
    drop(window);
    code
}

fn create_webview(
    window: &Window,
    web_context: &mut WebContext,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<WebView, String> {
    let builder = WebViewBuilder::new_with_web_context(web_context)
        // Nothing is stored on disk (cookies, caches).
        .with_incognito(true)
        // Main frame only (wry's default). The bundles only publish globals
        // that the page script takes over.
        .with_initialization_script(format!(
            "{BGUTILS_SCRIPT}\n;\n{YOUTUBE_SCRIPT}\n;\n{PAGE_SCRIPT}"
        ))
        .with_ipc_handler(move |request| {
            // Fails only while the event loop shuts down.
            let _ = proxy.send_event(UserEvent::Page(request.into_body()));
        })
        .with_url(PAGE_URL);

    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    let built = {
        // WebKitGTK must be placed in a GTK container (works on X11 and Wayland).
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let container = window
            .default_vbox()
            .ok_or("the helper window has no GTK container")?;
        builder.build_gtk(container)
    };
    #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    let built = builder.build(window);
    built.map_err(|error| format!("could not create the system webview: {error}"))
}

/// Handles one event loop event; returns an exit code to stop.
fn handle(page: &mut Result<Page, String>, event: Event<'_, UserEvent>) -> Option<i32> {
    match (event, page) {
        (Event::UserEvent(UserEvent::StdinClosed), _) => Some(0),
        (Event::UserEvent(UserEvent::Request(request)), Err(failure)) => {
            // Answer, then exit: the app starts a fresh helper next time.
            send_reply(&error_reply(request.id, failure));
            Some(1)
        }
        (Event::UserEvent(UserEvent::Request(request)), Ok(page)) => {
            match &mut page.waiting {
                Some(waiting) => waiting.push(request),
                None => page.dispatch(&request),
            }
            None
        }
        (Event::UserEvent(UserEvent::Page(body)), Ok(page)) => page.on_message(&body),
        (Event::NewEvents(StartCause::ResumeTimeReached { .. }), page) => {
            let (failure, waiting) = match page {
                Ok(Page { waiting: None, .. }) => return None,
                Ok(Page {
                    waiting: Some(waiting),
                    ..
                }) => (
                    "YouTube did not load in the system webview (offline?)",
                    std::mem::take(waiting),
                ),
                Err(failure) => (failure.as_str(), Vec::new()),
            };
            eprintln!("{failure}");
            for request in waiting {
                send_reply(&error_reply(request.id, failure));
            }
            Some(1)
        }
        _ => None,
    }
}

impl Page {
    fn on_message(&mut self, body: &str) -> Option<i32> {
        match decode_line::<PageMessage>(body)? {
            PageMessage::Ready => {
                for request in self.waiting.take().unwrap_or_default() {
                    self.dispatch(&request);
                }
                None
            }
            // Stdout is gone: the app has quit.
            PageMessage::Reply(reply) => (!send_reply(&reply)).then_some(0),
            PageMessage::Log { message } => {
                eprintln!("{message}");
                None
            }
        }
    }

    /// Starts the request in the page; the answer arrives as an IPC message.
    fn dispatch(&self, request: &HelperRequest) {
        let result = page_call(request).and_then(|script| {
            self.webview
                .evaluate_script(&script)
                .map_err(|e| format!("could not run the page script: {e}"))
        });
        if let Err(error) = result {
            send_reply(&error_reply(request.id, &error));
        }
    }
}

/// The JavaScript call that starts a request in the page. Arguments are
/// passed as JSON, which is valid JavaScript, so they cannot inject code.
fn page_call(request: &HelperRequest) -> Result<String, String> {
    let id = request.id;
    let call = match &request.action {
        RequestAction::Mint { identifiers } => serde_json::to_string(identifiers)
            .map(|ids| format!("window.__elizabeatsMint({id}, {ids});")),
        RequestAction::Resolve { resolve } if is_video_id(resolve) => {
            serde_json::to_string(resolve)
                .map(|video_id| format!("window.__elizabeatsResolve({id}, {video_id});"))
        }
        RequestAction::Resolve { resolve } => {
            let shown: String = resolve.chars().take(40).collect();
            return Err(format!("invalid video ID {shown:?}"));
        }
    };
    call.map_err(|e| e.to_string())
}

fn error_reply(id: u64, message: &str) -> HelperReply {
    HelperReply {
        id,
        outcome: ReplyOutcome::Error {
            error: message.to_owned(),
            user_facing: false,
        },
    }
}

/// Writes one reply line to stdout. Returns false when stdout is closed.
fn send_reply(reply: &HelperReply) -> bool {
    let Ok(line) = encode_line(reply) else {
        return true;
    };
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&line)
        .and_then(|()| stdout.flush())
        .is_ok()
}

/// Reads request lines from stdin until it closes (EOF or a broken pipe: the
/// app is gone), then tells the event loop.
fn spawn_stdin_reader(proxy: EventLoopProxy<UserEvent>) {
    let spawned = crate::spawn_thread("minter-stdin", move || {
        for_each_line(std::io::stdin().lock(), |line| {
            match decode_line::<HelperRequest>(line) {
                Some(request) => proxy.send_event(UserEvent::Request(request)).is_ok(),
                None => {
                    eprintln!("ignoring malformed request line");
                    true
                }
            }
        });
        let _ = proxy.send_event(UserEvent::StdinClosed);
    });
    if let Err(error) = spawned {
        eprintln!("could not start the stdin reader: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_messages() {
        let message = |line: &str| decode_line::<PageMessage>(line);
        assert_eq!(message(r#"{"type":"ready"}"#), Some(PageMessage::Ready));
        assert_eq!(
            message(r#"{"type":"log","message":"hi"}"#),
            Some(PageMessage::Log {
                message: "hi".into()
            })
        );
        assert_eq!(
            message(r#"{"type":"reply","id":4,"tokens":["t"],"valid_until":1800000000}"#),
            Some(PageMessage::Reply(HelperReply {
                id: 4,
                outcome: ReplyOutcome::Tokens {
                    tokens: vec!["t".into()],
                    valid_until: 1_800_000_000
                }
            }))
        );
        assert_eq!(
            message(r#"{"type":"reply","id":5,"error":"nope"}"#),
            Some(PageMessage::Reply(error_reply(5, "nope")))
        );
        assert_eq!(message(r#"{"type":"other"}"#), None);
        assert_eq!(message("not json"), None);
    }

    #[test]
    fn replies_pass_through_unchanged() {
        for reply in [
            r#"{"id":6,"stream":{"url":"https://x/","mime_type":"audio/mp4","content_length":5,"bitrate":1,"itag":140,"user_agent":"UA"}}"#,
            r#"{"id":7,"error":"Private video","user_facing":true}"#,
        ] {
            let typed = format!(r#"{{"type":"reply",{}"#, &reply[1..]);
            let Some(PageMessage::Reply(parsed)) = decode_line::<PageMessage>(&typed) else {
                panic!("not a reply: {typed}");
            };
            let line = encode_line(&parsed).unwrap();
            assert_eq!(std::str::from_utf8(&line).unwrap(), format!("{reply}\n"));
        }
    }

    #[test]
    fn page_calls() {
        let request = |action| HelperRequest { id: 3, action };
        assert_eq!(
            page_call(&request(RequestAction::Mint {
                identifiers: vec!["a\"b".into()]
            })),
            Ok(r#"window.__elizabeatsMint(3, ["a\"b"]);"#.to_owned())
        );
        assert_eq!(
            page_call(&request(RequestAction::Resolve {
                resolve: "dQw4w9WgXcQ".into()
            })),
            Ok(r#"window.__elizabeatsResolve(3, "dQw4w9WgXcQ");"#.to_owned())
        );
        assert!(
            page_call(&request(RequestAction::Resolve {
                resolve: "\");alert(1);//".into()
            }))
            .is_err()
        );
    }

    #[test]
    fn injected_scripts_are_embedded() {
        assert!(BGUTILS_SCRIPT.contains("__elizabeatsBgutils"));
        assert!(YOUTUBE_SCRIPT.contains("__elizabeatsYoutube"));
        assert!(PAGE_SCRIPT.contains("__elizabeatsMint"));
        assert!(PAGE_SCRIPT.contains("__elizabeatsResolve"));
    }
}
