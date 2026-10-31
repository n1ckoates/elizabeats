//! The newline-delimited JSON protocol between the app and the webview helper
//! process, plus identifier validation shared by every entry point.
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
//! prewarms: it is answered with no tokens once a minter is ready, and starts
//! the youtubei.js session in the background. `user_facing` errors are
//! YouTube's own readable messages; other errors are technical.
//! Lines that are not valid replies (e.g. stray library output) are ignored.

use std::io::BufRead;
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Deserializer, Serialize};

use crate::{AudioStream, BotguardError, MintedTokens};

/// We mint one identifier at a time; leave generous headroom.
pub(crate) const MAX_IDENTIFIERS: usize = 64;
const MAX_IDENTIFIER_LEN: usize = 256;
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
        stream: StreamInfo,
    },
    Error {
        error: String,
        /// A readable message from YouTube (e.g. "This video is unavailable").
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        user_facing: bool,
    },
}

/// A resolved stream as the page reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StreamInfo {
    pub url: String,
    pub mime_type: String,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub content_length: Option<u64>,
    #[serde(default, deserialize_with = "lenient_u64")]
    pub bitrate: Option<u64>,
    pub itag: u32,
    /// The webview's own `navigator.userAgent`.
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
    /// Checks a reply to a mint request.
    pub fn into_tokens(self, requested: usize) -> Result<MintedTokens, BotguardError> {
        match self.outcome {
            ReplyOutcome::Error { error, user_facing } => Err(into_error(error, user_facing)),
            ReplyOutcome::Stream { .. } => Err(BotguardError::Other(
                "the webview helper answered a mint request with a stream".into(),
            )),
            ReplyOutcome::Tokens {
                tokens,
                valid_until,
            } => {
                if tokens.len() != requested {
                    return Err(BotguardError::Other(format!(
                        "the webview minter returned {} tokens for {requested} identifiers",
                        tokens.len()
                    )));
                }
                if let Some(bad) = tokens.iter().find(|token| !looks_like_po_token(token)) {
                    let shown: String = bad.chars().take(40).collect();
                    return Err(BotguardError::Other(format!(
                        "implausible PO token {shown:?}"
                    )));
                }
                Ok(MintedTokens {
                    tokens,
                    valid_until: UNIX_EPOCH + Duration::from_secs(valid_until),
                })
            }
        }
    }

    /// Checks a reply to a resolve request.
    pub fn into_stream(self) -> Result<AudioStream, BotguardError> {
        match self.outcome {
            ReplyOutcome::Error { error, user_facing } => Err(into_error(error, user_facing)),
            ReplyOutcome::Tokens { .. } => Err(BotguardError::Other(
                "the webview helper answered a resolve request with tokens".into(),
            )),
            ReplyOutcome::Stream { stream } => {
                if !stream.url.starts_with("https://") {
                    return Err(BotguardError::Js("resolved stream URL is not HTTPS".into()));
                }
                // Verified by the live tests: plain ranged GETs with just this
                // User-Agent download whole songs. No Origin/Referer/cookies.
                let headers = stream
                    .user_agent
                    .iter()
                    .map(|user_agent| ("User-Agent".to_owned(), user_agent.clone()))
                    .collect();
                Ok(AudioStream {
                    url: stream.url,
                    mime_type: stream.mime_type,
                    content_length: stream.content_length,
                    bitrate: stream.bitrate,
                    itag: stream.itag,
                    user_agent: stream.user_agent,
                    headers,
                })
            }
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

/// PO tokens are URL-safe base64 protobufs, roughly 100-200 characters.
pub(crate) fn looks_like_po_token(token: &str) -> bool {
    (40..=4096).contains(&token.len())
        && token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '='))
}

/// Reads one `\n`-terminated line, dropping anything past `MAX_LINE_LEN`.
/// Returns false at end of input.
pub(crate) fn read_bounded_line(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
) -> std::io::Result<bool> {
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

/// Identifiers are YouTube video IDs (11 chars of `[A-Za-z0-9_-]`), visitor
/// data (URL-safe base64, often with `%3D` padding) or data sync IDs (digits
/// and `|`). Anything else is rejected before it reaches JavaScript.
pub(crate) fn validate_identifiers(identifiers: &[String]) -> Result<(), BotguardError> {
    if identifiers.is_empty() {
        return Err(BotguardError::InvalidIdentifier(
            "no identifiers given".into(),
        ));
    }
    if identifiers.len() > MAX_IDENTIFIERS {
        return Err(BotguardError::InvalidIdentifier(format!(
            "too many identifiers ({} > {MAX_IDENTIFIERS})",
            identifiers.len()
        )));
    }
    for identifier in identifiers {
        let allowed = |c: char| c.is_ascii_alphanumeric() || "-_%=.|+/:".contains(c);
        if identifier.is_empty()
            || identifier.len() > MAX_IDENTIFIER_LEN
            || !identifier.chars().all(allowed)
        {
            let shown: String = identifier.chars().take(40).collect();
            return Err(BotguardError::InvalidIdentifier(format!("{shown:?}")));
        }
    }
    Ok(())
}

/// A YouTube video ID: 11 characters of `[A-Za-z0-9_-]`.
pub(crate) fn is_video_id(value: &str) -> bool {
    value.len() == 11
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Decodes `%XX` escapes (visitor data arrives URL-encoded, but BotGuard must
/// see the raw value). Malformed escapes are kept as they are.
pub(crate) fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| input.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn accepts_real_identifiers() {
        validate_identifiers(&ids(&[
            "dQw4w9WgXcQ",
            "CgtxRzl5bVZuQl8zcyjF25fWBjIKCgJVUxIEGgAgOw%3D%3D",
            "CgtxRzl5bVZuQl8zcyjF25fWBjIKCgJVUxIEGgAgOw==",
            "113052417227541249867||",
        ]))
        .unwrap();
    }

    #[test]
    fn rejects_bad_identifiers() {
        assert!(validate_identifiers(&[]).is_err());
        assert!(validate_identifiers(&ids(&[""])).is_err());
        assert!(validate_identifiers(&ids(&["has space"])).is_err());
        assert!(validate_identifiers(&ids(&["quote\""])).is_err());
        assert!(validate_identifiers(&ids(&["ünïcode"])).is_err());
        assert!(validate_identifiers(&ids(&["a\nb"])).is_err());
        assert!(validate_identifiers(&["x".repeat(257)]).is_err());
        assert!(validate_identifiers(&vec!["dQw4w9WgXcQ".to_owned(); 65]).is_err());
        validate_identifiers(&vec!["dQw4w9WgXcQ".to_owned(); 64]).unwrap();
    }

    #[test]
    fn video_ids() {
        assert!(is_video_id("dQw4w9WgXcQ"));
        assert!(is_video_id("fa5IWHDbftI"));
        assert!(!is_video_id("dQw4w9WgXc"));
        assert!(!is_video_id("dQw4w9WgXc!"));
    }

    #[test]
    fn decodes_percent_escapes() {
        assert_eq!(percent_decode("abc%3D%3D"), "abc==");
        assert_eq!(percent_decode("a%2Fb"), "a/b");
        assert_eq!(percent_decode("plain"), "plain");
        assert_eq!(percent_decode("bad%zz%4"), "bad%zz%4");
        assert_eq!(percent_decode("%"), "%");
        // Invalid UTF-8 after decoding keeps the original.
        assert_eq!(percent_decode("%FF"), "%FF");
    }

    #[test]
    fn request_wire_format() {
        let request = HelperRequest {
            id: 7,
            action: RequestAction::Mint {
                identifiers: ids(&["dQw4w9WgXcQ"]),
            },
        };
        let line = encode_line(&request).unwrap();
        assert_eq!(line, b"{\"id\":7,\"identifiers\":[\"dQw4w9WgXcQ\"]}\n");
        let parsed: HelperRequest = decode_line(std::str::from_utf8(&line).unwrap()).unwrap();
        assert_eq!(parsed, request);

        let request = HelperRequest {
            id: 8,
            action: RequestAction::Resolve {
                resolve: "dQw4w9WgXcQ".into(),
            },
        };
        let line = encode_line(&request).unwrap();
        assert_eq!(line, b"{\"id\":8,\"resolve\":\"dQw4w9WgXcQ\"}\n");
        let parsed: HelperRequest = decode_line(std::str::from_utf8(&line).unwrap()).unwrap();
        assert_eq!(parsed, request);

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
        let err: HelperReply = decode_line(r#"{"id":2,"error":"m"}"#).unwrap();
        assert_eq!(
            err,
            HelperReply {
                id: 2,
                outcome: ReplyOutcome::Error {
                    error: "m".into(),
                    user_facing: false
                }
            }
        );
        assert_eq!(
            serde_json::to_string(&err).unwrap(),
            r#"{"id":2,"error":"m"}"#
        );
        let readable: HelperReply =
            decode_line(r#"{"id":2,"error":"Gone","user_facing":true}"#).unwrap();
        assert!(matches!(
            readable.outcome,
            ReplyOutcome::Error {
                user_facing: true,
                ..
            }
        ));
        // Extra fields (e.g. the page's "type") are tolerated.
        let typed: HelperReply =
            decode_line(r#"{"type":"reply","id":3,"tokens":[],"valid_until":5}"#).unwrap();
        assert_eq!(typed.id, 3);
    }

    #[test]
    fn stream_replies() {
        let reply: HelperReply = decode_line(
            r#"{"type":"reply","id":4,"stream":{"url":"https://rr1---sn.googlevideo.com/videoplayback?itag=140","mime_type":"audio/mp4; codecs=\"mp4a.40.2\"","content_length":"3433977","bitrate":130000,"itag":140,"user_agent":"UA"}}"#,
        )
        .unwrap();
        let stream = reply.into_stream().unwrap();
        assert_eq!(stream.itag, 140);
        assert_eq!(stream.content_length, Some(3_433_977));
        assert_eq!(stream.bitrate, Some(130_000));
        assert_eq!(stream.user_agent.as_deref(), Some("UA"));
        assert_eq!(
            stream.headers,
            vec![("User-Agent".to_owned(), "UA".to_owned())]
        );

        // Missing optional fields and numeric content lengths.
        let reply: HelperReply = decode_line(
            r#"{"id":5,"stream":{"url":"https://x/","mime_type":"audio/mp4","content_length":12,"bitrate":null,"itag":140}}"#,
        )
        .unwrap();
        let stream = reply.into_stream().unwrap();
        assert_eq!(stream.content_length, Some(12));
        assert_eq!(stream.bitrate, None);
        assert!(stream.headers.is_empty());

        let plain_http: HelperReply = decode_line(
            r#"{"id":6,"stream":{"url":"http://x/","mime_type":"audio/mp4","itag":140}}"#,
        )
        .unwrap();
        assert!(plain_http.into_stream().is_err());

        // Replies of the wrong kind are rejected.
        let tokens: HelperReply = decode_line(r#"{"id":7,"tokens":[],"valid_until":5}"#).unwrap();
        assert!(tokens.into_stream().is_err());
        let stream: HelperReply = decode_line(
            r#"{"id":8,"stream":{"url":"https://x/","mime_type":"audio/mp4","itag":140}}"#,
        )
        .unwrap();
        assert!(stream.into_tokens(0).is_err());
    }

    #[test]
    fn user_facing_errors_become_youtube_errors() {
        let reply = |line: &str| decode_line::<HelperReply>(line).unwrap();
        let error = reply(r#"{"id":1,"error":"This video is unavailable","user_facing":true}"#)
            .into_stream()
            .unwrap_err();
        assert!(
            matches!(&error, BotguardError::Youtube(m) if m == "This video is unavailable"),
            "{error:?}"
        );
        let error = reply(r#"{"id":1,"error":"fetch failed"}"#)
            .into_stream()
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
    fn replies_are_checked_against_the_request() {
        let token = "MnV9_-".repeat(20);
        let reply = |tokens: Vec<String>| HelperReply {
            id: 1,
            outcome: ReplyOutcome::Tokens {
                tokens,
                valid_until: 1_800_000_000,
            },
        };
        let minted = reply(vec![token.clone()]).into_tokens(1).unwrap();
        assert_eq!(minted.tokens, vec![token.clone()]);
        assert_eq!(
            minted.valid_until,
            UNIX_EPOCH + Duration::from_secs(1_800_000_000)
        );
        assert!(reply(vec![token.clone()]).into_tokens(2).is_err());
        assert!(reply(vec!["short".into()]).into_tokens(1).is_err());
        assert!(reply(vec![]).into_tokens(0).unwrap().tokens.is_empty());
        let error = HelperReply {
            id: 1,
            outcome: ReplyOutcome::Error {
                error: "boom".into(),
                user_facing: false,
            },
        };
        assert_eq!(error.into_tokens(1).unwrap_err().to_string(), "boom");
    }

    #[test]
    fn bounded_lines() {
        let long = "x".repeat(MAX_LINE_LEN + 100);
        let input = format!("first\n{long}\nlast");
        let mut reader = std::io::BufReader::with_capacity(1024, input.as_bytes());
        let mut line = Vec::new();
        assert!(read_bounded_line(&mut reader, &mut line).unwrap());
        assert_eq!(line, b"first");
        line.clear();
        assert!(read_bounded_line(&mut reader, &mut line).unwrap());
        assert_eq!(line.len(), MAX_LINE_LEN + 1);
        line.clear();
        assert!(read_bounded_line(&mut reader, &mut line).unwrap());
        assert_eq!(line, b"last");
        line.clear();
        assert!(!read_bounded_line(&mut reader, &mut line).unwrap());
    }

    #[test]
    fn po_token_shape() {
        assert!(looks_like_po_token(&"MnV9_-".repeat(20)));
        assert!(!looks_like_po_token("short"));
        assert!(!looks_like_po_token(&"a+b/".repeat(20)));
    }
}
