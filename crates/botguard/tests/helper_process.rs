//! The app side of the helper protocol against a scripted fake helper
//! (`elizabeats-botguard-helper --fake-minter <mode>`): request matching,
//! timeouts, killing and restarting the helper. No network or webview needed.

#![allow(clippy::unwrap_used, clippy::expect_used)] // test code

use std::time::{Duration, Instant};

use elizabeats_botguard::{BotguardError, BotguardService, HelperOptions};

const HELPER: &str = env!("CARGO_BIN_EXE_elizabeats-botguard-helper");

fn service(mode: &str, timeout: Duration) -> (tempfile::TempDir, BotguardService) {
    let dir = tempfile::tempdir().unwrap();
    let options = HelperOptions {
        program: HELPER.into(),
        args: vec!["--fake-minter".into(), mode.into()],
        mint_timeout: timeout,
        resolve_timeout: timeout,
    };
    let service = BotguardService::start_with_helper(options).unwrap();
    (dir, service)
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| (*v).to_owned()).collect()
}

/// The fake helper puts its process ID into every token.
fn helper_pid(token: &str) -> u32 {
    let digits: String = token["fake".len()..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap()
}

/// The fake helper puts its process ID into every stream URL (`pid=…`).
fn stream_pid(url: &str) -> u32 {
    let start = url.find("pid=").unwrap() + "pid=".len();
    let digits: String = url[start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().unwrap()
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
async fn tokens_come_back_in_request_order_from_one_helper() {
    let (_dir, service) = service("echo", Duration::from_secs(10));
    let minted = service
        .mint(&ids(&["dQw4w9WgXcQ", "Cgt%3D%3D"]))
        .await
        .unwrap();
    assert_eq!(minted.tokens.len(), 2);
    assert!(
        minted.tokens[0].contains("xdQw4w9WgXcQx"),
        "{:?}",
        minted.tokens
    );
    // Visitor data is percent-decoded before minting.
    assert!(minted.tokens[1].contains("xCgt__x"), "{:?}", minted.tokens);
    assert!(minted.valid_until > std::time::SystemTime::now());

    // Concurrent requests share the helper and get their own answers.
    let requests = (0..8).map(|i| {
        let service = service.clone();
        async move {
            let id = format!("video{i:06}");
            let minted = service.mint(std::slice::from_ref(&id)).await.unwrap();
            (id, minted.tokens)
        }
    });
    let pid = helper_pid(&minted.tokens[0]);
    for (id, tokens) in futures_join_all(requests).await {
        assert_eq!(tokens.len(), 1);
        assert!(tokens[0].contains(&format!("x{id}x")));
        assert_eq!(helper_pid(&tokens[0]), pid, "helper was restarted");
    }
}

/// A service whose first helper process misbehaves (`hang-once` / `crash-once`);
/// returns the marker file the fake writes that process's ID to.
fn misbehaving_once(
    mode: &str,
    timeout: Duration,
) -> (tempfile::TempDir, std::path::PathBuf, BotguardService) {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("first-helper.pid");
    let options = HelperOptions {
        program: HELPER.into(),
        args: vec!["--fake-minter".into(), mode.into(), marker.clone().into()],
        mint_timeout: timeout,
        resolve_timeout: timeout,
    };
    let service = BotguardService::start_with_helper(options).unwrap();
    (dir, marker, service)
}

#[tokio::test]
async fn hung_helper_is_killed_and_replaced() {
    let (_dir, marker, service) = misbehaving_once("hang-once", Duration::from_millis(800));
    // Two requests in flight: the helper never answers the first one but
    // does answer the second.
    let started = Instant::now();
    let (first, second) = (ids(&["aaaaaaaaaaa"]), ids(&["bbbbbbbbbbb"]));
    let (a, b) = tokio::join!(service.mint(&first), service.mint(&second));
    assert!(matches!(a, Err(BotguardError::Timeout)), "{a:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    let hung_pid: u32 = std::fs::read_to_string(&marker).unwrap().parse().unwrap();
    let b = b.unwrap();
    assert!(b.tokens[0].contains("xbbbbbbbbbbbx"));
    assert_eq!(helper_pid(&b.tokens[0]), hung_pid);

    // The overdue helper is killed ...
    #[cfg(unix)]
    wait_until_gone(hung_pid);

    // ... and the next request gets a fresh helper, which answers.
    let minted = service.mint(&ids(&["dQw4w9WgXcQ"])).await.unwrap();
    assert_ne!(helper_pid(&minted.tokens[0]), hung_pid);
}

#[tokio::test]
async fn crashed_helper_fails_fast_and_is_restarted() {
    let (_dir, marker, service) = misbehaving_once("crash-once", Duration::from_secs(10));
    let started = Instant::now();
    let error = service.mint(&ids(&["dQw4w9WgXcQ"])).await.unwrap_err();
    assert!(
        error.to_string().contains("exited unexpectedly"),
        "{error:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    let crashed_pid: u32 = std::fs::read_to_string(&marker).unwrap().parse().unwrap();
    let minted = service.mint(&ids(&["dQw4w9WgXcQ"])).await.unwrap();
    assert_ne!(helper_pid(&minted.tokens[0]), crashed_pid);
}

#[tokio::test]
async fn restart_after_kill_uses_a_new_process() {
    // "junk" echoes (with noise), so a working helper can be observed; kill
    // it from outside and check the service starts another one.
    let (_dir, service) = service("junk", Duration::from_secs(10));
    let token = &service.mint(&ids(&["dQw4w9WgXcQ"])).await.unwrap().tokens[0];
    let pid = helper_pid(token);
    #[cfg(unix)]
    {
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
        let token = &service.mint(&ids(&["dQw4w9WgXcQ"])).await.unwrap().tokens[0];
        assert_ne!(helper_pid(token), pid);
    }
    // Killing it from outside needs `kill`, so the rest is unix only.
    #[cfg(not(unix))]
    let _ = pid;
}

#[tokio::test]
async fn helper_errors_are_reported() {
    let (_dir, service) = service("error", Duration::from_secs(10));
    let error = service.mint(&ids(&["dQw4w9WgXcQ"])).await.unwrap_err();
    assert_eq!(error.to_string(), "fake failure");
}

#[tokio::test]
async fn missing_helper_executable_fails_fast() {
    let dir = tempfile::tempdir().unwrap();
    let options = HelperOptions::webview(dir.path().join("does-not-exist"), dir.path());
    let service = BotguardService::start_with_helper(options).unwrap();
    let error = service.mint(&ids(&["dQw4w9WgXcQ"])).await.unwrap_err();
    assert!(error.to_string().contains("could not start"), "{error:?}");
}

#[tokio::test]
async fn bad_input_never_reaches_the_helper() {
    let (_dir, service) = service("echo", Duration::from_secs(10));
    assert!(matches!(
        service.mint(&ids(&["has space"])).await,
        Err(BotguardError::InvalidIdentifier(_))
    ));
    assert!(matches!(
        service.mint(&[]).await,
        Err(BotguardError::InvalidIdentifier(_))
    ));
    assert!(matches!(
        service.resolve_audio_stream("not-an-id").await,
        Err(BotguardError::InvalidIdentifier(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn helper_exits_when_the_service_is_dropped() {
    let (_dir, service) = service("echo", Duration::from_secs(10));
    let token = &service.mint(&ids(&["dQw4w9WgXcQ"])).await.unwrap().tokens[0];
    let pid = helper_pid(token);
    assert!(is_running(pid));
    drop(service);
    wait_until_gone(pid);
}

#[tokio::test]
async fn resolves_streams_through_the_helper() {
    let (_dir, service) = service("echo", Duration::from_secs(10));
    let stream = service.resolve_audio_stream("dQw4w9WgXcQ").await.unwrap();
    assert!(stream.url.contains("&v=dQw4w9WgXcQ"), "{}", stream.url);
    assert_eq!(stream.itag, 140);
    assert_eq!(stream.content_length, Some(3_433_977));
    assert_eq!(stream.bitrate, Some(130_000));
    assert!(stream.mime_type.starts_with("audio/mp4"));
    // The webview's User-Agent is passed on for the downloads.
    assert_eq!(stream.user_agent.as_deref(), Some("FakeWebView/1.0"));
    assert_eq!(
        stream.headers,
        vec![("User-Agent".to_owned(), "FakeWebView/1.0".to_owned())]
    );

    // Resolves and mints share one helper, concurrently.
    let pid = stream_pid(&stream.url);
    let third = ids(&["ccccccccccc"]);
    let (a, b, minted) = tokio::join!(
        service.resolve_audio_stream("aaaaaaaaaaa"),
        service.resolve_audio_stream("bbbbbbbbbbb"),
        service.mint(&third),
    );
    let (a, b, minted) = (a.unwrap(), b.unwrap(), minted.unwrap());
    assert!(a.url.ends_with("&v=aaaaaaaaaaa"));
    assert!(b.url.ends_with("&v=bbbbbbbbbbb"));
    assert_eq!(stream_pid(&a.url), pid);
    assert_eq!(stream_pid(&b.url), pid);
    assert_eq!(helper_pid(&minted.tokens[0]), pid);
}

#[tokio::test]
async fn user_facing_resolve_errors_are_youtube_errors() {
    let (_dir, unavailable) = service("unavailable", Duration::from_secs(10));
    let error = unavailable
        .resolve_audio_stream("dQw4w9WgXcQ")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, BotguardError::Youtube(message) if message == "This video is unavailable"),
        "{error:?}"
    );
    // Minting still works on the same helper.
    unavailable.mint(&ids(&["dQw4w9WgXcQ"])).await.unwrap();

    let (_dir2, failing) = service("error", Duration::from_secs(10));
    let error = failing
        .resolve_audio_stream("dQw4w9WgXcQ")
        .await
        .unwrap_err();
    assert!(matches!(error, BotguardError::Other(_)), "{error:?}");
}

#[tokio::test]
async fn hung_resolve_kills_and_replaces_the_helper() {
    let (_dir, marker, service) = misbehaving_once("hang-once", Duration::from_millis(800));
    let started = Instant::now();
    let error = service
        .resolve_audio_stream("dQw4w9WgXcQ")
        .await
        .unwrap_err();
    assert!(matches!(error, BotguardError::Timeout), "{error:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    let hung_pid: u32 = std::fs::read_to_string(&marker).unwrap().parse().unwrap();
    #[cfg(unix)]
    wait_until_gone(hung_pid);

    let stream = service.resolve_audio_stream("dQw4w9WgXcQ").await.unwrap();
    assert_ne!(stream_pid(&stream.url), hung_pid);
}

#[tokio::test]
async fn resolve_uses_its_own_timeout() {
    // Resolves never answer: they fail after resolve_timeout, not mint_timeout.
    let dir = tempfile::tempdir().unwrap();
    let options = HelperOptions {
        program: HELPER.into(),
        args: vec!["--fake-minter".into(), "hang-resolve".into()],
        mint_timeout: Duration::from_secs(30),
        resolve_timeout: Duration::from_millis(500),
    };
    let service = BotguardService::start_with_helper(options).unwrap();
    let pid = helper_pid(&service.mint(&ids(&["dQw4w9WgXcQ"])).await.unwrap().tokens[0]);
    let started = Instant::now();
    let error = service
        .resolve_audio_stream("dQw4w9WgXcQ")
        .await
        .unwrap_err();
    assert!(matches!(error, BotguardError::Timeout), "{error:?}");
    assert!(started.elapsed() < Duration::from_secs(3));
    #[cfg(unix)]
    wait_until_gone(pid);
    #[cfg(not(unix))]
    let _ = pid;
    drop(dir);
}

#[tokio::test]
async fn crash_during_resolve_fails_fast_and_restarts() {
    let (_dir, marker, service) = misbehaving_once("crash-once", Duration::from_secs(10));
    let started = Instant::now();
    let error = service
        .resolve_audio_stream("dQw4w9WgXcQ")
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("exited unexpectedly"),
        "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    let crashed_pid: u32 = std::fs::read_to_string(&marker).unwrap().parse().unwrap();
    let stream = service.resolve_audio_stream("dQw4w9WgXcQ").await.unwrap();
    assert_ne!(stream_pid(&stream.url), crashed_pid);
}

/// Minimal `join_all` so the tests need no extra dependency.
async fn futures_join_all<F>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futures.into_iter().map(tokio::spawn).collect();
    let mut results = Vec::new();
    for handle in handles {
        results.push(handle.await.unwrap());
    }
    results
}
