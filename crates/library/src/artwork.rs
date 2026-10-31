//! Embedded cover art, read on demand and kept in a small cache.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Arc;

use lofty::config::ParseOptions;
use lofty::file::TaggedFileExt;
use lofty::picture::{Picture, PictureType};

use crate::scan::probe;

/// Cover art embedded in a local audio file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Artwork {
    pub bytes: Vec<u8>,
    /// For example `image/jpeg`.
    pub mime: String,
}

/// Reads the front cover (or else the first picture) from `path`.
pub(crate) fn read_artwork(path: &Path) -> Option<Artwork> {
    let tagged = probe(path, ParseOptions::new().read_cover_art(true))?;
    let primary = tagged.primary_tag();
    let tags = primary.into_iter().chain(tagged.tags().iter());
    let pictures: Vec<&Picture> = tags.flat_map(|tag| tag.pictures()).collect();
    let picture = pictures
        .iter()
        .find(|picture| picture.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first())?;
    let bytes = picture.data().to_vec();
    if bytes.is_empty() {
        return None;
    }
    let mime = picture
        .mime_type()
        .map(|mime| mime.as_str().to_owned())
        .filter(|mime| mime.starts_with("image/"))
        .or_else(|| sniff_mime(&bytes).map(str::to_owned))?;
    Some(Artwork { bytes, mime })
}

fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG") {
        Some("image/png")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

/// A least-recently-used cache. Files without artwork are cached as `None`
/// so they are not re-read on every repaint.
pub(crate) struct ArtworkCache {
    capacity: usize,
    entries: HashMap<String, Option<Arc<Artwork>>>,
    /// Most recently used at the back.
    order: VecDeque<String>,
}

impl ArtworkCache {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    pub(crate) fn get(&mut self, id: &str) -> Option<Option<Arc<Artwork>>> {
        let value = self.entries.get(id)?.clone();
        self.touch(id);
        Some(value)
    }

    pub(crate) fn insert(&mut self, id: String, value: Option<Arc<Artwork>>) {
        if self.entries.insert(id.clone(), value).is_some() {
            self.touch(&id);
            return;
        }
        self.order.push_back(id);
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
    }

    fn touch(&mut self, id: &str) {
        if let Some(position) = self.order.iter().position(|entry| entry == id)
            && let Some(entry) = self.order.remove(position)
        {
            self.order.push_back(entry);
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art(byte: u8) -> Option<Arc<Artwork>> {
        Some(Arc::new(Artwork {
            bytes: vec![byte],
            mime: "image/png".into(),
        }))
    }

    #[test]
    fn evicts_least_recently_used() {
        let mut cache = ArtworkCache::new(2);
        cache.insert("a".into(), art(1));
        cache.insert("b".into(), None);
        assert!(cache.get("a").is_some()); // `a` is now the most recent.
        cache.insert("c".into(), art(3));
        assert_eq!(cache.len(), 2);
        assert!(cache.get("b").is_none(), "b was least recently used");
        assert_eq!(cache.get("a"), Some(art(1)));
        assert_eq!(cache.get("c"), Some(art(3)));
    }

    #[test]
    fn sniffs_common_image_types() {
        assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_mime(b"\x89PNG\r\n"), Some("image/png"));
        assert_eq!(sniff_mime(b"nope"), None);
    }
}
