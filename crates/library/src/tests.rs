use std::fs;
use std::sync::atomic::AtomicUsize;

use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{Accessor, ItemKey, Tag, TagExt, TagType};

use super::*;

/// One second of silent 8 kHz, 8-bit mono PCM.
fn wav_bytes() -> Vec<u8> {
    let samples = 8000u32;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + samples).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
    bytes.extend_from_slice(&8000u32.to_le_bytes()); // sample rate
    bytes.extend_from_slice(&8000u32.to_le_bytes()); // byte rate
    bytes.extend_from_slice(&1u16.to_le_bytes()); // block align
    bytes.extend_from_slice(&8u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&samples.to_le_bytes());
    bytes.extend(std::iter::repeat_n(128u8, samples as usize));
    bytes
}

/// A few silent MPEG-1 Layer III frames (128 kbps, 44.1 kHz).
fn mp3_bytes() -> Vec<u8> {
    let mut frame = vec![0u8; 417];
    frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
    frame.repeat(20)
}

fn write_wav(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, wav_bytes()).unwrap();
}

fn write_mp3(path: &Path) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, mp3_bytes()).unwrap();
}

fn tag_file(path: &Path, edit: impl FnOnce(&mut Tag)) {
    let mut tag = Tag::new(TagType::Id3v2);
    edit(&mut tag);
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

struct Harness {
    library: LocalLibrary,
    changes: Arc<AtomicUsize>,
    diagnostics: Arc<Mutex<Vec<DiagnosticEvent>>>,
}

fn harness() -> Harness {
    let changes = Arc::new(AtomicUsize::new(0));
    let diagnostics = Arc::new(Mutex::new(Vec::new()));
    let counter = Arc::clone(&changes);
    let sink = Arc::clone(&diagnostics);
    let library = LocalLibrary::new(
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
        Arc::new(move |event| sink.lock().unwrap().push(event)),
    );
    Harness {
        library,
        changes,
        diagnostics,
    }
}

fn wait_until(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn ids(library: &LocalLibrary) -> Vec<String> {
    library
        .tracks()
        .unwrap()
        .iter()
        .map(|track| track.id.clone())
        .collect()
}

#[tokio::test]
async fn scans_tags_with_fallbacks_and_sorts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_wav(&root.join("zebra.wav"));
    write_wav(&root.join("Nested/Deeper/LOUD.WAV"));
    write_mp3(&root.join("tagged.mp3"));
    tag_file(&root.join("tagged.mp3"), |tag| {
        tag.set_title("  Alpha  ".into());
        tag.set_artist("Band".into());
        tag.set_album("Record".into());
    });
    write_wav(&root.join("album-artist.wav"));
    tag_file(&root.join("album-artist.wav"), |tag| {
        tag.set_title("   ".into()); // Blank: falls back to the file name.
        tag.insert_text(ItemKey::AlbumArtist, "Album Person".into());
    });
    fs::write(root.join("notes.txt"), "not audio").unwrap();
    fs::write(root.join("broken.mp3"), "not really audio").unwrap();

    let h = harness();
    h.library.set_library_folder(Some(root.to_path_buf()));
    h.library.rescan_and_wait().await;

    let tracks = h.library.tracks().unwrap();
    let summary: Vec<(&str, &str, &str, Option<&str>)> = tracks
        .iter()
        .map(|t| {
            (
                t.id.as_str(),
                t.title.as_str(),
                t.artist.as_str(),
                t.album.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("album-artist.wav", "album-artist", "Album Person", None),
            ("tagged.mp3", "Alpha", "Band", Some("Record")),
            ("broken.mp3", "broken", "Unknown artist", None),
            ("Nested/Deeper/LOUD.WAV", "LOUD", "Unknown artist", None),
            ("zebra.wav", "zebra", "Unknown artist", None),
        ]
    );
    for track in tracks.iter() {
        assert_eq!(track.source, SourceId::Local);
        assert!(track.artwork_url.is_none());
    }
    let zebra = tracks.iter().find(|t| t.id == "zebra.wav").unwrap();
    let duration = zebra.duration_seconds.unwrap();
    assert!((duration - 1.0).abs() < 0.05, "duration was {duration}");
    assert!(h.changes.load(Ordering::SeqCst) >= 1);
    assert!(
        h.diagnostics
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.operation == "rescan" && e.error.is_none())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn does_not_follow_symlinks() {
    let outside = tempfile::tempdir().unwrap();
    write_wav(&outside.path().join("secret.wav"));
    write_wav(&outside.path().join("dir/inner.wav"));

    let dir = tempfile::tempdir().unwrap();
    write_wav(&dir.path().join("real.wav"));
    std::os::unix::fs::symlink(
        outside.path().join("secret.wav"),
        dir.path().join("link.wav"),
    )
    .unwrap();
    std::os::unix::fs::symlink(outside.path().join("dir"), dir.path().join("linked-dir")).unwrap();

    let h = harness();
    h.library.set_library_folder(Some(dir.path().to_path_buf()));
    h.library.rescan_and_wait().await;
    assert_eq!(ids(&h.library), ["real.wav"]);
    assert!(h.library.resolve("link.wav").is_err());
}

#[tokio::test]
async fn search_matches_title_artist_and_album() {
    let dir = tempfile::tempdir().unwrap();
    write_mp3(&dir.path().join("a.mp3"));
    tag_file(&dir.path().join("a.mp3"), |tag| {
        tag.set_title("Morning Song".into());
        tag.set_artist("The Birds".into());
        tag.set_album("Dawn".into());
    });
    write_wav(&dir.path().join("b.wav"));

    let h = harness();
    h.library.set_library_folder(Some(dir.path().to_path_buf()));
    h.library.rescan_and_wait().await;

    let found = |query: &str| -> Vec<String> {
        h.library
            .search(query)
            .unwrap()
            .into_iter()
            .map(|t| t.id)
            .collect()
    };
    assert_eq!(found(""), ["b.wav", "a.mp3"]);
    assert_eq!(found("   "), ["b.wav", "a.mp3"]);
    assert_eq!(found("  BIRDS "), ["a.mp3"]);
    assert_eq!(found("dawn"), ["a.mp3"]);
    assert_eq!(found("song the"), ["a.mp3"]); // Spans title and artist.
    assert_eq!(found("unknown artist"), ["b.wav"]);
    assert!(found("nothing").is_empty());
}

#[tokio::test]
async fn resolve_only_returns_indexed_regular_files() {
    let dir = tempfile::tempdir().unwrap();
    write_wav(&dir.path().join("Sub/one.wav"));
    write_wav(&dir.path().join("two.wav"));
    let outside = dir.path().parent().unwrap().join("escape.wav");

    let h = harness();
    h.library.set_library_folder(Some(dir.path().to_path_buf()));
    h.library.rescan_and_wait().await;

    let resolved = h.library.resolve("Sub/one.wav").unwrap();
    assert!(resolved.ends_with("Sub/one.wav"));
    assert!(resolved.is_absolute());
    assert_eq!(h.library.path_of("Sub/one.wav"), Some(resolved));

    for id in ["missing.wav", "../escape.wav", "/etc/passwd", ""] {
        assert_eq!(
            h.library.resolve(id),
            Err("This local track is no longer available.".to_owned()),
            "{id}"
        );
    }
    assert!(!outside.exists());

    // Deleted after indexing.
    fs::remove_file(dir.path().join("two.wav")).unwrap();
    assert!(h.library.resolve("two.wav").is_err());
    assert!(
        h.diagnostics
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.operation == "resolve")
    );
}

#[tokio::test]
async fn reports_unavailable_folder_and_recovers_when_it_returns() {
    let dir = tempfile::tempdir().unwrap();
    let folder = dir.path().join("Music");

    let h = harness();
    h.library.set_library_folder(Some(folder.clone()));
    h.library.rescan_and_wait().await;
    assert_eq!(
        h.library.tracks(),
        Err(format!(
            "Library folder is unavailable: {}",
            folder.display()
        ))
    );
    assert!(h.library.search("").is_err());
    assert!(h.diagnostics.lock().unwrap().iter().any(|e| {
        e.operation == "rescan"
            && e.error.as_deref() == Some("Library folder is unavailable.")
            && e.source == Some(SourceId::Local)
    }));

    write_wav(&folder.join("back.wav"));
    wait_until(
        "the folder to be rescanned",
        Duration::from_secs(15),
        || h.library.tracks().is_ok_and(|tracks| tracks.len() == 1),
    );
}

#[tokio::test]
async fn reports_a_file_that_is_not_a_folder() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("file.txt");
    fs::write(&file, "x").unwrap();

    let h = harness();
    h.library.set_library_folder(Some(file.clone()));
    h.library.rescan_and_wait().await;
    assert_eq!(
        h.library.tracks(),
        Err(format!(
            "Library folder is not a folder: {}",
            file.display()
        ))
    );
}

#[tokio::test]
async fn clearing_the_folder_empties_the_library() {
    let dir = tempfile::tempdir().unwrap();
    write_wav(&dir.path().join("a.wav"));
    let h = harness();
    h.library.set_library_folder(Some(dir.path().to_path_buf()));
    h.library.rescan_and_wait().await;
    assert_eq!(ids(&h.library).len(), 1);

    h.library.set_library_folder(None);
    assert!(ids(&h.library).is_empty());
    assert_eq!(h.library.path_of("a.wav"), None);
    h.library.rescan_and_wait().await;
    assert!(ids(&h.library).is_empty());
}

#[tokio::test]
async fn newer_folder_supersedes_older_scan() {
    let first = tempfile::tempdir().unwrap();
    for index in 0..300 {
        write_wav(&first.path().join(format!("old-{index}.wav")));
    }
    let second = tempfile::tempdir().unwrap();
    write_wav(&second.path().join("new.wav"));

    let h = harness();
    h.library
        .set_library_folder(Some(first.path().to_path_buf()));
    h.library
        .set_library_folder(Some(second.path().to_path_buf()));
    h.library.rescan_and_wait().await;
    assert_eq!(ids(&h.library), ["new.wav"]);
    // Give any stray older scan a chance to (wrongly) publish.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(ids(&h.library), ["new.wav"]);
}

#[tokio::test]
async fn watcher_rescans_after_changes() {
    let dir = tempfile::tempdir().unwrap();
    write_wav(&dir.path().join("first.wav"));
    let h = harness();
    h.library.set_library_folder(Some(dir.path().to_path_buf()));
    h.library.rescan_and_wait().await;
    assert_eq!(ids(&h.library), ["first.wav"]);

    // Let the watcher settle before changing files.
    std::thread::sleep(Duration::from_millis(500));
    write_wav(&dir.path().join("Added/second.wav"));
    wait_until(
        "the new file to be indexed",
        Duration::from_secs(15),
        || h.library.tracks().is_ok_and(|tracks| tracks.len() == 2),
    );

    fs::remove_file(dir.path().join("first.wav")).unwrap();
    wait_until(
        "the deleted file to be dropped",
        Duration::from_secs(15),
        || {
            h.library.tracks().is_ok_and(|tracks| {
                tracks
                    .iter()
                    .map(|t| t.id.as_str())
                    .eq(["Added/second.wav"])
            })
        },
    );
}

#[tokio::test]
async fn reads_front_cover_artwork() {
    let dir = tempfile::tempdir().unwrap();
    let png = b"\x89PNG\r\n\x1a\nnot-a-real-png".to_vec();
    let jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3];
    write_mp3(&dir.path().join("art.mp3"));
    tag_file(&dir.path().join("art.mp3"), |tag| {
        tag.set_title("With art".into());
        tag.push_picture(
            Picture::unchecked(png.clone())
                .pic_type(PictureType::Other)
                .mime_type(MimeType::Png)
                .build(),
        );
        tag.push_picture(
            Picture::unchecked(jpeg.clone())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Jpeg)
                .build(),
        );
    });
    write_wav(&dir.path().join("plain.wav"));

    let h = harness();
    h.library.set_library_folder(Some(dir.path().to_path_buf()));
    h.library.rescan_and_wait().await;

    let art = h.library.artwork("art.mp3").unwrap();
    assert_eq!(art.mime, "image/jpeg");
    assert_eq!(art.bytes, jpeg);
    // Cached: the same allocation comes back.
    assert!(Arc::ptr_eq(&art, &h.library.artwork("art.mp3").unwrap()));
    assert!(h.library.artwork("plain.wav").is_none());
    assert!(h.library.artwork("missing.mp3").is_none());
}
