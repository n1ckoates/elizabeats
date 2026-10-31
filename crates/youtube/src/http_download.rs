//! Fetching a video's audio: youtubei.js + BotGuard (in the system webview)
//! resolve the stream, which is then downloaded in chunks into a `.part` file.
//!
//! YouTube throttles a single long-running request, so (like YouTube's own
//! web player) we fetch at most ~10 MB per request using the `range=` URL
//! parameter. When the length is unknown we fall back to standard HTTP
//! `Range` headers. Each chunk has its own timeout and a few retries for
//! transient failures; a 403 is never retried because it means YouTube
//! rejected the request (usually a PO token / BotGuard problem).

use std::time::Duration;

use elizabeats_botguard::{BotguardError, BotguardService};
use reqwest::{StatusCode, header};

use crate::cache_files::PartFile;
use crate::error::{YoutubeError, http_detail, redact};
use crate::{AudioFetcher, BoxFuture};

/// The real [`AudioFetcher`]. YouTube Music songs only download in full
/// through the YouTube Music client with a PO token minted by a real browser
/// engine, which is what youtubei.js + BotGuard (system webview) provide.
pub(crate) struct StreamFetcher {
    /// Downloads need it; without it (e.g. in tests) they fail clearly.
    botguard: Option<BotguardService>,
    /// Compression is disabled so the bytes we count are exactly the file's.
    http: reqwest::Client,
}

impl StreamFetcher {
    pub(crate) fn new(botguard: Option<BotguardService>) -> Result<Self, YoutubeError> {
        Ok(Self {
            botguard,
            http: http_client(
                reqwest::Client::builder()
                    .no_gzip()
                    .no_brotli()
                    .connect_timeout(Duration::from_secs(15)),
            )?,
        })
    }

    async fn fetch_audio(&self, video_id: &str, part: &mut PartFile) -> Result<(), YoutubeError> {
        let botguard = self.botguard.as_ref().ok_or_else(|| YoutubeError::Client {
            detail: "BotGuard service is not running".to_owned(),
        })?;
        let unsupported = YoutubeError::UnsupportedFormat.to_string();
        let stream =
            botguard
                .resolve_audio_stream(video_id)
                .await
                .map_err(|error| match error {
                    BotguardError::Youtube(message) if message == unsupported => {
                        YoutubeError::UnsupportedFormat
                    }
                    BotguardError::Youtube(message) => YoutubeError::Unavailable {
                        message,
                        detail: "youtubei.js playability".to_owned(),
                    },
                    other => YoutubeError::Player {
                        detail: redact(&other.to_string()),
                    },
                })?;
        if !stream
            .mime_type
            .to_ascii_lowercase()
            .starts_with("audio/mp4")
        {
            return Err(YoutubeError::UnsupportedFormat);
        }
        tracing::info!(
            "downloading {video_id}: itag {} ({:?} bytes)",
            stream.itag,
            stream.content_length
        );
        let request = StreamRequest {
            url: &stream.url,
            user_agent: stream.user_agent.as_deref().unwrap_or_default(),
            content_length: stream.content_length,
        };
        download_stream(&self.http, &request, part, &DownloadLimits::default()).await
    }
}

impl AudioFetcher for StreamFetcher {
    fn fetch<'a>(
        &'a self,
        video_id: &'a str,
        part: &'a mut PartFile,
    ) -> BoxFuture<'a, Result<(), YoutubeError>> {
        Box::pin(self.fetch_audio(video_id, part))
    }
}

/// Builds an HTTP client, with a user-facing error if that fails.
pub(crate) fn http_client(
    builder: reqwest::ClientBuilder,
) -> Result<reqwest::Client, YoutubeError> {
    builder.build().map_err(|error| YoutubeError::Client {
        detail: http_detail(error),
    })
}

#[derive(Clone, Debug)]
pub(crate) struct DownloadLimits {
    /// Upper bound of bytes per request. Requests use 90–100 % of this.
    pub max_chunk_bytes: u64,
    pub chunk_timeout: Duration,
    /// Extra attempts per chunk after the first one fails transiently.
    pub max_retries: u32,
    /// Delay before the first retry; doubled for each further retry.
    pub retry_delay: Duration,
}

impl Default for DownloadLimits {
    fn default() -> Self {
        Self {
            max_chunk_bytes: 10_000_000,
            chunk_timeout: Duration::from_secs(45),
            max_retries: 3,
            retry_delay: Duration::from_secs(1),
        }
    }
}

pub(crate) struct StreamRequest<'a> {
    pub url: &'a str,
    pub user_agent: &'a str,
    /// Exact stream size in bytes, when YouTube told us.
    pub content_length: Option<u64>,
}

/// Downloads the whole stream into `part`. On error the caller drops `part`,
/// which deletes the temporary file.
pub(crate) async fn download_stream(
    http: &reqwest::Client,
    request: &StreamRequest<'_>,
    part: &mut PartFile,
    limits: &DownloadLimits,
) -> Result<(), YoutubeError> {
    let mut total = request.content_length.filter(|len| *len > 0);
    let use_url_param = total.is_some();
    let mut offset: u64 = 0;

    loop {
        let chunk_len = chunk_size(limits.max_chunk_bytes);
        let mut end = offset + chunk_len - 1;
        if let Some(total) = total {
            end = end.min(total - 1);
        }
        let start = offset;
        let requested = end - start + 1;
        let chunk =
            fetch_chunk_with_retries(http, request, start, end, use_url_param, limits).await?;
        let received = chunk.body.len() as u64;
        if received == 0 {
            if start > 0 && chunk.total == Some(start) {
                // A range past the end of a file of unknown size: it's complete.
                total = Some(start);
                break;
            }
            return Err(incomplete(format!("empty response at byte {start}")));
        }
        part.write_all(&chunk.body)
            .await
            .map_err(YoutubeError::cache_write)?;
        offset += received;
        if total.is_none() {
            total = chunk.total;
        }

        let finished = match total {
            Some(total) => offset >= total,
            // No total known: a whole-file response or a short final chunk ends it.
            None => chunk.whole_file || received < requested,
        };
        if finished {
            break;
        }
    }

    if let Some(total) = total
        && (offset != total || part.written() != total)
    {
        return Err(incomplete(format!("received {offset} of {total} bytes")));
    }
    Ok(())
}

struct Chunk {
    body: Vec<u8>,
    /// Total size learned from `Content-Range`, if any.
    total: Option<u64>,
    /// The server ignored our range and sent the whole file.
    whole_file: bool,
}

enum Failure {
    Retry(YoutubeError),
    Fatal(YoutubeError),
}

async fn fetch_chunk_with_retries(
    http: &reqwest::Client,
    request: &StreamRequest<'_>,
    start: u64,
    end: u64,
    use_url_param: bool,
    limits: &DownloadLimits,
) -> Result<Chunk, YoutubeError> {
    let mut attempt = 0;
    loop {
        let result = tokio::time::timeout(
            limits.chunk_timeout,
            fetch_chunk(http, request, start, end, use_url_param),
        )
        .await
        .unwrap_or(Err(Failure::Retry(YoutubeError::DownloadTimedOut)));
        match result {
            Ok(chunk) => return Ok(chunk),
            Err(Failure::Fatal(error)) => return Err(error),
            Err(Failure::Retry(error)) => {
                if attempt >= limits.max_retries {
                    return Err(error);
                }
                tracing::warn!(
                    "retrying chunk {start}-{end} after: {}",
                    error.diagnostic_message()
                );
                tokio::time::sleep(limits.retry_delay * 2u32.saturating_pow(attempt)).await;
                attempt += 1;
            }
        }
    }
}

async fn fetch_chunk(
    http: &reqwest::Client,
    request: &StreamRequest<'_>,
    start: u64,
    end: u64,
    use_url_param: bool,
) -> Result<Chunk, Failure> {
    let mut builder = if use_url_param {
        let mut url = reqwest::Url::parse(request.url)
            .map_err(|_| Failure::Fatal(incomplete("invalid stream URL".into())))?;
        url.query_pairs_mut()
            .append_pair("range", &format!("{start}-{end}"));
        http.get(url)
    } else {
        http.get(request.url)
            .header(header::RANGE, format!("bytes={start}-{end}"))
    };
    builder = builder
        .header(header::USER_AGENT, request.user_agent)
        .header(header::ORIGIN, "https://www.youtube.com")
        .header(header::REFERER, "https://www.youtube.com/");

    let mut response = builder.send().await.map_err(network_failure)?;
    let status = response.status();
    if status == StatusCode::FORBIDDEN {
        return Err(Failure::Fatal(YoutubeError::DownloadForbidden));
    }
    if !use_url_param && status == StatusCode::RANGE_NOT_SATISFIABLE && start > 0 {
        // We asked past the end of a file of unknown size: it is complete.
        return Ok(Chunk {
            body: Vec::new(),
            total: Some(start),
            whole_file: true,
        });
    }
    if !status.is_success() {
        let error = YoutubeError::DownloadHttp {
            status: status.as_u16(),
        };
        let transient = status.is_server_error()
            || status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::TOO_MANY_REQUESTS;
        return Err(if transient {
            Failure::Retry(error)
        } else {
            Failure::Fatal(error)
        });
    }

    let (total, whole_file) = if use_url_param {
        (None, false)
    } else if status == StatusCode::PARTIAL_CONTENT {
        let total = response
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .and_then(parse_content_range_total);
        (total, false)
    } else if start == 0 {
        (response.content_length(), true)
    } else {
        return Err(Failure::Fatal(incomplete(
            "server ignored the requested range".into(),
        )));
    };

    let requested = end - start + 1;
    let limit = if whole_file { u64::MAX } else { requested };
    let mut body = Vec::with_capacity(usize::try_from(requested.min(16 << 20)).unwrap_or(0));
    while let Some(bytes) = response.chunk().await.map_err(network_failure)? {
        if body.len() as u64 + bytes.len() as u64 > limit {
            return Err(Failure::Fatal(incomplete(
                "server sent more data than requested".into(),
            )));
        }
        body.extend_from_slice(&bytes);
    }
    Ok(Chunk {
        body,
        total,
        whole_file,
    })
}

/// `bytes 0-99/1234` → `Some(1234)`; `bytes 0-99/*` → `None`.
fn parse_content_range_total(value: &str) -> Option<u64> {
    value.rsplit_once('/')?.1.trim().parse().ok()
}

fn network_failure(error: reqwest::Error) -> Failure {
    Failure::Retry(if error.is_timeout() {
        YoutubeError::DownloadTimedOut
    } else {
        YoutubeError::DownloadNetwork {
            detail: http_detail(error),
        }
    })
}

fn incomplete(detail: String) -> YoutubeError {
    YoutubeError::DownloadIncomplete { detail }
}

/// 90–100 % of `max`, varied per request like YouTube's own player.
fn chunk_size(max: u64) -> u64 {
    let min = (max / 10 * 9).max(1);
    crate::random_between(min, max.max(min))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;
    use crate::cache_files::CacheFiles;

    /// A tiny HTTP/1.1 server for download tests. Behaviour is chosen by path:
    /// `/ok`, `/header` (Range headers, no length given up front), `/forbidden`,
    /// `/flaky` (first request 500), `/broken` (always 503), `/short`
    /// (drops the last bytes), `/hang` (never answers).
    struct TestServer {
        base: String,
        requests: Arc<Mutex<Vec<String>>>,
    }

    fn test_body(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    impl TestServer {
        async fn start(body: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let log = requests.clone();
            let body = Arc::new(body);
            tokio::spawn(async move {
                while let Ok((mut socket, _)) = listener.accept().await {
                    let body = body.clone();
                    let log = log.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        let mut tmp = [0u8; 1024];
                        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            match socket.read(&mut tmp).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                            }
                        }
                        let text = String::from_utf8_lossy(&buf).into_owned();
                        let target = text.split(' ').nth(1).unwrap_or_default().to_owned();
                        let headers: HashMap<String, String> = text
                            .lines()
                            .skip(1)
                            .filter_map(|l| l.split_once(':'))
                            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
                            .collect();
                        let count = {
                            let mut log = log.lock().unwrap();
                            log.push(target.clone());
                            log.len()
                        };
                        let response = respond(&target, &headers, &body, count);
                        if let Some(response) = response {
                            let _ = socket.write_all(&response).await;
                        } else {
                            tokio::time::sleep(Duration::from_secs(3600)).await;
                        }
                    });
                }
            });
            Self { base, requests }
        }

        fn request_log(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }

        async fn download(
            &self,
            path: &str,
            content_length: Option<u64>,
        ) -> (Result<u64, YoutubeError>, tempfile::TempDir) {
            download(
                &format!("{}{path}", self.base),
                content_length,
                &fast_limits(),
            )
            .await
        }
    }

    fn respond(
        target: &str,
        headers: &HashMap<String, String>,
        body: &[u8],
        count: usize,
    ) -> Option<Vec<u8>> {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let reply = |status: &str, extra: &str, payload: &[u8]| {
            let mut out = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
                payload.len()
            )
            .into_bytes();
            out.extend_from_slice(payload);
            out
        };
        let parse_range = |text: &str| -> Option<(usize, usize)> {
            let (a, b) = text.split_once('-')?;
            Some((a.parse().ok()?, b.parse().ok()?))
        };
        match path {
            "/forbidden" => return Some(reply("403 Forbidden", "", b"no")),
            "/broken" => return Some(reply("503 Service Unavailable", "", b"")),
            "/flaky" if count == 1 => return Some(reply("500 Internal Server Error", "", b"")),
            "/hang" => return None,
            _ => {}
        }
        let body = if path == "/short" {
            &body[..body.len() - 10]
        } else {
            body
        };
        if let Some(range) = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("range="))
            .and_then(parse_range)
        {
            let end = range.1.min(body.len().saturating_sub(1));
            let slice = body.get(range.0..=end).unwrap_or_default();
            return Some(reply("200 OK", "", slice));
        }
        if let Some((start, end)) = headers
            .get("range")
            .and_then(|r| r.strip_prefix("bytes="))
            .and_then(parse_range)
        {
            if start >= body.len() {
                return Some(reply("416 Range Not Satisfiable", "", b""));
            }
            let end = end.min(body.len() - 1);
            let extra = format!("Content-Range: bytes {start}-{end}/{}\r\n", body.len());
            return Some(reply("206 Partial Content", &extra, &body[start..=end]));
        }
        Some(reply("200 OK", "", body))
    }

    fn fast_limits() -> DownloadLimits {
        DownloadLimits {
            max_chunk_bytes: 1000,
            chunk_timeout: Duration::from_millis(500),
            max_retries: 2,
            retry_delay: Duration::from_millis(10),
        }
    }

    /// Downloads `url` into a fresh cache folder, as the cache does.
    async fn download(
        url: &str,
        content_length: Option<u64>,
        limits: &DownloadLimits,
    ) -> (Result<u64, YoutubeError>, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let files = CacheFiles::new(temp.path().to_owned());
        let http = http_client(reqwest::Client::builder()).unwrap();
        let request = StreamRequest {
            url,
            user_agent: "test",
            content_length,
        };
        let mut part = files.begin("abcdefghijk.m4a").await.unwrap();
        let result = match download_stream(&http, &request, &mut part, limits).await {
            Ok(()) => part.finish().await.map_err(YoutubeError::cache),
            Err(error) => Err(error),
        };
        (result, temp)
    }

    fn dir_names(temp: &tempfile::TempDir) -> Vec<String> {
        std::fs::read_dir(temp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn downloads_in_chunks_with_range_param() {
        let body = test_body(4500);
        let server = TestServer::start(body.clone()).await;
        let (result, temp) = server.download("/ok?sig=1", Some(4500)).await;
        assert_eq!(result, Ok(4500));
        let saved = std::fs::read(temp.path().join("abcdefghijk.m4a")).unwrap();
        assert_eq!(saved, body);
        let log = server.request_log();
        assert!(log.len() >= 5, "chunked: {log:?}");
        assert!(log.iter().all(|t| t.starts_with("/ok?sig=1&range=")));
        assert_eq!(dir_names(&temp), vec!["abcdefghijk.m4a"]);
    }

    #[tokio::test]
    async fn downloads_with_range_headers_when_length_unknown() {
        let body = test_body(2500);
        let server = TestServer::start(body.clone()).await;
        let (result, temp) = server.download("/header", None).await;
        assert_eq!(result, Ok(2500));
        let saved = std::fs::read(temp.path().join("abcdefghijk.m4a")).unwrap();
        assert_eq!(saved, body);
    }

    #[tokio::test]
    async fn length_mismatch_fails_and_cleans_up() {
        let server = TestServer::start(test_body(3000)).await;
        let (result, temp) = server.download("/short", Some(3000)).await;
        assert!(
            matches!(result, Err(YoutubeError::DownloadIncomplete { .. })),
            "{result:?}"
        );
        assert!(dir_names(&temp).is_empty(), "{:?}", dir_names(&temp));
    }

    #[tokio::test]
    async fn forbidden_is_not_retried() {
        let server = TestServer::start(test_body(100)).await;
        let (result, temp) = server.download("/forbidden", Some(100)).await;
        assert_eq!(result, Err(YoutubeError::DownloadForbidden));
        assert_eq!(server.request_log().len(), 1);
        assert!(dir_names(&temp).is_empty());
    }

    #[tokio::test]
    async fn server_errors_are_retried() {
        let body = test_body(1500);
        let server = TestServer::start(body.clone()).await;
        let (result, _temp) = server.download("/flaky", Some(1500)).await;
        assert_eq!(result, Ok(1500));

        let server = TestServer::start(body).await;
        let (result, temp) = server.download("/broken", Some(1500)).await;
        assert_eq!(result, Err(YoutubeError::DownloadHttp { status: 503 }));
        assert_eq!(server.request_log().len(), 3, "1 try + 2 retries");
        assert!(dir_names(&temp).is_empty());
    }

    #[tokio::test]
    async fn chunk_timeout_fails_and_cleans_up() {
        let server = TestServer::start(test_body(100)).await;
        let (result, temp) = server.download("/hang", Some(100)).await;
        assert_eq!(result, Err(YoutubeError::DownloadTimedOut));
        assert_eq!(server.request_log().len(), 3);
        assert!(dir_names(&temp).is_empty());
    }

    #[tokio::test]
    async fn network_errors_are_reported_without_urls() {
        // Nothing listens on this port.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        // Windows keeps retrying a refused connection for about 2 s before
        // failing, so allow longer than that (or it reports a timeout).
        let limits = DownloadLimits {
            chunk_timeout: Duration::from_secs(5),
            ..fast_limits()
        };
        let url = format!("http://{address}/ok?pot=SECRET");
        let (result, _temp) = download(&url, Some(100), &limits).await;
        match result {
            Err(error @ YoutubeError::DownloadNetwork { .. }) => {
                assert!(!error.diagnostic_message().contains("SECRET"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_content_range() {
        assert_eq!(parse_content_range_total("bytes 0-9/1234"), Some(1234));
        assert_eq!(parse_content_range_total("bytes 0-9/*"), None);
        let size = chunk_size(10_000_000);
        assert!((9_000_000..=10_000_000).contains(&size));
    }
}
