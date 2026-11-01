//! The app side of the helper protocol against a scripted fake helper
//! (`elizabeats-botguard-helper --fake-minter <mode>`): request matching,
//! timeouts, killing and restarting the helper. No network or webview needed.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code

use std::path::PathBuf;
use std::time::{Duration, Instant};

use elizabeats_botguard::{AudioStream, BotguardError, BotguardService, HelperOptions};

const HELPER: &str = env!("CARGO_BIN_EXE_elizabeats-botguard-helper");
const VIDEO: &str = "dQw4w9WgXcQ";

/// A service using the fake helper in `mode`, with a marker file for the
/// `*-once` modes (the misbehaving helper writes its process ID there).
fn service(
    mode: &str,
    mint_timeout: Duration,
    resolve_timeout: Duration,
) -> (tempfile::TempDir, PathBuf, BotguardService) {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("first-helper.pid");
    let options = HelperOptions {
        program: HELPER.into(),
        args: vec!["--fake-minter".into(), mode.into(), marker.clone().into()],
        mint_timeout,
        resolve_timeout,
    };
    let service = BotguardService::start_with_helper(options).unwrap();
    (dir, marker, service)
}

fn echo() -> (tempfile::TempDir, PathBuf, BotguardService) {
    service("echo", Duration::from_secs(10), Duration::from_secs(10))
}

/// The fake helper puts its process ID into every stream URL (`pid=…`).
fn helper_pid(stream: &AudioStream) -> u32 {
    let start = stream.url.find("pid=").unwrap() + "pid=".len();
    let digits: String = stream.url[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap()
}

fn marker_pid(marker: &PathBuf) -> u32 {
    std::fs::read_to_string(marker).unwrap().parse().unwrap()
}

fn assert_fast(started: Instant, limit: Duration) {
    assert!(started.elapsed() < limit, "{:?}", started.elapsed());
}

/// Whether a process is still running (unix only; `kill -0` sends nothing).
#[cfg(unix)]
fn is_running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn wait_until_gone(pid: u32) {
    let until = Instant::now() + Duration::from_secs(5);
    while is_running(pid) {
        assert!(Instant::now() < until, "helper {pid} is still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test]
async fn resolves_streams_through_one_helper() {
    let (_dir, _, service) = echo();
    service.prewarm();
    let stream = service.resolve_audio_stream(VIDEO).await.unwrap();
    assert!(
        stream.url.ends_with(&format!("&v={VIDEO}")),
        "{}",
        stream.url
    );
    assert_eq!(stream.itag, 140);
    assert_eq!(stream.content_length, Some(3_433_977));
    assert_eq!(stream.bitrate, Some(130_000));
    assert!(stream.mime_type.starts_with("audio/mp4"));
    // The webview's User-Agent is passed on for the downloads.
    assert_eq!(stream.user_agent.as_deref(), Some("FakeWebView/1.0"));

    // Concurrent requests share the helper and get their own answers.
    let pid = helper_pid(&stream);
    let requests: Vec<_> = (0..8)
        .map(|i| {
            let service = service.clone();
            tokio::spawn(async move {
                let video_id = format!("video{i:06}");
                let stream = service.resolve_audio_stream(&video_id).await.unwrap();
                (video_id, stream)
            })
        })
        .collect();
    for request in requests {
        let (video_id, stream) = request.await.unwrap();
        assert!(stream.url.ends_with(&format!("&v={video_id}")));
        assert_eq!(helper_pid(&stream), pid, "helper was restarted");
    }
}

#[tokio::test]
async fn hung_helper_is_killed_and_replaced() {
    // The timeout also covers starting the helper process, which can take
    // a while on a busy CI machine; if it's too short, the helper is killed
    // before it even writes the marker file and the test fails spuriously.
    let timeout = Duration::from_secs(3);
    let (_dir, marker, service) = service("hang-once", timeout, timeout);
    // Two requests in flight: the helper never answers the first one but
    // does answer the second.
    let started = Instant::now();
    let (a, b) = tokio::join!(
        service.resolve_audio_stream("aaaaaaaaaaa"),
        service.resolve_audio_stream("bbbbbbbbbbb")
    );
    assert!(matches!(a, Err(BotguardError::Timeout)), "{a:?}");
    assert_fast(started, timeout + Duration::from_secs(3));
    let hung_pid = marker_pid(&marker);
    assert_eq!(helper_pid(&b.unwrap()), hung_pid);

    // The overdue helper is killed ...
    #[cfg(unix)]
    wait_until_gone(hung_pid);

    // ... and the next request gets a fresh helper, which answers.
    let stream = service.resolve_audio_stream(VIDEO).await.unwrap();
    assert_ne!(helper_pid(&stream), hung_pid);
}

#[tokio::test]
async fn resolve_uses_its_own_timeout() {
    // The first request never gets an answer: a resolve fails after
    // resolve_timeout, not mint_timeout.
    let (_dir, _, service) = service(
        "hang-once",
        Duration::from_secs(30),
        Duration::from_millis(500),
    );
    let started = Instant::now();
    let error = service.resolve_audio_stream(VIDEO).await.unwrap_err();
    assert!(matches!(error, BotguardError::Timeout), "{error:?}");
    assert_fast(started, Duration::from_secs(3));
}

#[tokio::test]
async fn crashed_helper_fails_fast_and_is_restarted() {
    let (_dir, marker, service) = service(
        "crash-once",
        Duration::from_secs(10),
        Duration::from_secs(10),
    );
    let started = Instant::now();
    let error = service.resolve_audio_stream(VIDEO).await.unwrap_err();
    assert!(
        error.to_string().contains("exited unexpectedly"),
        "{error:?}"
    );
    assert_fast(started, Duration::from_secs(5));

    let stream = service.resolve_audio_stream(VIDEO).await.unwrap();
    assert_ne!(helper_pid(&stream), marker_pid(&marker));
}

#[cfg(unix)]
#[tokio::test]
async fn restart_after_kill_uses_a_new_process() {
    let (_dir, _, service) = echo();
    let pid = helper_pid(&service.resolve_audio_stream(VIDEO).await.unwrap());
    assert!(
        std::process::Command::new("kill")
            .arg(pid.to_string())
            .status()
            .unwrap()
            .success()
    );
    wait_until_gone(pid);
    // Give the reader thread a moment to notice the closed pipe.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let stream = service.resolve_audio_stream(VIDEO).await.unwrap();
    assert_ne!(helper_pid(&stream), pid);
}

#[cfg(unix)]
#[tokio::test]
async fn helper_exits_when_the_service_is_dropped() {
    let (_dir, _, service) = echo();
    let pid = helper_pid(&service.resolve_audio_stream(VIDEO).await.unwrap());
    assert!(is_running(pid));
    drop(service);
    wait_until_gone(pid);
}

#[tokio::test]
async fn helper_errors_are_reported() {
    let long = Duration::from_secs(10);
    let (_dir, _, unavailable) = service("unavailable", long, long);
    let error = unavailable.resolve_audio_stream(VIDEO).await.unwrap_err();
    assert!(
        matches!(&error, BotguardError::Youtube(message) if message == "This video is unavailable"),
        "{error:?}"
    );

    let (_dir2, _, failing) = service("error", long, long);
    let error = failing.resolve_audio_stream(VIDEO).await.unwrap_err();
    assert!(matches!(&error, BotguardError::Other(_)), "{error:?}");
    assert_eq!(error.to_string(), "fake failure");
}

#[tokio::test]
async fn missing_helper_executable_fails_fast() {
    let dir = tempfile::tempdir().unwrap();
    let options = HelperOptions::webview(dir.path().join("does-not-exist"), dir.path());
    let service = BotguardService::start_with_helper(options).unwrap();
    let error = service.resolve_audio_stream(VIDEO).await.unwrap_err();
    assert!(error.to_string().contains("could not start"), "{error:?}");
}

#[tokio::test]
async fn bad_input_never_reaches_the_helper() {
    let (_dir, _, service) = echo();
    for video_id in ["not-an-id", "dQw4w9WgXc!", "\");alert(1);//"] {
        assert!(matches!(
            service.resolve_audio_stream(video_id).await,
            Err(BotguardError::InvalidIdentifier(_))
        ));
    }
}
