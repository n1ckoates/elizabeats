//! Test stand-in for the app executable.
//!
//! - `elizabeats-botguard-helper --elizabeats-webview-minter [dir]`: the real
//!   webview helper, exactly as the app runs it (see `helper_main_if_requested`).
//! - `elizabeats-botguard-helper --fake-minter <mode> [marker]`: a scripted fake helper
//!   speaking the same line protocol, for testing the app side (timeouts,
//!   restarts) without a webview. Modes:
//!   - `echo`: answers mint requests with fake tokens and resolve requests
//!     with a canned stream; both contain this process's ID (so tests can
//!     tell helpers apart).
//!   - `hang-once <marker file>`: if the marker file does not exist yet,
//!     writes its process ID there and never answers; otherwise echoes.
//!   - `crash-once <marker file>`: like `hang-once`, but exits with code 3
//!     on the first request instead of hanging.
//!   - `junk`: writes a non-protocol line before every echo.
//!   - `error`: answers every request with a (technical) error.
//!   - `unavailable`: echoes mints; answers resolves with a user-facing
//!     error ("This video is unavailable").
//!   - `hang-resolve`: echoes mints; never answers resolves.

use std::io::{BufRead, Write};
use std::time::{SystemTime, UNIX_EPOCH};

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
    let misbehave = match marker {
        Some(path) if !std::path::Path::new(path).exists() => {
            std::fs::write(path, pid.to_string()).is_ok()
        }
        _ => false,
    };
    let mut first = true;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return 1 };
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let id = request["id"].as_u64().unwrap_or_default();
        let is_first = std::mem::replace(&mut first, false);
        let resolve = request["resolve"].as_str();
        let reply = match mode {
            "hang-once" if misbehave && is_first => continue,
            "crash-once" if misbehave && is_first => return 3,
            "hang-resolve" if resolve.is_some() => continue,
            "error" => serde_json::json!({ "id": id, "error": "fake failure" }),
            "unavailable" if resolve.is_some() => serde_json::json!({
                "id": id,
                "error": "This video is unavailable",
                "user_facing": true,
            }),
            _ => match resolve {
                Some(video_id) => {
                    serde_json::json!({ "id": id, "stream": fake_stream(pid, video_id) })
                }
                None => {
                    let tokens: Vec<String> = request["identifiers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|identifier| fake_token(pid, identifier.as_str().unwrap_or_default()))
                        .collect();
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |d| d.as_secs());
                    serde_json::json!({ "id": id, "tokens": tokens, "valid_until": now + 3600 })
                }
            },
        };
        if mode == "junk" {
            let _ = writeln!(stdout, "some library chatter");
        }
        if writeln!(stdout, "{reply}")
            .and_then(|()| stdout.flush())
            .is_err()
        {
            return 1;
        }
    }
    0
}

/// `fake<pid>x<identifier>` padded to the length of a real PO token.
fn fake_token(pid: u32, identifier: &str) -> String {
    let safe: String = identifier
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("fake{pid}x{safe}x{}", "A".repeat(64))
}

/// A stream as the real page reports it; the URL names the process and video.
fn fake_stream(pid: u32, video_id: &str) -> serde_json::Value {
    serde_json::json!({
        "url": format!("https://example.invalid/videoplayback?pid={pid}&v={video_id}"),
        "mime_type": "audio/mp4; codecs=\"mp4a.40.2\"",
        "content_length": 3_433_977,
        "bitrate": "130000",
        "itag": 140,
        "user_agent": "FakeWebView/1.0",
    })
}
