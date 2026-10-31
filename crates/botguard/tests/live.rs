//! Live tests against YouTube with the real webview helper. Run with:
//! `cargo test -p elizabeats-botguard --test live -- --ignored --nocapture --test-threads=1`

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use elizabeats_botguard::{AudioStream, BotguardService, HELPER_FLAG, HelperOptions};

/// Test stand-in for the app executable (calls `helper_main_if_requested`).
const HELPER: &str = env!("CARGO_BIN_EXE_elizabeats-botguard-helper");
/// YouTube Music songs that need the YTMUSIC client + a real PO token.
const SONGS: [&str; 2] = ["fa5IWHDbftI", "wU26xVT_vBU"];
/// Our downloader uses 10 MB chunks (like YouTube's own player); smaller
/// ones here so these ~5 MB songs take several ranged requests.
const CHUNK: u64 = 2 * 1024 * 1024;

fn start_service() -> (tempfile::TempDir, BotguardService) {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let options = HelperOptions::webview(HELPER, dir.path());
    let service = BotguardService::start_with_helper(options).unwrap();
    (dir, service)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs network access to YouTube and a system webview"]
async fn mints_po_tokens_in_the_system_webview() {
    let (_dir, service) = start_service();
    let video = vec!["dQw4w9WgXcQ".to_owned()];
    let both = vec![
        "fa5IWHDbftI".to_owned(),
        "CgtxRzl5bVZuQl8zcyjF25fWBjIKCgJVUxIEGgAgOw%3D%3D".to_owned(),
    ];

    let started = Instant::now();
    let cold = service.mint(&video).await.unwrap();
    let cold_time = started.elapsed();
    let started = Instant::now();
    let warm = service.mint(&video).await.unwrap();
    let warm_time = started.elapsed();
    let started = Instant::now();
    let pair = service.mint(&both).await.unwrap();
    let pair_time = started.elapsed();

    println!("cold mint (spawn helper + webview + BotGuard): {cold_time:?}");
    println!("warm mint: {warm_time:?}; warm mint of 2 identifiers: {pair_time:?}");
    println!(
        "token: {} (valid until {:?})",
        cold.tokens[0], cold.valid_until
    );
    assert_eq!(cold.tokens.len(), 1);
    assert_eq!(warm.tokens.len(), 1);
    assert_eq!(pair.tokens.len(), 2);
    assert!(cold.tokens[0].len() > 80);
    assert!(cold.valid_until > std::time::SystemTime::now() + Duration::from_secs(600));
}

/// Drives the helper executable directly over its pipes.
#[test]
#[ignore = "needs network access to YouTube and a system webview"]
fn real_helper_speaks_the_protocol_and_exits_when_stdin_closes() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(HELPER)
        .arg(HELPER_FLAG)
        .arg(dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    writeln!(stdin, r#"{{"id":41,"identifiers":[]}}"#).unwrap();
    writeln!(stdin, r#"{{"id":42,"identifiers":["dQw4w9WgXcQ"]}}"#).unwrap();
    writeln!(stdin, r#"{{"id":43,"resolve":"{}"}}"#, SONGS[0]).unwrap();
    let mut replies = Vec::new();
    for _ in 0..3 {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        println!("helper: {}", line.trim_end());
        replies.push(serde_json::from_str::<serde_json::Value>(&line).unwrap());
    }
    replies.sort_by_key(|reply| reply["id"].as_u64());
    assert_eq!(replies[0]["id"], 41);
    assert_eq!(replies[0]["tokens"].as_array().unwrap().len(), 0);
    assert_eq!(replies[1]["id"], 42);
    assert!(replies[1]["tokens"][0].as_str().unwrap().len() > 80);
    assert!(replies[1]["valid_until"].as_u64().unwrap() > 1_700_000_000);
    assert_eq!(replies[2]["id"], 43);
    let stream = &replies[2]["stream"];
    assert!(stream["url"].as_str().unwrap().starts_with("https://"));
    assert!(
        stream["user_agent"]
            .as_str()
            .unwrap()
            .contains("AppleWebKit")
    );

    // Closing stdin must end the helper.
    drop(stdin);
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(
            Instant::now() < until,
            "helper did not exit after stdin closed"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Downloads the whole stream in `&range=` chunks; returns bytes and time.
async fn download_fully(stream: &AudioStream) -> (u64, Duration) {
    let client = reqwest::Client::new();
    let total = stream.content_length.expect("content length");
    let started = Instant::now();
    let mut received = 0u64;
    while received < total {
        let end = (received + CHUNK).min(total) - 1;
        let mut request = client.get(format!("{}&range={received}-{end}", stream.url));
        for (name, value) in &stream.headers {
            request = request.header(name, value);
        }
        let response = request.send().await.unwrap();
        let status = response.status();
        assert!(
            status.as_u16() == 200 || status.as_u16() == 206,
            "chunk {received}-{end}: HTTP {status}"
        );
        let body = response.bytes().await.unwrap();
        assert_eq!(
            body.len() as u64,
            end - received + 1,
            "chunk {received}-{end} size"
        );
        if received == 0 {
            // MP4 files start with an `ftyp` box.
            assert_eq!(&body[4..8], b"ftyp", "not an MP4 file");
        }
        received += body.len() as u64;
    }
    (received, started.elapsed())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs network access to YouTube and a system webview"]
async fn resolves_and_downloads_youtube_music_songs() {
    let (_dir, service) = start_service();
    for (index, video_id) in SONGS.iter().enumerate() {
        let started = Instant::now();
        let stream = service.resolve_audio_stream(video_id).await.unwrap();
        let resolve_time = started.elapsed();
        println!(
            "{video_id}: resolved in {resolve_time:?} ({}): itag {} {} {:?} bytes, {:?} bps, headers {:?}",
            if index == 0 {
                "cold: helper + webview + minter + youtubei.js session"
            } else {
                "warm minter and session, new video"
            },
            stream.itag,
            stream.mime_type,
            stream.content_length,
            stream.bitrate,
            stream.headers,
        );
        assert!(stream.mime_type.starts_with("audio/mp4"));
        let user_agent = stream.user_agent.as_deref().unwrap();
        assert!(user_agent.contains("AppleWebKit"), "{user_agent}");
        assert_eq!(
            stream.headers,
            vec![("User-Agent".to_owned(), user_agent.to_owned())]
        );
        // Not even the User-Agent is required (checked on the first MB).
        let bare = reqwest::Client::new()
            .get(format!("{}&range=0-1048575", stream.url))
            .send()
            .await
            .unwrap();
        assert!(
            matches!(bare.status().as_u16(), 200 | 206),
            "no headers: HTTP {}",
            bare.status()
        );
        assert_eq!(bare.bytes().await.unwrap().len(), 1_048_576);

        let (bytes, download_time) = download_fully(&stream).await;
        assert_eq!(Some(bytes), stream.content_length);
        println!("{video_id}: downloaded {bytes} bytes in {download_time:?}");
    }

    // Fully warm: the same videos again, then both at once.
    for video_id in SONGS {
        let started = Instant::now();
        service.resolve_audio_stream(video_id).await.unwrap();
        println!("{video_id}: warm re-resolve in {:?}", started.elapsed());
    }
    let started = Instant::now();
    let (a, b) = tokio::join!(
        service.resolve_audio_stream(SONGS[0]),
        service.resolve_audio_stream(SONGS[1])
    );
    a.unwrap();
    b.unwrap();
    println!("both songs concurrently in {:?}", started.elapsed());
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs network access to YouTube and a system webview"]
async fn reports_unavailable_videos_readably() {
    let (_dir, service) = start_service();
    let error = service
        .resolve_audio_stream("aaaaaaaaaaa")
        .await
        .unwrap_err();
    println!("error: {error:?}");
    assert!(
        matches!(error, elizabeats_botguard::BotguardError::Youtube(_)),
        "{error:?}"
    );
    assert!(!error.to_string().contains("Error:"), "{error}");
}
