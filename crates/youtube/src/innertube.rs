//! YouTube Music's own API ("InnerTube") for search and link lookup.
//!
//! Two plain JSON requests with the YouTube Music web client (`WEB_REMIX`):
//! `search` (with the "Songs" filter) and `next` (metadata for one video),
//! falling back to `player` for videos YouTube Music doesn't describe.
//! The parsing mirrors what the Electron app did before it used youtubei.js.

use std::time::Duration;

use elizabeats_core::{SourceId, Track};
use serde_json::{Value, json};

use crate::error::{YoutubeError, redact};
use crate::video_id::is_valid_video_id;

const ORIGIN: &str = "https://music.youtube.com";
/// Search params for YouTube Music's "Songs" filter.
const SONGS_FILTER: &str = "EgWKAQIIAWoKEAoQAxAEEAkQBQ==";
/// Used when the current version can't be read from music.youtube.com.
const FALLBACK_CLIENT_VERSION: &str = "1.20261006.10.00";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const MAX_SEARCH_RESULTS: usize = 20;

pub(crate) struct Innertube {
    http: reqwest::Client,
    /// The web client version, read once from music.youtube.com.
    client_version: tokio::sync::OnceCell<String>,
}

impl Innertube {
    pub(crate) fn new() -> Result<Self, YoutubeError> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(10))
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| YoutubeError::Client {
                detail: redact(&error.without_url().to_string()),
            })?;
        Ok(Self {
            http,
            client_version: tokio::sync::OnceCell::new(),
        })
    }

    /// YouTube Music song search: at most 20 Tracks.
    pub(crate) async fn search_tracks(&self, query: &str) -> Result<Vec<Track>, YoutubeError> {
        let response = self
            .post("search", json!({ "query": query, "params": SONGS_FILTER }))
            .await
            .map_err(|detail| YoutubeError::SearchFailed { detail })?;
        Ok(parse_search(&response))
    }

    /// Metadata for one video (a pasted link).
    pub(crate) async fn track_for_video(&self, video_id: &str) -> Result<Track, YoutubeError> {
        let next = self
            .post("next", json!({ "videoId": video_id }))
            .await
            .map_err(|detail| YoutubeError::Player { detail })?;
        if let Some(track) = parse_next(&next, video_id) {
            return Ok(track);
        }
        let player = self
            .post("player", json!({ "videoId": video_id }))
            .await
            .map_err(|detail| YoutubeError::Player { detail })?;
        parse_player(&player, video_id)
    }

    /// Looks up the client version (and opens the connection to YouTube Music)
    /// ahead of time, so the first search doesn't wait for it (~0.7 s).
    pub(crate) async fn warm_up(&self) {
        let _ = self.client_version().await;
    }

    async fn client_version(&self) -> String {
        if let Some(version) = self.client_version.get() {
            return version.clone();
        }
        match self.fetch_client_version().await {
            Some(version) => self
                .client_version
                .get_or_init(|| async { version })
                .await
                .clone(),
            // Not cached, so the next request tries the page again.
            None => FALLBACK_CLIENT_VERSION.to_owned(),
        }
    }

    async fn fetch_client_version(&self) -> Option<String> {
        let html = self.http.get(ORIGIN).send().await.ok()?.text().await.ok()?;
        client_version_from_html(&html)
    }

    /// POSTs to `youtubei/v1/<endpoint>`; errors are redacted detail strings.
    async fn post(&self, endpoint: &str, mut body: Value) -> Result<Value, String> {
        let version = self.client_version().await;
        body["context"] = json!({
            "client": { "clientName": "WEB_REMIX", "clientVersion": version, "hl": "en", "gl": "US" }
        });
        let response = self
            .http
            .post(format!("{ORIGIN}/youtubei/v1/{endpoint}?prettyPrint=false"))
            .header("Content-Type", "application/json")
            .header("Origin", ORIGIN)
            .header("X-Origin", ORIGIN)
            .header("X-YouTube-Client-Name", "67")
            .header("X-YouTube-Client-Version", &version)
            .body(body.to_string())
            .send()
            .await
            .map_err(|error| redact(&error.without_url().to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("{endpoint}: HTTP {}", status.as_u16()));
        }
        let text = response
            .text()
            .await
            .map_err(|error| format!("{endpoint}: {}", redact(&error.without_url().to_string())))?;
        serde_json::from_str(&text).map_err(|error| format!("{endpoint}: invalid JSON ({error})"))
    }
}

pub(crate) fn client_version_from_html(html: &str) -> Option<String> {
    let key = "\"INNERTUBE_CONTEXT_CLIENT_VERSION\":\"";
    let start = html.find(key)? + key.len();
    let version = &html[start..start + html[start..].find('"')?];
    (!version.is_empty() && version.bytes().all(|b| b.is_ascii_digit() || b == b'.'))
        .then(|| version.to_owned())
}

// ---- Parsing ---------------------------------------------------------------

fn text(value: &Value) -> Option<&str> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn runs(value: &Value) -> Vec<&Value> {
    value["runs"]
        .as_array()
        .map(|runs| runs.iter().collect())
        .unwrap_or_default()
}

/// Text runs that carry content (not the " • " separators).
fn meaningful_runs(value: &Value) -> Vec<&Value> {
    runs(value)
        .into_iter()
        .filter(|run| text(&run["text"]).is_some_and(|text| text != "•"))
        .collect()
}

fn page_type(run: &Value) -> &str {
    run["navigationEndpoint"]["browseEndpoint"]["browseEndpointContextSupportedConfigs"]
        ["browseEndpointContextMusicConfig"]["pageType"]
        .as_str()
        .unwrap_or_default()
}

/// `m:ss` or `h:mm:ss` → seconds.
pub(crate) fn parse_duration(value: &str) -> Option<f64> {
    let parts: Vec<&str> = value.trim().split(':').collect();
    if !(2..=3).contains(&parts.len())
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let seconds = parts.iter().try_fold(0u64, |total, part| {
        Some(total * 60 + part.parse::<u64>().ok()?)
    })?;
    // A real track length always fits exactly in an f64.
    #[allow(clippy::cast_precision_loss)]
    Some(seconds as f64)
}

fn is_explicit(badges: &Value) -> bool {
    badges.as_array().is_some_and(|badges| {
        badges.iter().any(|badge| {
            badge["musicInlineBadgeRenderer"]["icon"]["iconType"].as_str()
                == Some("MUSIC_EXPLICIT_BADGE")
        })
    })
}

fn artwork(thumbnails: &Value) -> Option<String> {
    let sizes: Vec<(&str, u32, u32)> = thumbnails
        .as_array()?
        .iter()
        .filter_map(|thumbnail| {
            let url = thumbnail["url"].as_str()?;
            let width = u32::try_from(thumbnail["width"].as_u64().unwrap_or(0)).unwrap_or(0);
            let height = u32::try_from(thumbnail["height"].as_u64().unwrap_or(0)).unwrap_or(0);
            Some((url, width, height))
        })
        .collect();
    best_artwork(&sizes)
}

/// Artist, album and duration from a byline like `Artist • Album • 3:21`.
struct Byline {
    artist: Option<String>,
    album: Option<String>,
    duration: Option<f64>,
}

fn byline(value: &Value) -> Byline {
    let runs = meaningful_runs(value);
    let artist = runs
        .iter()
        .find(|run| page_type(run).contains("ARTIST"))
        .or_else(|| runs.first())
        .and_then(|run| text(&run["text"]))
        .map(str::to_owned);
    let album = runs
        .iter()
        .find(|run| page_type(run).contains("ALBUM"))
        .and_then(|run| text(&run["text"]))
        .map(str::to_owned);
    let duration = runs
        .iter()
        .rev()
        .find_map(|run| text(&run["text"]).and_then(parse_duration));
    Byline {
        artist,
        album,
        duration,
    }
}

fn search_item(item: &Value) -> Option<Track> {
    let columns = item["flexColumns"].as_array()?;
    let column = |index: usize| {
        columns
            .get(index)
            .map(|column| &column["musicResponsiveListItemFlexColumnRenderer"]["text"])
    };
    let title_run = runs(column(0)?).into_iter().next()?;
    let id = title_run["navigationEndpoint"]["watchEndpoint"]["videoId"].as_str()?;
    let title = text(&title_run["text"])?;
    if !is_valid_video_id(id) {
        return None;
    }
    let byline = column(1).map(byline).unwrap_or(Byline {
        artist: None,
        album: None,
        duration: None,
    });
    Some(Track {
        id: id.to_owned(),
        source: SourceId::YoutubeMusic,
        title: title.to_owned(),
        artist: byline.artist.unwrap_or_else(|| "Unknown artist".to_owned()),
        album: byline.album,
        duration_seconds: byline.duration,
        artwork_url: artwork(
            &item["thumbnail"]["musicThumbnailRenderer"]["thumbnail"]["thumbnails"],
        ),
        explicit: is_explicit(&item["badges"]),
    })
}

/// Visits every object under `value` named `key`, depth first.
fn collect<'a>(value: &'a Value, key: &str, found: &mut Vec<&'a Value>) {
    match value {
        Value::Object(map) => {
            if let Some(item) = map.get(key) {
                found.push(item);
                return;
            }
            for child in map.values() {
                collect(child, key, found);
            }
        }
        Value::Array(items) => {
            for child in items {
                collect(child, key, found);
            }
        }
        _ => {}
    }
}

/// Songs from a search response, in order, without duplicates (the top
/// result is usually repeated in the Songs list).
pub(crate) fn parse_search(response: &Value) -> Vec<Track> {
    let mut items = Vec::new();
    collect(response, "musicResponsiveListItemRenderer", &mut items);
    let mut tracks: Vec<Track> = Vec::new();
    for track in items.into_iter().filter_map(search_item) {
        if !tracks.iter().any(|existing| existing.id == track.id) {
            tracks.push(track);
        }
        if tracks.len() == MAX_SEARCH_RESULTS {
            break;
        }
    }
    tracks
}

/// The requested video's entry in a `next` response's play queue.
pub(crate) fn parse_next(response: &Value, video_id: &str) -> Option<Track> {
    let mut items = Vec::new();
    collect(response, "playlistPanelVideoRenderer", &mut items);
    let item = items
        .into_iter()
        .find(|item| item["videoId"].as_str() == Some(video_id))?;
    let title = meaningful_runs(&item["title"])
        .into_iter()
        .find_map(|run| text(&run["text"]))?;
    let byline = byline(&item["longBylineText"]);
    let duration = meaningful_runs(&item["lengthText"])
        .into_iter()
        .find_map(|run| text(&run["text"]).and_then(parse_duration))
        .or(byline.duration);
    Some(Track {
        id: video_id.to_owned(),
        source: SourceId::YoutubeMusic,
        title: title.to_owned(),
        artist: byline.artist.unwrap_or_else(|| "Unknown artist".to_owned()),
        album: byline.album,
        duration_seconds: duration,
        artwork_url: artwork(&item["thumbnail"]["thumbnails"]),
        explicit: is_explicit(&item["badges"]),
    })
}

/// Fallback metadata from a `player` response's video details.
pub(crate) fn parse_player(response: &Value, video_id: &str) -> Result<Track, YoutubeError> {
    let details = &response["videoDetails"];
    let Some(title) =
        text(&details["title"]).filter(|_| details["videoId"].as_str() == Some(video_id))
    else {
        let status = response["playabilityStatus"]["status"]
            .as_str()
            .unwrap_or_default();
        let reason = text(&response["playabilityStatus"]["reason"]);
        return Err(match (status, reason) {
            ("ERROR" | "LOGIN_REQUIRED" | "UNPLAYABLE", Some(reason)) => {
                YoutubeError::Unavailable {
                    message: reason.to_owned(),
                    detail: format!("player: {status}"),
                }
            }
            _ => YoutubeError::IncompleteMetadata,
        });
    };
    // Auto-generated music channels are called "<Artist> - Topic".
    let artist = text(&details["author"])
        .map(|name| name.trim_end_matches(" - Topic").trim())
        .filter(|name| !name.is_empty())
        .unwrap_or("Unknown artist");
    #[allow(clippy::cast_precision_loss)]
    let duration = details["lengthSeconds"]
        .as_str()
        .and_then(|seconds| seconds.parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(|seconds| seconds as f64);
    Ok(Track {
        id: video_id.to_owned(),
        source: SourceId::YoutubeMusic,
        title: title.to_owned(),
        artist: artist.to_owned(),
        album: None,
        duration_seconds: duration,
        artwork_url: artwork(&details["thumbnail"]["thumbnails"]),
        explicit: false,
    })
}

// ---- Artwork ---------------------------------------------------------------

fn best_artwork(sizes: &[(&str, u32, u32)]) -> Option<String> {
    let url = choose_artwork(sizes)?;
    // YouTube Music lists only 60/120 px covers, but its image server renders
    // any size: ask for 544 px instead.
    let small_square = sizes
        .iter()
        .any(|(u, w, h)| *u == url && w == h && *w < 226);
    Some(if small_square {
        resize_cover(url, 544)
    } else {
        url.to_owned()
    })
}

/// `https://…googleusercontent.com/…=w120-h120-l90-rj` → `…=w544-h544-l90-rj`.
/// Other URLs are returned unchanged.
fn resize_cover(url: &str, size: u32) -> String {
    let Some((base, params)) = url.rsplit_once('=') else {
        return url.to_owned();
    };
    let is_google_image = base
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|host| host.ends_with(".googleusercontent.com"));
    if !is_google_image || base.contains('?') {
        return url.to_owned();
    }
    let mut changed = false;
    let params: Vec<String> = params
        .split('-')
        .map(|param| {
            let is_size = param.len() > 1
                && (param.starts_with('w') || param.starts_with('h'))
                && param[1..].bytes().all(|b| b.is_ascii_digit());
            if is_size {
                changed = true;
                format!("{}{size}", &param[..1])
            } else {
                param.to_owned()
            }
        })
        .collect();
    if changed {
        format!("{base}={}", params.join("-"))
    } else {
        url.to_owned()
    }
}

/// Prefers a square cover 226–544 px wide (the biggest such), then the
/// smallest image at least 226 px on both sides, then the biggest overall.
fn choose_artwork<'a>(sizes: &[(&'a str, u32, u32)]) -> Option<&'a str> {
    let area = |(_, w, h): &&(&str, u32, u32)| u64::from(*w) * u64::from(*h);
    let usable = sizes
        .iter()
        .filter(|(url, _, _)| url.starts_with("https://"));
    usable
        .clone()
        .filter(|(_, w, h)| (226..=544).contains(w) && w.abs_diff(*h) <= w / 20)
        .max_by_key(area)
        .or_else(|| {
            usable
                .clone()
                .filter(|(_, w, h)| (*w).min(*h) >= 226)
                .min_by_key(area)
        })
        .or_else(|| usable.max_by_key(area))
        .map(|(url, _, _)| *url)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    /// Live timing of search: `cargo test --release -p elizabeats-youtube
    /// search_timing -- --ignored --nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs network"]
    async fn search_timing() {
        let innertube = Innertube::new().unwrap();
        let started = std::time::Instant::now();
        let version = innertube.fetch_client_version().await;
        println!(
            "client version page: {:>4} ms ({version:?})",
            started.elapsed().as_millis()
        );
        // As in the app: warm up at launch, then the DJ types a query.
        let innertube = Innertube::new().unwrap();
        innertube.warm_up().await;
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        for query in [
            "daft punk one more time",
            "kendrick lamar humble",
            "taylor swift",
            "olivia rodrigo good 4 u",
            "macklemore thrift shop",
        ] {
            let started = std::time::Instant::now();
            let tracks = innertube.search_tracks(query).await.unwrap();
            println!(
                "{query:<26} {:>4} ms ({} tracks)",
                started.elapsed().as_millis(),
                tracks.len()
            );
        }
    }

    fn run(text: &str, page_type: Option<&str>) -> Value {
        match page_type {
            Some(page_type) => json!({
                "text": text,
                "navigationEndpoint": {"browseEndpoint": {"browseEndpointContextSupportedConfigs": {
                    "browseEndpointContextMusicConfig": {"pageType": page_type}}}}
            }),
            None => json!({ "text": text }),
        }
    }

    fn song(id: &str, title: &str, explicit: bool) -> Value {
        json!({"musicResponsiveListItemRenderer": {
            "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                {"url": "https://lh3.googleusercontent.com/c=w60-h60-l90-rj", "width": 60, "height": 60},
                {"url": "https://lh3.googleusercontent.com/c=w120-h120-l90-rj", "width": 120, "height": 120}
            ]}}},
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                    {"text": title, "navigationEndpoint": {"watchEndpoint": {"videoId": id}}}
                ]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                    run("Kendrick Lamar", Some("MUSIC_PAGE_TYPE_ARTIST")),
                    run(" • ", None),
                    run("DAMN.", Some("MUSIC_PAGE_TYPE_ALBUM")),
                    run(" • ", None),
                    run("2:57", None)
                ]}}}
            ],
            "badges": if explicit {
                json!([{"musicInlineBadgeRenderer": {"icon": {"iconType": "MUSIC_EXPLICIT_BADGE"}}}])
            } else {
                json!(null)
            }
        }})
    }

    #[test]
    fn parses_search_results_with_explicit_badges() {
        let response = json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"content": {"sections": [
            {"musicShelfRenderer": {"contents": [
                song("tvTRZJ-4EyI", "HUMBLE.", true),
                song("tvTRZJ-4EyI", "HUMBLE.", true),
                song("hRK7PVJFbS8", "King Kunta", false)
            ]}}
        ]}}]}}});
        let tracks = parse_search(&response);
        assert_eq!(tracks.len(), 2, "duplicates removed");
        let humble = &tracks[0];
        assert_eq!(humble.id, "tvTRZJ-4EyI");
        assert_eq!(humble.title, "HUMBLE.");
        assert_eq!(humble.artist, "Kendrick Lamar");
        assert_eq!(humble.album.as_deref(), Some("DAMN."));
        assert_eq!(humble.duration_seconds, Some(177.0));
        assert!(humble.explicit);
        assert!(!tracks[1].explicit);
        assert_eq!(
            humble.artwork_url.as_deref(),
            Some("https://lh3.googleusercontent.com/c=w544-h544-l90-rj")
        );
    }

    #[test]
    fn skips_items_without_a_video() {
        let response = json!([{"musicResponsiveListItemRenderer": {"flexColumns": [
            {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "An album"}]}}}
        ]}}]);
        assert!(parse_search(&response).is_empty());
        assert!(parse_search(&json!({})).is_empty());
    }

    #[test]
    fn parses_next_for_a_pasted_link() {
        let response = json!({"contents": {"queue": [{"playlistPanelVideoRenderer": {
            "videoId": "fa5IWHDbftI",
            "title": {"runs": [{"text": "One More Time"}]},
            "longBylineText": {"runs": [
                run("Daft Punk", None), run(" • ", None), run("Discovery", None), run(" • ", None), run("2001", None)
            ]},
            "lengthText": {"runs": [{"text": "5:21"}]},
            "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/x.jpg", "width": 544, "height": 544}]}
        }}]}});
        let track = parse_next(&response, "fa5IWHDbftI").unwrap();
        assert_eq!(track.title, "One More Time");
        assert_eq!(track.artist, "Daft Punk");
        assert_eq!(track.duration_seconds, Some(321.0));
        assert_eq!(
            track.artwork_url.as_deref(),
            Some("https://i.ytimg.com/x.jpg")
        );
        assert!(parse_next(&response, "dQw4w9WgXcQ").is_none());
    }

    #[test]
    fn player_fallback_and_unavailable_videos() {
        let response = json!({"videoDetails": {
            "videoId": "dQw4w9WgXcQ", "title": "Never Gonna Give You Up", "author": "Rick Astley - Topic",
            "lengthSeconds": "213", "thumbnail": {"thumbnails": []}
        }});
        let track = parse_player(&response, "dQw4w9WgXcQ").unwrap();
        assert_eq!(track.artist, "Rick Astley");
        assert_eq!(track.duration_seconds, Some(213.0));

        let gone = json!({"playabilityStatus": {"status": "ERROR", "reason": "This video is unavailable"}});
        assert_eq!(
            parse_player(&gone, "dQw4w9WgXcQ").unwrap_err().to_string(),
            "This video is unavailable"
        );
        assert_eq!(
            parse_player(&json!({}), "dQw4w9WgXcQ").unwrap_err(),
            YoutubeError::IncompleteMetadata
        );
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("2:57"), Some(177.0));
        assert_eq!(parse_duration("1:02:03"), Some(3723.0));
        assert_eq!(parse_duration("2001"), None);
        assert_eq!(parse_duration("1:2:3:4"), None);
        assert_eq!(parse_duration("a:bc"), None);
    }

    #[test]
    fn reads_client_version_from_page() {
        let html = r#"...,"INNERTUBE_CONTEXT_CLIENT_VERSION":"1.20261006.10.00","x":1"#;
        assert_eq!(
            client_version_from_html(html).as_deref(),
            Some("1.20261006.10.00")
        );
        assert_eq!(client_version_from_html("nothing here"), None);
        assert_eq!(
            client_version_from_html(r#""INNERTUBE_CONTEXT_CLIENT_VERSION":"<script>""#),
            None
        );
    }

    #[test]
    fn chooses_square_cover_of_reasonable_size() {
        let sizes = [
            ("https://a/60", 60, 60),
            ("https://a/120", 120, 120),
            ("https://a/226", 226, 226),
            ("https://a/544", 544, 544),
            ("https://a/1200", 1200, 1200),
        ];
        assert_eq!(choose_artwork(&sizes), Some("https://a/544"));
        let video = [
            ("https://v/default", 120, 90),
            ("https://v/hq", 480, 360),
            ("https://v/max", 1280, 720),
        ];
        assert_eq!(choose_artwork(&video), Some("https://v/hq"));
        assert_eq!(choose_artwork(&[("http://insecure", 300, 300)]), None);
        assert_eq!(choose_artwork(&[]), None);
    }

    #[test]
    fn resizes_small_google_covers() {
        assert_eq!(
            resize_cover(
                "https://yt3.googleusercontent.com/abc_D-e=w120-h120-l90-rj",
                544
            ),
            "https://yt3.googleusercontent.com/abc_D-e=w544-h544-l90-rj"
        );
        for unchanged in [
            "https://i.ytimg.com/vi/x/hqdefault.jpg?sqp=a=b",
            "https://evil.com/googleusercontent.com=w120-h120",
            "https://yt3.googleusercontent.com/abc",
        ] {
            assert_eq!(resize_cover(unchanged, 544), unchanged);
        }
    }
}
