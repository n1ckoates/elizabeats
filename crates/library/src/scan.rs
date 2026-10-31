//! Finding audio files in the Library folder and reading their tags.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::thread;

use elizabeats_core::{SourceId, Track};
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag};

pub(crate) const UNKNOWN_ARTIST: &str = "Unknown artist";

/// Upper bound on threads reading tags at once, so a huge Library cannot
/// starve playback of CPU or file handles.
const MAX_TAG_READERS: usize = 4;

/// The result of scanning a Library folder.
pub(crate) struct ScanResult {
    /// Sorted for display.
    pub tracks: Vec<Track>,
    /// Track id → absolute file path. Only paths found by the scan are ever
    /// resolved, so ids can never escape the Library folder.
    pub files: HashMap<String, PathBuf>,
}

pub(crate) fn is_supported_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("mp3") || extension.eq_ignore_ascii_case("wav")
        })
}

/// Collects every supported audio file below `folder`. Symlinks (to files or
/// folders) are never followed, and unreadable folders are skipped.
pub(crate) fn find_audio_files(folder: &Path, cancelled: &dyn Fn() -> bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![folder.to_path_buf()];
    while let Some(directory) = pending.pop() {
        if cancelled() {
            return Vec::new();
        }
        // A folder can disappear or become unreadable mid-scan; skip it.
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            // `DirEntry::file_type` does not follow symlinks.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() && is_supported_file(&path) {
                files.push(path);
            }
        }
    }
    files
}

/// `/`-separated path of `file` relative to `folder`.
pub(crate) fn track_id(folder: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(folder).ok()?;
    let parts: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Builds the Library index for `folder`. Returns `None` if `cancelled`
/// reports that a newer scan has made this one pointless.
pub(crate) fn scan(folder: &Path, cancelled: &(dyn Fn() -> bool + Sync)) -> Option<ScanResult> {
    let paths = find_audio_files(folder, cancelled);
    if cancelled() {
        return None;
    }
    let mut work = Vec::with_capacity(paths.len());
    let mut files = HashMap::with_capacity(paths.len());
    for path in paths {
        let Some(id) = track_id(folder, &path) else {
            continue;
        };
        // Lossy file names could in theory collide; keep the first one.
        if files.contains_key(&id) {
            continue;
        }
        files.insert(id.clone(), path.clone());
        work.push((id, path));
    }

    let mut tracks = read_tracks_in_parallel(&work, cancelled)?;
    sort_tracks(&mut tracks);
    Some(ScanResult { tracks, files })
}

fn read_tracks_in_parallel(
    work: &[(String, PathBuf)],
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Option<Vec<Track>> {
    let threads = thread::available_parallelism()
        .map_or(1, |count| count.get())
        .clamp(1, MAX_TAG_READERS)
        .min(work.len().max(1));
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Track>> = Mutex::new(Vec::with_capacity(work.len()));

    let read_some = || {
        let mut local = Vec::new();
        loop {
            if cancelled() {
                break;
            }
            let index = next.fetch_add(1, AtomicOrdering::Relaxed);
            let Some((id, path)) = work.get(index) else {
                break;
            };
            local.push(read_track(id, path));
        }
        let mut results = results
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        results.extend(local);
    };

    thread::scope(|scope| {
        // Extra workers are a bonus: if one cannot be spawned the current
        // thread still processes everything below.
        for _ in 1..threads {
            let _ = thread::Builder::new()
                .name("library-tags".into())
                .spawn_scoped(scope, read_some);
        }
        read_some();
    });

    if cancelled() {
        return None;
    }
    Some(
        results
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
}

fn first_text(tags: &[&Tag], get: impl Fn(&Tag) -> Option<String>) -> Option<String> {
    tags.iter()
        .filter_map(|tag| get(tag))
        .map(|text| text.trim().to_owned())
        .find(|text| !text.is_empty())
}

/// Reads one Track's tags. Unreadable or untagged files still produce a
/// Track named after the file.
pub(crate) fn read_track(id: &str, path: &Path) -> Track {
    let fallback_title = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| id.to_owned());
    let mut track = Track {
        id: id.to_owned(),
        source: SourceId::Local,
        title: fallback_title,
        artist: UNKNOWN_ARTIST.to_owned(),
        album: None,
        duration_seconds: None,
        artwork_url: None,
        explicit: false,
    };

    let options = ParseOptions::new().read_cover_art(false);
    let Some(tagged) = probe(path, options) else {
        return track;
    };

    let duration = tagged.properties().duration().as_secs_f64();
    if duration > 0.0 && duration.is_finite() {
        track.duration_seconds = Some(duration);
    }

    // Primary tag first (e.g. ID3v2 for MP3), then any others.
    let primary = tagged.primary_tag();
    let mut tags: Vec<&Tag> = primary.into_iter().collect();
    tags.extend(
        tagged
            .tags()
            .iter()
            .filter(|tag| primary.is_none_or(|primary| !std::ptr::eq(*tag, primary))),
    );

    if let Some(title) = first_text(&tags, |tag| tag.title().map(|text| text.into_owned())) {
        track.title = title;
    }
    track.album = first_text(&tags, |tag| tag.album().map(|text| text.into_owned()));
    let artist = first_text(&tags, |tag| tag.artist().map(|text| text.into_owned()))
        .or_else(|| {
            first_text(&tags, |tag| {
                let artists: Vec<&str> = tag.get_strings(ItemKey::TrackArtists).collect();
                (!artists.is_empty()).then(|| artists.join(", "))
            })
        })
        .or_else(|| {
            first_text(&tags, |tag| {
                tag.get_string(ItemKey::AlbumArtist).map(str::to_owned)
            })
        });
    if let Some(artist) = artist {
        track.artist = artist;
    }
    track
}

/// Opens `path` with lofty, detecting the format from its contents first and
/// its extension second.
/// A malformed file must never take down the Library, so a panic inside the
/// tag parser is treated like any other unreadable file.
pub(crate) fn probe(path: &Path, options: ParseOptions) -> Option<lofty::file::TaggedFile> {
    std::panic::catch_unwind(|| {
        Probe::open(path)
            .ok()?
            .options(options)
            .guess_file_type()
            .ok()?
            .read()
            .ok()
    })
    .ok()
    .flatten()
}

/// Approximates JavaScript's `localeCompare(…, { sensitivity: "base" })`
/// closely enough for display ordering: case-insensitive.
fn compare_case_insensitive(left: &str, right: &str) -> Ordering {
    left.chars()
        .flat_map(char::to_lowercase)
        .cmp(right.chars().flat_map(char::to_lowercase))
}

/// Title, then artist (both case-insensitive), then id.
pub(crate) fn sort_tracks(tracks: &mut [Track]) {
    tracks.sort_by(|left, right| {
        compare_case_insensitive(&left.title, &right.title)
            .then_with(|| compare_case_insensitive(&left.artist, &right.artist))
            .then_with(|| left.id.cmp(&right.id))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_extensions_are_case_insensitive() {
        assert!(is_supported_file(Path::new("a/b.MP3")));
        assert!(is_supported_file(Path::new("b.Wav")));
        assert!(!is_supported_file(Path::new("b.flac")));
        assert!(!is_supported_file(Path::new("mp3")));
    }

    #[test]
    fn track_ids_use_forward_slashes() {
        let folder = Path::new("/music");
        let file = Path::new("/music").join("Artist").join("Song.mp3");
        assert_eq!(track_id(folder, &file).as_deref(), Some("Artist/Song.mp3"));
        assert_eq!(track_id(folder, Path::new("/elsewhere/x.mp3")), None);
    }

    fn track(id: &str, title: &str, artist: &str) -> Track {
        Track {
            id: id.into(),
            source: SourceId::Local,
            title: title.into(),
            artist: artist.into(),
            album: None,
            duration_seconds: None,
            artwork_url: None,
            explicit: false,
        }
    }

    #[test]
    fn sorts_by_title_then_artist_then_id() {
        let mut tracks = vec![
            track("3", "beta", "A"),
            track("2", "Alpha", "b"),
            track("1", "alpha", "B"),
            track("0", "ALPHA", "a"),
        ];
        sort_tracks(&mut tracks);
        let ids: Vec<&str> = tracks.iter().map(|track| track.id.as_str()).collect();
        assert_eq!(ids, ["0", "1", "2", "3"]);
    }
}
