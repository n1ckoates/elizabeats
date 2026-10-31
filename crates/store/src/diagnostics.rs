//! `diagnostics.jsonl`: best-effort local diagnostics with secrets redacted.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};
use elizabeats_core::{DiagnosticEvent, DiagnosticSink};
use regex::Regex;
use serde::Serialize;

/// Rotate before the file would grow past this size.
pub const MAX_DIAGNOSTICS_BYTES: u64 = 512 * 1024;
/// Rotated files kept: `diagnostics.jsonl.1` (newest) to `.3` (oldest).
pub const MAX_ROTATED_FILES: u32 = 3;
const MAX_TEXT_CHARS: usize = 500;
/// Events waiting to be written; more than this are dropped.
const QUEUE_CAPACITY: usize = 1024;
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// The patterns from the Electron app, applied in order.
static REDACTIONS: LazyLock<Option<Vec<(Regex, &'static str)>>> = LazyLock::new(|| {
    [
        (
            r#"(?i)(?:https?|elizabeats-(?:audio|local))://[^\s"']+"#,
            "[redacted URL]",
        ),
        (
            r#"(?i)(authorization|proxy-authorization)\s*["']?\s*[:=]\s*["']?(?:bearer|basic)\s+[^,;}"']+"#,
            "${1}=[redacted]",
        ),
        (
            r#"(?i)(cookie|set-cookie|authorization|proxy-authorization|x-[0-9A-Za-z_-]*token|po[-_ ]?token|proof[-_ ]?token|signature|sig)\s*["']?\s*[:=]\s*["']?[^,;\s}"']+"#,
            "${1}=[redacted]",
        ),
        (
            r"(?i)(?-u:\b)(?:cookie|set-cookie|authorization|proxy-authorization)\s+(?:bearer|basic)\s+[^,;\s}]+",
            "[redacted header]",
        ),
    ]
    .into_iter()
    .map(|(pattern, replacement)| Regex::new(pattern).ok().map(|regex| (regex, replacement)))
    .collect()
});

/// Removes URLs, credentials, and tokens from `value`, trims it, and limits
/// it to 500 characters.
pub fn redact(value: &str) -> String {
    let Some(redactions) = REDACTIONS.as_ref() else {
        // Never write unredacted text.
        return "[redaction unavailable]".to_owned();
    };
    let mut text = value.to_owned();
    for (regex, replacement) in redactions {
        text = regex.replace_all(&text, *replacement).into_owned();
    }
    text.trim().chars().take(MAX_TEXT_CHARS).collect()
}

fn sanitize(event: DiagnosticEvent) -> DiagnosticEvent {
    let optional = |value: Option<String>| value.filter(|v| !v.is_empty()).map(|v| redact(&v));
    DiagnosticEvent {
        operation: redact(&event.operation),
        source: event.source,
        video_id: optional(event.video_id),
        client_name: optional(event.client_name),
        status_code: event.status_code,
        error: optional(event.error),
    }
}

#[derive(Serialize)]
struct Line<'a> {
    timestamp: &'a str,
    #[serde(flatten)]
    event: &'a DiagnosticEvent,
}

enum Message {
    Event {
        timestamp: String,
        event: DiagnosticEvent,
    },
    Flush(mpsc::Sender<()>),
}

/// Writes diagnostics on a background thread so recording never blocks
/// playback. Events are dropped if the writer falls far behind, and write
/// failures are ignored.
pub struct DiagnosticsStore {
    sender: SyncSender<Message>,
    path: PathBuf,
}

impl DiagnosticsStore {
    pub fn new(path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let writer_path = path.clone();
        let spawned = std::thread::Builder::new()
            .name("diagnostics".into())
            .spawn(move || run_writer(&writer_path, &receiver));
        if let Err(error) = spawned {
            // Diagnostics are optional; without the thread, events are dropped.
            tracing::warn!("Diagnostics are disabled: {error}");
        }
        Self { sender, path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Queues `event` for writing. Never blocks.
    pub fn record(&self, event: DiagnosticEvent) {
        record_with(&self.sender, event);
    }

    /// Waits (up to two seconds) until every event recorded so far is written.
    pub fn flush(&self) {
        let (done, wait) = mpsc::channel();
        if self.sender.send(Message::Flush(done)).is_ok() {
            let _ = wait.recv_timeout(FLUSH_TIMEOUT);
        }
    }

    /// A cheap handle other crates can use to record events.
    pub fn sink(&self) -> DiagnosticSink {
        let sender = self.sender.clone();
        Arc::new(move |event| record_with(&sender, event))
    }
}

fn record_with(sender: &SyncSender<Message>, event: DiagnosticEvent) {
    let timestamp = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    match sender.try_send(Message::Event { timestamp, event }) {
        Ok(()) | Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {}
    }
}

/// Runs until every sender (the store and all sinks) is gone.
fn run_writer(path: &Path, receiver: &Receiver<Message>) {
    let mut writer = Writer {
        path: path.to_path_buf(),
        file: None,
        size: 0,
    };
    for message in receiver {
        match message {
            Message::Event { timestamp, event } => {
                let event = sanitize(event);
                let line = Line {
                    timestamp: &timestamp,
                    event: &event,
                };
                if let Ok(mut bytes) = serde_json::to_vec(&line) {
                    bytes.push(b'\n');
                    writer.append(&bytes);
                }
            }
            Message::Flush(done) => {
                if let Some(file) = &writer.file {
                    let _ = file.sync_data();
                }
                let _ = done.send(());
            }
        }
    }
}

struct Writer {
    path: PathBuf,
    file: Option<File>,
    /// Current size of `path`, tracked to avoid a `stat` per line.
    size: u64,
}

impl Writer {
    fn append(&mut self, line: &[u8]) {
        let length = line.len() as u64;
        if self.file.is_none() && self.open().is_err() {
            return;
        }
        if self.size > 0 && self.size + length > MAX_DIAGNOSTICS_BYTES {
            self.file = None;
            rotate(&self.path);
            if self.open().is_err() {
                return;
            }
        }
        let written = self.file.as_mut().map(|file| file.write_all(line));
        match written {
            Some(Ok(())) => self.size += length,
            // Reopen next time; the file may have been moved or deleted.
            _ => self.file = None,
        }
    }

    fn open(&mut self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = file.metadata()?.len();
        self.file = Some(file);
        Ok(())
    }
}

/// `x.2` → `x.3`, `x.1` → `x.2`, `x` → `x.1`. The oldest file is replaced.
fn rotate(path: &Path) {
    for index in (0..MAX_ROTATED_FILES).rev() {
        let source = if index == 0 {
            path.to_path_buf()
        } else {
            rotated_path(path, index)
        };
        // Missing older files are expected during the first rotations.
        let _ = fs::rename(source, rotated_path(path, index + 1));
    }
}

fn rotated_path(path: &Path, index: u32) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use elizabeats_core::SourceId;

    use super::*;

    #[test]
    fn all_patterns_compile() {
        assert_eq!(REDACTIONS.as_ref().map(Vec::len), Some(4));
    }

    #[test]
    fn redacts_urls() {
        for (input, expected) in [
            (
                "GET https://rr1.googlevideo.com/videoplayback?sig=abc failed",
                "GET [redacted URL] failed",
            ),
            ("see HTTP://Example.com/a", "see [redacted URL]"),
            ("elizabeats-audio://cache/abc.webm", "[redacted URL]"),
            (
                "Elizabeats-Local://library/My%20Song.mp3 missing",
                "[redacted URL] missing",
            ),
            (r#"url "http://x.y/z" here"#, r#"url "[redacted URL]" here"#),
            ("url 'http://x.y/z' here", "url '[redacted URL]' here"),
            ("elizabeats-app://index.html", "elizabeats-app://index.html"),
            ("ftp://not.redacted", "ftp://not.redacted"),
        ] {
            assert_eq!(redact(input), expected, "{input}");
        }
    }

    #[test]
    fn redacts_authorization_schemes_in_key_values() {
        for (input, expected) in [
            ("authorization: Bearer abc.def", "authorization=[redacted]"),
            (
                "Proxy-Authorization=Basic dXNlcjpwYXNz, next",
                "Proxy-Authorization=[redacted], next",
            ),
            (
                r#"{"authorization":"Bearer abc"}"#,
                r#"{"authorization=[redacted]"}"#,
            ),
        ] {
            assert_eq!(redact(input), expected, "{input}");
        }
    }

    #[test]
    fn redacts_secret_key_values() {
        for (input, expected) in [
            ("cookie=SID=abc; other", "cookie=[redacted]; other"),
            ("Set-Cookie: VISITOR=xyz", "Set-Cookie=[redacted]"),
            ("authorization=token123", "authorization=[redacted]"),
            (
                "x-goog-visitor-token: abc",
                "x-goog-visitor-token=[redacted]",
            ),
            ("X-Youtube-Token=abc,def", "X-Youtube-Token=[redacted],def"),
            ("poToken=abc123", "poToken=[redacted]"),
            ("po_token: abc", "po_token=[redacted]"),
            ("po-token=abc", "po-token=[redacted]"),
            ("po token = abc", "po token=[redacted]"),
            ("proofToken=abc", "proofToken=[redacted]"),
            ("proof_token=abc", "proof_token=[redacted]"),
            ("signature=abc&x=1", "signature=[redacted]"),
            ("sig: 'abc'", "sig=[redacted]'"),
            (r#"{"sig":"abc","n":1}"#, r#"{"sig=[redacted]","n":1}"#),
            // Matches inside longer words too, like the original.
            ("nsig=abc", "nsig=[redacted]"),
        ] {
            assert_eq!(redact(input), expected, "{input}");
        }
    }

    #[test]
    fn redacts_bare_authorization_headers() {
        for (input, expected) in [
            (
                "sent Authorization Bearer abc.def then",
                "sent [redacted header] then",
            ),
            ("cookie basic xyz", "[redacted header]"),
            ("myauthorization bearer abc", "myauthorization bearer abc"),
        ] {
            assert_eq!(redact(input), expected, "{input}");
        }
    }

    #[test]
    fn leaves_ordinary_text_alone_trims_and_truncates() {
        assert_eq!(redact("  Playback failed: 403  "), "Playback failed: 403");
        assert_eq!(redact(&"é".repeat(600)).chars().count(), 500);
    }

    fn read_lines(path: &Path) -> Vec<serde_json::Value> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn writes_sanitized_events_with_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.jsonl");
        let store = DiagnosticsStore::new(path.clone());
        let mut event = DiagnosticEvent::new(" download ")
            .source(SourceId::YoutubeMusic)
            .video_id("abc")
            .client_name("")
            .error("GET https://x.y/z?sig=1 failed");
        event.status_code = Some(403);
        store.record(event);
        (store.sink())(DiagnosticEvent::new("rescan").source(SourceId::Local));
        store.flush();

        let lines = read_lines(&path);
        assert_eq!(lines.len(), 2);
        let first = lines[0].as_object().unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        let positions: Vec<usize> = [
            "timestamp",
            "operation",
            "source",
            "videoId",
            "statusCode",
            "error",
        ]
        .iter()
        .map(|key| raw.find(&format!("\"{key}\"")).unwrap())
        .collect();
        assert!(positions.is_sorted(), "same key order as the Electron app");
        assert!(
            !first.contains_key("clientName"),
            "empty values are omitted"
        );
        assert_eq!(first["operation"], "download");
        assert_eq!(first["source"], "youtube-music");
        assert_eq!(first["statusCode"], 403);
        assert_eq!(first["error"], "GET [redacted URL] failed");
        let timestamp = first["timestamp"].as_str().unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(timestamp).is_ok());
        assert!(
            timestamp.ends_with('Z') && timestamp.len() == 24,
            "{timestamp}"
        );
        assert_eq!(lines[1]["operation"], "rescan");
    }

    #[test]
    fn rotates_and_keeps_three_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diagnostics.jsonl");
        let store = DiagnosticsStore::new(path.clone());
        // ~520-byte lines: about 1000 per file.
        let error = "e".repeat(480);
        for round in 0..5 {
            for index in 0..1100 {
                // Stay under the queue capacity so nothing is dropped.
                if index % 500 == 0 {
                    store.flush();
                }
                store.record(DiagnosticEvent::new(format!("r{round}")).error(error.clone()));
            }
        }
        store.flush();

        assert!(path.exists());
        for index in 1..=3 {
            let rotated = rotated_path(&path, index);
            let size = fs::metadata(&rotated).unwrap().len();
            assert!(
                size <= MAX_DIAGNOSTICS_BYTES && size > MAX_DIAGNOSTICS_BYTES - 1024,
                "{size}"
            );
        }
        assert!(!rotated_path(&path, 4).exists());
        assert!(fs::metadata(&path).unwrap().len() <= MAX_DIAGNOSTICS_BYTES);
        // The newest events are in the live file.
        let last = read_lines(&path).pop().unwrap();
        assert_eq!(last["operation"], "r4");
    }

    #[test]
    fn rotation_without_older_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("d.jsonl");
        fs::write(&path, "old\n").unwrap();
        rotate(&path);
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(rotated_path(&path, 1)).unwrap(), "old\n");
    }

    #[test]
    fn unwritable_location_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        fs::write(&blocker, "x").unwrap();
        let store = DiagnosticsStore::new(blocker.join("diagnostics.jsonl"));
        store.record(DiagnosticEvent::new("x"));
        store.flush();
    }
}
