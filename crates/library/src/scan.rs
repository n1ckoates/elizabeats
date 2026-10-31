//! Finding audio files in the Library folder and reading their tags.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::thread;

use elizabeats_core::{SourceId, Track};
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, TaggedFile, TaggedFileExt};
use lofty::picture::PictureType;
use lofty::probe::Probe;
use lofty::tag::{Accessor, ItemKey, Tag};

/// Upper bound on threads reading tags at once, so a huge Library cannot
/// starve playback of CPU or file handles.
const MAX_TAG_READERS: usize = 4;

/// Builds the Library index for `folder`: the Tracks sorted for display, and
/// Track id → absolute file path. Only paths found by the scan are ever
/// resolved, so ids can never escape the Library folder. Returns `None` if
/// `cancelled` reports that a newer scan has made this one pointless.
pub(crate) fn scan(
    folder: &Path,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Option<(Vec<Track>, HashMap<String, PathBuf>)> {
    let paths = find_audio_files(folder, cancelled);
    let mut work = Vec::with_capacity(paths.len());
    let mut files = HashMap::with_capacity(paths.len());
    for path in paths {
        // Lossy file names could in theory collide; keep the first one.
        if let Some(id) = track_id(folder, &path)
            && !files.contains_key(&id)
        {
            files.insert(id.clone(), path.clone());
            work.push((id, path));
        }
    }
    let mut tracks = read_tracks_in_parallel(&work, cancelled);
    if cancelled() {
        return None;
    }
    sort_tracks(&mut tracks);
    Some((tracks, files))
}

fn is_supported_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("mp3") || extension.eq_ignore_ascii_case("wav")
        })
}

/// Collects every supported audio file below `folder`. Symlinks (to files or
/// folders) are never followed, and unreadable folders are skipped.
fn find_audio_files(folder: &Path, cancelled: &dyn Fn() -> bool) -> Vec<PathBuf> {
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
fn track_id(folder: &Path, file: &Path) -> Option<String> {
    let relative = file.strip_prefix(folder).ok()?;
    let parts: Vec<String> = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect();
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Reads tags on up to [`MAX_TAG_READERS`] threads. Stops early (with a
/// partial result) once `cancelled`.
fn read_tracks_in_parallel(
    work: &[(String, PathBuf)],
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Vec<Track> {
    let threads = thread::available_parallelism()
        .map_or(1, |count| count.get())
        .min(MAX_TAG_READERS);
    let chunk_size = work.len().div_ceil(threads).max(1);
    let read = |chunk: &[(String, PathBuf)]| -> Vec<Track> {
        chunk
            .iter()
            .map_while(|(id, path)| (!cancelled()).then(|| read_track(id, path)))
            .collect()
    };
    thread::scope(|scope| {
        let readers: Vec<_> = work
            .chunks(chunk_size)
            .map(|chunk| {
                thread::Builder::new()
                    .name("library-tags".into())
                    .spawn_scoped(scope, move || read(chunk))
                    // If a thread can't be spawned, read its share here.
                    .map_err(|_| chunk)
            })
            .collect();
        readers
            .into_iter()
            .flat_map(|reader| match reader {
                Ok(handle) => handle.join().unwrap_or_default(),
                Err(chunk) => read(chunk),
            })
            .collect()
    })
}

/// Opens `path` with lofty, detecting the format from its contents first and
/// its extension second. A malformed file must never take down the Library,
/// so a panic inside the tag parser is treated like any other unreadable file.
fn probe(path: &Path, read_cover_art: bool) -> Option<TaggedFile> {
    let options = ParseOptions::new().read_cover_art(read_cover_art);
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

/// The primary tag (e.g. ID3v2 for MP3) first, then any others.
fn tags_primary_first(tagged: &TaggedFile) -> Vec<&Tag> {
    let primary = tagged.primary_tag();
    let others = tagged
        .tags()
        .iter()
        .filter(|tag| primary.is_none_or(|primary| !std::ptr::eq(*tag, primary)));
    primary.into_iter().chain(others).collect()
}

fn first_text(tags: &[&Tag], get: impl Fn(&Tag) -> Option<String>) -> Option<String> {
    tags.iter()
        .filter_map(|tag| get(tag))
        .map(|text| text.trim().to_owned())
        .find(|text| !text.is_empty())
}

/// Reads one Track's tags. Unreadable or untagged files still produce a
/// Track named after the file.
fn read_track(id: &str, path: &Path) -> Track {
    let mut track = Track {
        id: id.to_owned(),
        source: SourceId::Local,
        title: path
            .file_stem()
            .map_or_else(|| id.to_owned(), |stem| stem.to_string_lossy().into_owned()),
        artist: "Unknown artist".to_owned(),
        album: None,
        duration_seconds: None,
        artwork_url: None,
        explicit: false,
    };
    let Some(tagged) = probe(path, false) else {
        return track;
    };

    let duration = tagged.properties().duration().as_secs_f64();
    if duration > 0.0 && duration.is_finite() {
        track.duration_seconds = Some(duration);
    }

    let tags = tags_primary_first(&tagged);
    if let Some(title) = first_text(&tags, |tag| tag.title().map(Into::into)) {
        track.title = title;
    }
    track.album = first_text(&tags, |tag| tag.album().map(Into::into));
    let artist = first_text(&tags, |tag| tag.artist().map(Into::into))
        .or_else(|| {
            first_text(&tags, |tag| {
                let artists: Vec<&str> = tag.get_strings(ItemKey::TrackArtists).collect();
                (!artists.is_empty()).then(|| artists.join(", "))
            })
        })
        .or_else(|| {
            first_text(&tags, |tag| {
                tag.get_string(ItemKey::AlbumArtist).map(Into::into)
            })
        });
    if let Some(artist) = artist {
        track.artist = artist;
    }
    track
}

/// The front cover (or else the first picture) embedded in `path`.
pub(crate) fn read_artwork(path: &Path) -> Option<Vec<u8>> {
    let tagged = probe(path, true)?;
    let tags = tags_primary_first(&tagged);
    let mut pictures = tags.iter().flat_map(|tag| tag.pictures());
    let picture = pictures
        .clone()
        .find(|picture| picture.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.next())?;
    let bytes = picture.data();
    (!bytes.is_empty()).then(|| bytes.to_vec())
}

/// Approximates JavaScript's `localeCompare(…, { sensitivity: "base" })`
/// closely enough for display ordering: case-insensitive.
fn compare_case_insensitive(left: &str, right: &str) -> Ordering {
    left.chars()
        .flat_map(char::to_lowercase)
        .cmp(right.chars().flat_map(char::to_lowercase))
}

/// Title, then artist (both case-insensitive), then id.
fn sort_tracks(tracks: &mut [Track]) {
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

    #[test]
    fn sorts_by_title_then_artist_then_id() {
        let track = |id: &str, title: &str, artist: &str| Track {
            id: id.into(),
            source: SourceId::Local,
            title: title.into(),
            artist: artist.into(),
            album: None,
            duration_seconds: None,
            artwork_url: None,
            explicit: false,
        };
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
