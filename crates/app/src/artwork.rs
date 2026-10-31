//! Track artwork, loaded off the UI thread and kept in a small LRU cache.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use elizabeats_core::{SourceId, Track};
use gpui_kit::{Context, Image, ImageFormat};

use crate::station::Services;

const CAPACITY: usize = 400;
const MAX_REMOTE_BYTES: usize = 5 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ArtKey {
    Local(String),
    Remote(String),
}

pub struct ArtworkCache {
    services: Arc<Services>,
    /// `None` means "no artwork" (missing or unreadable); it is not retried.
    images: HashMap<ArtKey, Option<Arc<Image>>>,
    order: VecDeque<ArtKey>,
    loading: HashSet<ArtKey>,
}

/// Detects the image container from its first bytes.
pub fn sniff_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(ImageFormat::Gif)
    } else if bytes.starts_with(b"BM") {
        Some(ImageFormat::Bmp)
    } else {
        None
    }
}

fn to_image(bytes: Vec<u8>) -> Option<Arc<Image>> {
    let format = sniff_format(&bytes)?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

impl ArtworkCache {
    pub fn new(services: Arc<Services>) -> Self {
        Self {
            services,
            images: HashMap::new(),
            order: VecDeque::new(),
            loading: HashSet::new(),
        }
    }

    /// Returns cached artwork, starting a background load on a miss.
    pub fn get(&mut self, track: &Track, cx: &mut Context<Self>) -> Option<Arc<Image>> {
        let key = match (&track.source, &track.artwork_url) {
            (SourceId::Local, _) => ArtKey::Local(track.id.clone()),
            (SourceId::YoutubeMusic, Some(url)) => ArtKey::Remote(url.clone()),
            (SourceId::YoutubeMusic, None) => return None,
        };
        if let Some(image) = self.images.get(&key) {
            return image.clone();
        }
        if self.loading.insert(key.clone()) {
            self.load(key, cx);
        }
        None
    }

    fn load(&mut self, key: ArtKey, cx: &mut Context<Self>) {
        let services = self.services.clone();
        let handle = services.runtime.handle().clone();
        let job_key = key.clone();
        let job = handle.spawn(async move {
            match job_key {
                ArtKey::Local(id) => {
                    let library = services.library.clone();
                    tokio::task::spawn_blocking(move || library.artwork(&id))
                        .await
                        .ok()
                        .flatten()
                }
                ArtKey::Remote(url) => fetch_remote(&services.http, &url).await,
            }
        });
        cx.spawn(async move |this, cx| {
            let bytes = job.await.ok().flatten();
            let image = bytes.and_then(to_image);
            let _ = this.update(cx, |this, cx| {
                this.loading.remove(&key);
                this.insert(key, image);
                cx.notify();
            });
        })
        .detach();
    }

    fn insert(&mut self, key: ArtKey, image: Option<Arc<Image>>) {
        if self.images.insert(key.clone(), image).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > CAPACITY {
            if let Some(oldest) = self.order.pop_front() {
                self.images.remove(&oldest);
            }
        }
    }

    /// Local artwork changes when the Library is rescanned.
    pub fn forget_local(&mut self) {
        self.images
            .retain(|key, _| matches!(key, ArtKey::Remote(_)));
        self.order.retain(|key| matches!(key, ArtKey::Remote(_)));
    }
}

async fn fetch_remote(http: &reqwest::Client, url: &str) -> Option<Vec<u8>> {
    if !url.starts_with("https://") {
        return None;
    }
    let response = http
        .get(url)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_REMOTE_BYTES as u64)
    {
        return None;
    }
    let bytes = response.bytes().await.ok()?;
    (bytes.len() <= MAX_REMOTE_BYTES).then(|| bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_common_formats() {
        assert_eq!(
            sniff_format(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some(ImageFormat::Jpeg)
        );
        assert_eq!(
            sniff_format(b"\x89PNG\r\n\x1a\nrest"),
            Some(ImageFormat::Png)
        );
        assert_eq!(
            sniff_format(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageFormat::Webp)
        );
        assert_eq!(sniff_format(b"hello"), None);
        assert_eq!(sniff_format(&[]), None);
    }
}
