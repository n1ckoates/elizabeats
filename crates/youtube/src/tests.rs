//! Queue, dedup and Download Cache tests (no network), plus live tests that
//! talk to YouTube (`cargo test -p elizabeats-youtube -- --ignored --nocapture`).

use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

use super::*;

/// A fake fetcher: takes `delay`, then writes `size` bytes.
struct FakeFetcher {
    delay: Duration,
    size: usize,
    /// (video ID, start time, end time) of each fetch.
    calls: StdMutex<Vec<(String, Instant, Instant)>>,
    fail_ids: HashSet<String>,
}

impl FakeFetcher {
    fn new(delay: Duration, size: usize) -> Arc<Self> {
        Arc::new(Self {
            delay,
            size,
            calls: StdMutex::new(Vec::new()),
            fail_ids: HashSet::new(),
        })
    }

    fn calls(&self) -> Vec<(String, Instant, Instant)> {
        self.calls.lock().unwrap().clone()
    }
}

impl AudioFetcher for FakeFetcher {
    fn fetch<'a>(
        &'a self,
        video_id: &'a str,
        files: &'a CacheFiles,
    ) -> BoxFuture<'a, Result<FetchedAudio, YoutubeError>> {
        Box::pin(async move {
            let start = Instant::now();
            let file_name = format!("{video_id}.m4a");
            let mut part = files.begin(&file_name).await.map_err(YoutubeError::cache)?;
            tokio::time::sleep(self.delay).await;
            self.calls
                .lock()
                .unwrap()
                .push((video_id.to_owned(), start, Instant::now()));
            if self.fail_ids.contains(video_id) {
                return Err(YoutubeError::DownloadForbidden);
            }
            part.write_all(&vec![0u8; self.size])
                .await
                .map_err(YoutubeError::cache)?;
            let byte_size = part.finish().await.map_err(YoutubeError::cache)?;
            Ok(FetchedAudio {
                file_name,
                byte_size,
                client_type: "Fake".into(),
            })
        })
    }
}

struct Harness {
    youtube: Arc<YoutubeMusic>,
    temp: tempfile::TempDir,
    events: Arc<StdMutex<Vec<DiagnosticEvent>>>,
    changes: Arc<AtomicUsize>,
}

impl Harness {
    fn new(fetcher: Arc<dyn AudioFetcher>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        Self::in_dir(temp, fetcher)
    }

    fn in_dir(temp: tempfile::TempDir, fetcher: Arc<dyn AudioFetcher>) -> Self {
        let events = Arc::new(StdMutex::new(Vec::new()));
        let changes = Arc::new(AtomicUsize::new(0));
        let config = config(temp.path().to_owned(), events.clone(), changes.clone());
        let source = Arc::new(YoutubeSource::new(None).unwrap());
        let youtube = YoutubeMusic::with_parts(config, source, fetcher, Timing::default());
        Self {
            youtube: Arc::new(youtube),
            temp,
            events,
            changes,
        }
    }

    fn cache_dir(&self) -> PathBuf {
        self.temp.path().join("download-cache")
    }

    fn manifest(&self) -> Value {
        let bytes = std::fs::read(self.cache_dir().join("manifest.json")).unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn write_file(&self, name: &str, len: usize) {
        std::fs::create_dir_all(self.cache_dir()).unwrap();
        std::fs::write(self.cache_dir().join(name), vec![1u8; len]).unwrap();
    }

    fn write_manifest(&self, manifest: &Value) {
        std::fs::create_dir_all(self.cache_dir()).unwrap();
        std::fs::write(self.cache_dir().join("manifest.json"), manifest.to_string()).unwrap();
    }

    fn files_on_disk(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.cache_dir())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn events(&self) -> Vec<DiagnosticEvent> {
        self.events.lock().unwrap().clone()
    }
}

fn config(
    data_dir: PathBuf,
    events: Arc<StdMutex<Vec<DiagnosticEvent>>>,
    changes: Arc<AtomicUsize>,
) -> YoutubeConfig {
    YoutubeConfig {
        data_dir,
        botguard: None,
        diagnostics: Arc::new(move |event| events.lock().unwrap().push(event)),
        on_cache_change: Arc::new(move || {
            changes.fetch_add(1, Ordering::SeqCst);
        }),
    }
}

fn id(n: u8) -> String {
    format!("video{n:06}")
}

#[tokio::test(start_paused = true)]
async fn downloads_run_one_at_a_time_with_gaps() {
    let fetcher = FakeFetcher::new(Duration::from_secs(2), 100);
    let h = Harness::new(fetcher.clone());
    h.youtube.initialize(1 << 30, vec![]).await.unwrap();

    let (one, two, three) = (id(1), id(2), id(3));
    let (a, b, c) = tokio::join!(
        h.youtube.resolve(&one),
        h.youtube.resolve(&two),
        h.youtube.resolve(&three),
    );
    assert!(a.is_ok() && b.is_ok() && c.is_ok());

    let calls = fetcher.calls();
    let order: Vec<_> = calls.iter().map(|(id, _, _)| id.clone()).collect();
    assert_eq!(order, vec![id(1), id(2), id(3)], "request order is kept");
    for pair in calls.windows(2) {
        let gap = pair[1].1 - pair[0].2;
        assert!(
            gap >= Duration::from_secs(5) && gap <= Duration::from_millis(10_050),
            "gap {gap:?} must be 5–10 s"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn concurrent_requests_share_one_download() {
    let fetcher = FakeFetcher::new(Duration::from_secs(1), 100);
    let h = Harness::new(fetcher.clone());
    h.youtube.initialize(1 << 30, vec![]).await.unwrap();

    let one = id(1);
    let (a, b) = tokio::join!(h.youtube.resolve(&one), h.youtube.resolve(&one));
    assert_eq!(a.as_ref().ok(), b.as_ref().ok());
    assert_eq!(fetcher.calls().len(), 1);
    let path = a.unwrap();
    assert_eq!(path, h.cache_dir().join(format!("{}.m4a", id(1))));
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 100);

    // Cached now: no new fetch and no gap.
    let before = Instant::now();
    assert_eq!(h.youtube.resolve(&id(1)).await.unwrap(), path);
    assert_eq!(Instant::now(), before);
    assert_eq!(fetcher.calls().len(), 1);
    assert_eq!(h.youtube.cached_video_ids(), vec![id(1)]);
    assert_eq!(h.youtube.cache_status().used_bytes, 100);

    let manifest = h.manifest();
    assert_eq!(manifest["version"], 1);
    assert_eq!(manifest["entries"][0]["videoId"], json!(id(1)));
    assert_eq!(manifest["entries"][0]["byteSize"], 100);
    assert!(h.changes.load(Ordering::SeqCst) > 0);

    let resolves: Vec<_> = h
        .events()
        .into_iter()
        .filter(|e| e.operation == "resolve")
        .collect();
    assert_eq!(resolves.len(), 3);
    assert!(
        resolves
            .iter()
            .all(|e| e.error.is_none() && e.client_name.as_deref() == Some(CLIENT_NAME))
    );
}

#[tokio::test(start_paused = true)]
async fn dropped_caller_does_not_cancel_download() {
    let fetcher = FakeFetcher::new(Duration::from_secs(3), 100);
    let h = Harness::new(fetcher.clone());
    h.youtube.initialize(1 << 30, vec![]).await.unwrap();

    let youtube = h.youtube.clone();
    let caller = tokio::spawn(async move { youtube.resolve(&id(1)).await });
    tokio::time::sleep(Duration::from_secs(1)).await;
    caller.abort();
    tokio::time::sleep(Duration::from_secs(5)).await;
    h.youtube.flush().await;
    assert_eq!(h.youtube.cached_video_ids(), vec![id(1)]);
}

#[tokio::test(start_paused = true)]
async fn failures_reach_every_waiter_and_can_be_retried() {
    let fetcher = Arc::new(FakeFetcher {
        delay: Duration::from_secs(1),
        size: 100,
        calls: StdMutex::new(Vec::new()),
        fail_ids: HashSet::from([id(1)]),
    });
    let h = Harness::new(fetcher.clone());
    h.youtube.initialize(1 << 30, vec![]).await.unwrap();

    let one = id(1);
    let (a, b) = tokio::join!(h.youtube.resolve(&one), h.youtube.resolve(&one));
    assert_eq!(a, Err(YoutubeError::DownloadForbidden));
    assert_eq!(b, Err(YoutubeError::DownloadForbidden));
    assert_eq!(
        h.youtube.resolve(&id(1)).await,
        Err(YoutubeError::DownloadForbidden)
    );
    assert_eq!(fetcher.calls().len(), 2, "a later call tries again");
    assert!(h.files_on_disk().iter().all(|n| !n.ends_with(".part")));

    let event = h.events().into_iter().find(|e| e.error.is_some()).unwrap();
    assert_eq!(event.operation, "resolve");
    assert_eq!(event.status_code, Some(403));
    assert_eq!(event.video_id, Some(id(1)));
    assert!(event.error.unwrap().contains("PO token"));

    assert_eq!(
        h.youtube.resolve("../../etc").await,
        Err(YoutubeError::InvalidVideoId)
    );
}

#[tokio::test(start_paused = true)]
async fn whole_download_times_out_and_cleans_up() {
    let fetcher = FakeFetcher::new(Duration::from_secs(600), 100);
    let h = Harness::new(fetcher.clone());
    h.youtube.initialize(1 << 30, vec![]).await.unwrap();
    assert_eq!(
        h.youtube.resolve(&id(1)).await,
        Err(YoutubeError::DownloadTimedOut)
    );
    assert_eq!(h.files_on_disk(), vec!["manifest.json"]);
}

#[tokio::test]
async fn initialize_reconciles_manifest_with_disk() {
    let h = Harness::new(FakeFetcher::new(Duration::ZERO, 1));
    h.write_manifest(&json!({"version": 1, "entries": [
        {"videoId": "present0001", "fileName": "present0001.m4a", "byteSize": 999, "downloadedAt": "2025-01-01T00:00:00.000Z", "playCount": 2},
        {"videoId": "missing0001", "fileName": "missing0001.m4a", "byteSize": 10, "playCount": 4},
        {"videoId": "empty000001", "fileName": "empty000001.m4a", "byteSize": 10, "playCount": 0},
        {"videoId": "legacyopus1", "fileName": "legacyopus1.webm", "byteSize": 10, "playCount": 1},
        {"videoId": "bad", "fileName": "../../x.m4a", "playCount": 1},
    ]}));
    h.write_file("present0001.m4a", 50);
    h.write_file("empty000001.m4a", 0);
    h.write_file("legacyopus1.webm", 10);
    h.write_file("orphan00001.m4a", 30);
    h.write_file("emptyorphan.m4a", 0);
    h.write_file("orphan00001.m4a.abc.part", 5);
    h.write_file("manifest.json.xyz.tmp", 5);

    h.youtube.initialize(1 << 30, vec![]).await.unwrap();

    let mut ids = h.youtube.cached_video_ids();
    ids.sort();
    assert_eq!(ids, vec!["orphan00001", "present0001"]);
    assert_eq!(h.youtube.cache_status().used_bytes, 80);
    assert_eq!(
        h.files_on_disk(),
        vec!["manifest.json", "orphan00001.m4a", "present0001.m4a"]
    );
    let manifest = h.manifest();
    let entries = manifest["entries"].as_array().unwrap();
    let missing = entries
        .iter()
        .find(|e| e["videoId"] == "missing0001")
        .unwrap();
    assert_eq!(
        missing,
        &json!({"videoId": "missing0001", "playCount": 4}),
        "play count survives"
    );
    let present = entries
        .iter()
        .find(|e| e["videoId"] == "present0001")
        .unwrap();
    assert_eq!(present["byteSize"], 50);
}

#[tokio::test]
async fn trims_in_eviction_order_and_respects_protection() {
    let h = Harness::new(FakeFetcher::new(Duration::ZERO, 1));
    let entry = |id: &str, plays: u64, at: &str| json!({"videoId": id, "fileName": format!("{id}.m4a"), "byteSize": 100, "downloadedAt": at, "playCount": plays});
    h.write_manifest(&json!({"version": 1, "entries": [
        entry("favourite01", 9, "2025-01-01T00:00:00.000Z"),
        entry("oldunplayed", 0, "2025-01-01T00:00:00.000Z"),
        entry("newunplayed", 0, "2025-06-01T00:00:00.000Z"),
        entry("onceplayed1", 1, "2025-01-01T00:00:00.000Z"),
    ]}));
    for name in ["favourite01", "oldunplayed", "newunplayed", "onceplayed1"] {
        h.write_file(&format!("{name}.m4a"), 100);
    }

    // Room for 3 of 4, but "oldunplayed" is protected → "newunplayed" goes.
    h.youtube
        .initialize(300, vec!["oldunplayed".into()])
        .await
        .unwrap();
    let mut ids = h.youtube.cached_video_ids();
    ids.sort();
    assert_eq!(ids, vec!["favourite01", "oldunplayed", "onceplayed1"]);

    // Unprotecting it lets it go next once the cache shrinks.
    h.youtube.set_protected_video_ids(vec![]).await.unwrap();
    assert_eq!(h.youtube.cached_video_ids().len(), 3);
    h.youtube.set_max_cache_bytes(150).await.unwrap();
    assert_eq!(h.youtube.cached_video_ids(), vec!["favourite01"]);
    assert_eq!(
        h.youtube.cache_status(),
        DownloadCacheStatus {
            used_bytes: 100,
            max_bytes: 150,
            pending_clear_count: 0
        }
    );
    assert!(!h.cache_dir().join("onceplayed1.m4a").exists());
}

/// Acts like a full disk (the OS's "no space left" error) while `blocker`
/// is still in the cache folder.
struct DiskFullFetcher {
    blocker: &'static str,
    calls: AtomicUsize,
}

impl AudioFetcher for DiskFullFetcher {
    fn fetch<'a>(
        &'a self,
        video_id: &'a str,
        files: &'a CacheFiles,
    ) -> BoxFuture<'a, Result<FetchedAudio, YoutubeError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let file_name = format!("{video_id}.m4a");
            let mut part = files
                .begin(&file_name)
                .await
                .map_err(YoutubeError::cache_write)?;
            if files.size(self.blocker).await.is_some() {
                let full = std::io::Error::from(std::io::ErrorKind::StorageFull);
                return Err(YoutubeError::cache_write(full));
            }
            part.write_all(&[0u8; 10])
                .await
                .map_err(YoutubeError::cache_write)?;
            let byte_size = part.finish().await.map_err(YoutubeError::cache_write)?;
            Ok(FetchedAudio {
                file_name,
                byte_size,
                client_type: "Fake".into(),
            })
        })
    }
}

fn disk_full_harness(blocker: &'static str) -> (Harness, Arc<DiskFullFetcher>) {
    let fetcher = Arc::new(DiskFullFetcher {
        blocker,
        calls: AtomicUsize::new(0),
    });
    let h = Harness::new(fetcher.clone());
    h.write_manifest(&json!({"version": 1, "entries": [
        {"videoId": "queuedtrack", "fileName": "queuedtrack.m4a", "byteSize": 10, "playCount": 0},
        {"videoId": "othertrack1", "fileName": "othertrack1.m4a", "byteSize": 10, "playCount": 5},
    ]}));
    h.write_file("queuedtrack.m4a", 10);
    h.write_file("othertrack1.m4a", 10);
    (h, fetcher)
}

#[tokio::test]
async fn full_disk_evicts_unneeded_tracks_and_retries() {
    // Deleting the unneeded cached Track frees enough space.
    let (h, fetcher) = disk_full_harness("othertrack1.m4a");
    h.youtube
        .initialize(1 << 30, vec!["queuedtrack".into()])
        .await
        .unwrap();

    assert!(h.youtube.resolve(&id(1)).await.is_ok());
    assert_eq!(fetcher.calls.load(Ordering::SeqCst), 2, "retried once");
    let mut ids = h.youtube.cached_video_ids();
    ids.sort();
    assert_eq!(
        ids,
        vec!["queuedtrack".to_owned(), id(1)],
        "the queued Track is kept"
    );
    assert!(h.files_on_disk().iter().all(|n| !n.ends_with(".part")));
}

#[tokio::test]
async fn full_disk_with_nothing_to_evict_fails_clearly() {
    // Only Tracks the Queue needs are cached, so nothing can be freed.
    let (h, fetcher) = disk_full_harness("queuedtrack.m4a");
    h.youtube
        .initialize(1 << 30, vec!["queuedtrack".into(), "othertrack1".into()])
        .await
        .unwrap();

    let result = h.youtube.resolve(&id(1)).await;
    assert_eq!(result, Err(YoutubeError::DiskFull));
    assert_eq!(
        YoutubeError::DiskFull.to_string(),
        "Not enough disk space to download this track."
    );
    assert_eq!(
        fetcher.calls.load(Ordering::SeqCst),
        1,
        "no pointless retry"
    );
    assert_eq!(
        h.youtube.cached_video_ids().len(),
        2,
        "protected Tracks are kept"
    );
    assert!(h.files_on_disk().iter().all(|n| !n.ends_with(".part")));
}

#[tokio::test]
async fn clear_waits_for_protected_tracks() {
    let h = Harness::new(FakeFetcher::new(Duration::ZERO, 1));
    h.write_manifest(&json!({"version": 1, "entries": [
        {"videoId": "queuedtrack", "fileName": "queuedtrack.m4a", "byteSize": 10, "playCount": 0},
        {"videoId": "othertrack1", "fileName": "othertrack1.m4a", "byteSize": 10, "playCount": 5},
    ]}));
    h.write_file("queuedtrack.m4a", 10);
    h.write_file("othertrack1.m4a", 10);
    h.youtube
        .initialize(1 << 30, vec!["queuedtrack".into()])
        .await
        .unwrap();

    h.youtube.clear_download_cache().await.unwrap();
    assert_eq!(h.youtube.cached_video_ids(), vec!["queuedtrack"]);
    assert_eq!(h.youtube.cache_status().pending_clear_count, 1);
    assert_eq!(h.manifest()["entries"][1]["clearRequested"], true);

    h.youtube.set_protected_video_ids(vec![]).await.unwrap();
    assert!(h.youtube.cached_video_ids().is_empty());
    assert_eq!(h.youtube.cache_status().pending_clear_count, 0);
    assert_eq!(h.files_on_disk(), vec!["manifest.json"]);
}

#[tokio::test]
async fn record_play_and_vanished_files() {
    let fetcher = FakeFetcher::new(Duration::ZERO, 10);
    let h = Harness::new(fetcher.clone());
    h.youtube.initialize(1 << 30, vec![]).await.unwrap();
    h.youtube.record_play(&id(1)).await.unwrap();
    h.youtube.record_play(&id(1)).await.unwrap();
    assert_eq!(
        h.manifest()["entries"][0],
        json!({"videoId": id(1), "playCount": 2})
    );
    assert_eq!(
        h.youtube.record_play("bad id").await,
        Err(YoutubeError::InvalidVideoId)
    );

    let path = h.youtube.resolve(&id(1)).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    // The file vanished behind our back: it is downloaded again.
    assert_eq!(h.youtube.resolve(&id(1)).await.unwrap(), path);
    assert_eq!(fetcher.calls().len(), 2);
    h.youtube.flush().await;
    assert_eq!(h.manifest()["entries"][0]["playCount"], 2);
}

#[tokio::test]
async fn resolve_before_initialize_keeps_existing_manifest() {
    let h = Harness::new(FakeFetcher::new(Duration::ZERO, 10));
    h.write_manifest(&json!({"version": 1, "entries": [
        {"videoId": "playedbefor", "playCount": 7},
    ]}));
    h.youtube.resolve(&id(1)).await.unwrap();
    h.youtube.flush().await;
    let manifest = h.manifest();
    let ids: Vec<_> = manifest["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["videoId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(ids, vec!["playedbefor".to_owned(), id(1)]);
}

#[tokio::test]
async fn empty_search_needs_no_network() {
    let h = Harness::new(FakeFetcher::new(Duration::ZERO, 1));
    assert_eq!(h.youtube.search("   ").await, Ok(vec![]));
}

#[test]
fn random_between_stays_in_range() {
    for _ in 0..1000 {
        let n = random_between(5000, 10_000);
        assert!((5000..=10_000).contains(&n));
    }
    assert_eq!(random_between(7, 7), 7);
    assert_eq!(random_between(9, 3), 9);
}

/// Live tests against YouTube. Run with:
/// `cargo test -p elizabeats-youtube -- --ignored --nocapture`
mod live {
    use super::*;

    fn live_youtube() -> (
        YoutubeMusic,
        tempfile::TempDir,
        Arc<StdMutex<Vec<DiagnosticEvent>>>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let events = Arc::new(StdMutex::new(Vec::new()));
        let config = config(temp.path().to_owned(), events.clone(), Arc::default());
        (YoutubeMusic::new(config).unwrap(), temp, events)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs network"]
    async fn live_search() {
        let (youtube, _temp, _) = live_youtube();
        let started = std::time::Instant::now();
        let tracks = youtube.search("daft punk one more time").await.unwrap();
        println!("search: {} tracks in {:?}", tracks.len(), started.elapsed());
        for track in tracks.iter().take(5) {
            println!("  {track:?}");
        }
        assert!(!tracks.is_empty() && tracks.len() <= 20);
        let first = &tracks[0];
        assert!(is_valid_video_id(&first.id));
        assert!(first.title.to_lowercase().contains("one more time"));
        assert!(first.artist.to_lowercase().contains("daft punk"));
        assert!(first.duration_seconds.is_some_and(|d| d > 60.0));
        assert!(
            first
                .artwork_url
                .as_deref()
                .is_some_and(|u| u.starts_with("https://"))
        );
        assert!(tracks.iter().all(|t| t.source == SourceId::YoutubeMusic));
        let explicit = youtube.search("kendrick lamar humble").await.unwrap();
        println!(
            "explicit flags: {:?}",
            explicit
                .iter()
                .take(3)
                .map(|t| (&t.title, t.explicit))
                .collect::<Vec<_>>()
        );
        assert!(
            explicit.iter().any(|track| track.explicit),
            "expected an explicit result"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs network"]
    async fn live_url_lookup() {
        let (youtube, _temp, _) = live_youtube();
        for url in [
            "https://music.youtube.com/watch?v=FGBhQbmPwH8",
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/jNQXAC9IVRw",
        ] {
            let started = std::time::Instant::now();
            let tracks = youtube.search(url).await.unwrap();
            println!("{url}: {:?} in {:?}", tracks, started.elapsed());
            assert_eq!(tracks.len(), 1);
            assert_eq!(Some(tracks[0].id.clone()), parse_video_id(url));
            assert!(!tracks[0].title.is_empty() && !tracks[0].artist.is_empty());
            assert!(tracks[0].duration_seconds.is_some());
        }
    }
}
