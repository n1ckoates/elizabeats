//! Test stand-in for the app executable.
//!
//! - `elizabeats-botguard-helper --elizabeats-webview-minter [dir]`: the real
//!   webview helper, exactly as the app runs it (see `helper_main_if_requested`).
//! - `elizabeats-botguard-helper --fake-minter <mode> [marker]`: a scripted fake
//!   helper speaking the same line protocol, for testing the app side
//!   (timeouts, restarts) without a webview. It writes a non-protocol line
//!   before every reply, which the app must ignore. Modes:
//!   - `echo`: answers mints with no tokens and resolves with a canned stream
//!     whose URL contains this process's ID (so tests can tell helpers apart).
//!   - `hang-once <marker file>`: if the marker file does not exist yet,
//!     writes its process ID there and never answers its first resolve
//!     request; otherwise echoes. Mints (prewarming) are always answered.
//!     Every helper started with a marker also appends its process ID to
//!     `<marker>.started` once it's running, so tests can wait for it.
//!   - `crash-once <marker file>`: like `hang-once`, but exits with code 3
//!     on the first resolve request instead of hanging.
//!   - `error`: answers every request with a (technical) error.
//!   - `unavailable`: echoes mints; answers resolves with a user-facing
//!     error ("This video is unavailable").

use std::io::{BufRead, Write};

use serde_json::json;

fn main() {
    if let Some(code) = elizabeats_botguard::helper_main_if_requested() {
        std::process::exit(code);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, mode, rest @ ..] = args.as_slice()
        && flag == "--fake-minter"
    {
        std::process::exit(fake_minter(mode, rest.first().map(String::as_str)));
    }
    eprintln!(
        "usage: elizabeats-botguard-helper --elizabeats-webview-minter [dir] | --fake-minter <mode> [marker]"
    );
    std::process::exit(2);
}

fn fake_minter(mode: &str, marker: Option<&str>) -> i32 {
    let pid = std::process::id();
    // In the *-once modes only the first helper process misbehaves.
    let mut misbehave = match marker {
        Some(path) if !std::path::Path::new(path).exists() => {
            std::fs::write(path, pid.to_string()).is_ok()
        }
        _ => false,
    };
    if let Some(path) = marker {
        let started = format!("{path}.started");
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(started);
        if let Ok(file) = &mut file {
            let _ = writeln!(file, "{pid}");
        }
    }
    let mut stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { return 1 };
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let id = request["id"].as_u64().unwrap_or_default();
        let resolve = request["resolve"].as_str();
        let reply = match (mode, resolve) {
            ("hang-once", Some(_)) if std::mem::take(&mut misbehave) => continue,
            ("crash-once", Some(_)) if misbehave => return 3,
            ("error", _) => json!({ "id": id, "error": "fake failure" }),
            ("unavailable", Some(_)) => json!({
                "id": id,
                "error": "This video is unavailable",
                "user_facing": true,
            }),
            (_, Some(video_id)) => json!({
                "id": id,
                "stream": {
                    "url": format!("https://example.invalid/videoplayback?pid={pid}&v={video_id}"),
                    "mime_type": "audio/mp4; codecs=\"mp4a.40.2\"",
                    "content_length": 3_433_977,
                    "bitrate": "130000",
                    "itag": 140,
                    "user_agent": "FakeWebView/1.0",
                },
            }),
            (_, None) => json!({ "id": id, "tokens": [], "valid_until": 0 }),
        };
        if writeln!(stdout, "some library chatter\n{reply}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            return 1;
        }
    }
    0
}
