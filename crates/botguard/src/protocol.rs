//! The newline-delimited JSON protocol between the app and the webview helper
//! process.
//!
//! The app writes requests to the helper's stdin and reads replies from its
//! stdout, one JSON object per line. Requests carry an `id` that the reply
//! repeats, so several requests may be in flight at once:
//!
//! ```text
//! → {"id":1,"identifiers":["dQw4w9WgXcQ"]}          mint PO tokens
//! ← {"id":1,"tokens":["Mn…"],"valid_until":1791400000}
//! → {"id":2,"resolve":"dQw4w9WgXcQ"}                resolve an audio stream
//! ← {"id":2,"stream":{"url":"https://…","mime_type":"audio/mp4; codecs=\"mp4a.40.2\"",
//!     "content_length":3433977,"bitrate":130000,"itag":140,"user_agent":"Mozilla/5.0 …"}}
//! ← {"id":3,"error":"This video is unavailable","user_facing":true}
//! ```
//!
//! `valid_until` is a Unix timestamp in seconds. An empty `identifiers` list
//! prewarms (the only mint the app sends): it is answered with no tokens once
//! a minter is ready, and starts the youtubei.js session in the background.
//! `user_facing` errors are YouTube's own readable messages; other errors are
//! technical. Lines that are not valid replies (e.g. stray library output)
//! are ignored.

use std::io::{BufRead, BufReader, Read};

use serde::{Deserialize, Deserializer, Serialize};

use crate::BotguardError;

/// Upper bound for one request/reply line, in bytes.
pub(crate) const MAX_LINE_LEN: usize = 256 * 1024;

/// App → helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HelperRequest {
    pub id: u64,
    #[serde(flatten)]
    pub action: RequestAction,
}

/// What a request asks for; told apart by its field name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum RequestAction {
    /// One PO token per identifier (none: prewarm).
    Mint { identifiers: Vec<String> },
    /// The best AAC audio stream of this video ID.
    Resolve { resolve: String },
}

/// Helper → app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HelperReply {
    pub id: u64,
    #[serde(flatten)]
    pub outcome: ReplyOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum ReplyOutcome {
    Tokens {
        tokens: Vec<String>,
        valid_until: u64,
    },
    Stream {
        stream: AudioStream,
    },
    Error {
        error: String,
        /// A readable message from YouTube (e.g. "This video is unavailable").
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        user_facing: bool,
    },
}

/// A directly downloadable audio stream (always AAC in MP4), as the page
/// reports it.
///
/// Verified by the live tests: plain ranged GETs (chunks of at most 10 MB
/// using the `&range=start-end` URL parameter, like YouTube's own player)
/// download whole songs, sending at most `user_agent`. No
/// Origin/Referer/cookies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioStream {
    pub url: String,
    /// e.g. `audio/mp4; codecs="mp4a.40.2"`.
    pub mime_type: String,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub content_length: Option<u64>,
    /// Bits per second.
    #[serde(default, deserialize_with = "lenient_u64")]
    pub bitrate: Option<u64>,
    pub itag: u32,
    /// The webview's own `navigator.userAgent`, for the download requests.
    #[serde(default)]
    pub user_agent: Option<String>,
}

/// Accepts a number, a numeric string (youtubei.js uses both) or null.
fn lenient_u64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64)),
        serde_json::Value::String(text) => text.trim().parse().ok(),
        _ => None,
    })
}

/// Serialises one protocol message as a line (with the trailing newline).
pub(crate) fn encode_line(message: &impl Serialize) -> Result<Vec<u8>, BotguardError> {
    let mut line = serde_json::to_vec(message).map_err(|e| BotguardError::Other(e.to_string()))?;
    line.push(b'\n');
    Ok(line)
}

/// Parses one line; `None` for anything that is not a well-formed message.
pub(crate) fn decode_line<T: for<'de> Deserialize<'de>>(line: &str) -> Option<T> {
    let line = line.trim();
    if line.is_empty() || line.len() > MAX_LINE_LEN {
        return None;
    }
    serde_json::from_str(line).ok()
}

impl HelperReply {
    /// Checks the reply to a prewarm: anything but an error means ready.
    pub fn into_prewarmed(self) -> Result<(), BotguardError> {
        match self.outcome {
            ReplyOutcome::Error { error, user_facing } => Err(into_error(error, user_facing)),
            _ => Ok(()),
        }
    }

    /// Checks the reply to a resolve request.
    pub fn into_stream(self) -> Result<AudioStream, BotguardError> {
        match self.outcome {
            ReplyOutcome::Error { error, user_facing } => Err(into_error(error, user_facing)),
            ReplyOutcome::Tokens { .. } => Err(BotguardError::Other(
                "the webview helper answered a resolve request with tokens".into(),
            )),
            ReplyOutcome::Stream { stream } if !stream.url.starts_with("https://") => Err(
                BotguardError::Other("the resolved stream URL is not HTTPS".into()),
            ),
            ReplyOutcome::Stream { stream } => Ok(stream),
        }
    }
}

/// User-facing messages become [`BotguardError::Youtube`], which the app
/// shows as they are; everything else is a technical error.
fn into_error(message: String, user_facing: bool) -> BotguardError {
    if user_facing {
        BotguardError::Youtube(message)
    } else {
        BotguardError::Other(message)
    }
}

/// Calls `f` with each line of `reader` (lossily decoded; anything past
/// `MAX_LINE_LEN` dropped) until the input ends or fails, or `f` returns false.
pub(crate) fn for_each_line(reader: impl Read, mut f: impl FnMut(&str) -> bool) {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    while let Ok(true) = read_bounded_line(&mut reader, &mut line) {
        if !f(&String::from_utf8_lossy(&line)) {
            return;
        }
        line.clear();
    }
}

/// Reads one `\n`-terminated line, dropping anything past `MAX_LINE_LEN`.
/// Returns false at end of input.
fn read_bounded_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> std::io::Result<bool> {
    let mut read_any = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(read_any);
        }
        read_any = true;
        let (chunk, done) = match buffer.iter().position(|&b| b == b'\n') {
            Some(end) => (&buffer[..end], Some(end + 1)),
            None => (buffer, None),
        };
        let room = (MAX_LINE_LEN + 1).saturating_sub(line.len());
        line.extend_from_slice(&chunk[..chunk.len().min(room)]);
        let consumed = done.unwrap_or(buffer.len());
        reader.consume(consumed);
        if done.is_some() {
            return Ok(true);
        }
    }
}

/// A YouTube video ID: 11 characters of `[A-Za-z0-9_-]`. Anything else is
/// rejected before it reaches JavaScript.
pub(crate) fn is_video_id(value: &str) -> bool {
    value.len() == 11
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(line: &str) -> HelperReply {
        decode_line(line).unwrap()
    }

    #[test]
    fn video_ids() {
        assert!(is_video_id("dQw4w9WgXcQ"));
        assert!(is_video_id("fa5IWHDbftI"));
        assert!(!is_video_id("dQw4w9WgXc"));
        assert!(!is_video_id("dQw4w9WgXc!"));
    }

    #[test]
    fn request_wire_format() {
        for (action, expected) in [
            (
                RequestAction::Mint {
                    identifiers: vec!["dQw4w9WgXcQ".into()],
                },
                "{\"id\":7,\"identifiers\":[\"dQw4w9WgXcQ\"]}\n",
            ),
            (
                RequestAction::Resolve {
                    resolve: "dQw4w9WgXcQ".into(),
                },
                "{\"id\":7,\"resolve\":\"dQw4w9WgXcQ\"}\n",
            ),
        ] {
            let request = HelperRequest { id: 7, action };
            let line = encode_line(&request).unwrap();
            assert_eq!(std::str::from_utf8(&line).unwrap(), expected);
            assert_eq!(decode_line::<HelperRequest>(expected), Some(request));
        }
        assert_eq!(decode_line::<HelperRequest>(r#"{"id":9}"#), None);
        assert_eq!(
            decode_line::<HelperRequest>(r#"{"id":9,"resolve":5}"#),
            None
        );
    }

    #[test]
    fn reply_wire_format() {
        let ok = HelperReply {
            id: 1,
            outcome: ReplyOutcome::Tokens {
                tokens: vec!["a".into(), "b".into()],
                valid_until: 42,
            },
        };
        assert_eq!(
            serde_json::to_string(&ok).unwrap(),
            r#"{"id":1,"tokens":["a","b"],"valid_until":42}"#
        );
        let err = reply(r#"{"id":2,"error":"m"}"#);
        assert_eq!(
            err.outcome,
            ReplyOutcome::Error {
                error: "m".into(),
                user_facing: false
            }
        );
        assert_eq!(
            serde_json::to_string(&err).unwrap(),
            r#"{"id":2,"error":"m"}"#
        );
        // Extra fields (e.g. the page's "type") are tolerated.
        let typed = reply(r#"{"type":"reply","id":3,"tokens":[],"valid_until":5}"#);
        assert_eq!(typed.id, 3);
        typed.into_prewarmed().unwrap();
    }

    #[test]
    fn stream_replies() {
        let stream = reply(
            r#"{"type":"reply","id":4,"stream":{"url":"https://rr1---sn.googlevideo.com/videoplayback?itag=140","mime_type":"audio/mp4; codecs=\"mp4a.40.2\"","content_length":"3433977","bitrate":130000,"itag":140,"user_agent":"UA"}}"#,
        )
        .into_stream()
        .unwrap();
        assert_eq!(stream.itag, 140);
        assert_eq!(stream.content_length, Some(3_433_977));
        assert_eq!(stream.bitrate, Some(130_000));
        assert_eq!(stream.user_agent.as_deref(), Some("UA"));

        // Missing optional fields and numeric content lengths.
        let stream = reply(
            r#"{"id":5,"stream":{"url":"https://x/","mime_type":"audio/mp4","content_length":12,"bitrate":null,"itag":140}}"#,
        )
        .into_stream()
        .unwrap();
        assert_eq!(stream.content_length, Some(12));
        assert_eq!(stream.bitrate, None);
        assert_eq!(stream.user_agent, None);

        let plain_http =
            reply(r#"{"id":6,"stream":{"url":"http://x/","mime_type":"audio/mp4","itag":140}}"#);
        assert!(plain_http.into_stream().is_err());
        // Replies of the wrong kind are rejected.
        let tokens = reply(r#"{"id":7,"tokens":[],"valid_until":5}"#);
        assert!(tokens.into_stream().is_err());
    }

    #[test]
    fn user_facing_errors_become_youtube_errors() {
        let error = reply(r#"{"id":1,"error":"This video is unavailable","user_facing":true}"#)
            .into_stream()
            .unwrap_err();
        assert!(
            matches!(&error, BotguardError::Youtube(m) if m == "This video is unavailable"),
            "{error:?}"
        );
        let error = reply(r#"{"id":1,"error":"fetch failed"}"#)
            .into_prewarmed()
            .unwrap_err();
        assert!(matches!(error, BotguardError::Other(_)), "{error:?}");
    }

    #[test]
    fn junk_lines_are_ignored() {
        for line in [
            "",
            "   ",
            "hello from some library",
            "{}",
            r#"{"id":1}"#,
            r#"{"id":"1","error":"x"}"#,
            r#"{"id":1,"tokens":"x","valid_until":1}"#,
            r#"{"id":-1,"error":"x"}"#,
        ] {
            assert_eq!(decode_line::<HelperReply>(line), None, "{line:?}");
        }
        let huge = format!(r#"{{"id":1,"error":"{}"}}"#, "x".repeat(MAX_LINE_LEN));
        assert_eq!(decode_line::<HelperReply>(&huge), None);
    }

    #[test]
    fn bounded_lines() {
        let long = "x".repeat(MAX_LINE_LEN + 100);
        let input = format!("first\n{long}\nlast");
        let mut lines = Vec::new();
        for_each_line(input.as_bytes(), |line| {
            lines.push(line.to_owned());
            true
        });
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "first");
        assert_eq!(lines[1].len(), MAX_LINE_LEN + 1);
        assert_eq!(lines[2], "last");
    }
}
