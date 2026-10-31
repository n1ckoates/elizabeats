//! Library search: local results immediately, YouTube Music after a short pause.

use std::sync::Arc;
use std::time::Duration;

use elizabeats_core::{SourceId, Track};
use elizabeats_youtube::YoutubeMusic;
use gpui_kit::{Context, Entity, Task};

use crate::station::Station;

/// Typing pauses this long before YouTube Music is searched; Enter skips it.
const YOUTUBE_DEBOUNCE: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SearchStatus {
    Searching,
    Complete,
}

pub struct Search {
    station: Entity<Station>,
    query: String,
    tracks: Vec<Track>,
    errors: Vec<String>,
    /// Sources still running for the current request.
    remaining: usize,
    generation: u64,
    _youtube_task: Option<Task<()>>,
    _observe_library: gpui_kit::Subscription,
}

/// Merges `incoming` into `current` by Track identity; local Tracks first.
pub fn merge_tracks(current: &mut Vec<Track>, incoming: Vec<Track>) {
    for track in incoming {
        let key = track.key();
        if let Some(existing) = current.iter_mut().find(|existing| existing.key() == key) {
            *existing = track;
        } else {
            current.push(track);
        }
    }
    // Stable sort keeps each Source's own order.
    current.sort_by_key(|track| track.source != SourceId::Local);
}

impl Search {
    pub fn new(station: Entity<Station>, cx: &mut Context<Self>) -> Self {
        let mut last_revision = station.read(cx).library_revision();
        let observe_library = cx.observe(&station, move |this, station, cx| {
            let revision = station.read(cx).library_revision();
            if revision != last_revision {
                last_revision = revision;
                this.run(YOUTUBE_DEBOUNCE, cx);
            }
        });
        let mut search = Self {
            station,
            query: String::new(),
            tracks: Vec::new(),
            errors: Vec::new(),
            remaining: 0,
            generation: 0,
            _youtube_task: None,
            _observe_library: observe_library,
        };
        search.run(YOUTUBE_DEBOUNCE, cx);
        search
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn status(&self) -> SearchStatus {
        if self.remaining > 0 {
            SearchStatus::Searching
        } else {
            SearchStatus::Complete
        }
    }

    pub fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    pub fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        if query == self.query {
            return;
        }
        self.query = query;
        self.run(YOUTUBE_DEBOUNCE, cx);
    }

    /// Enter: search YouTube Music now.
    pub fn submit(&mut self, cx: &mut Context<Self>) {
        self.run(Duration::ZERO, cx);
    }

    fn run(&mut self, youtube_delay: Duration, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let query = self.query.trim().to_owned();
        self.tracks.clear();
        self.errors.clear();
        self.remaining = if query.is_empty() { 1 } else { 2 };
        self._youtube_task = None;

        let services = self.station.read(cx).services().clone();
        let local = services.library.search(&query);
        self.apply(generation, local, "Local library");

        if !query.is_empty() {
            let youtube = services.youtube.clone();
            let handle = services.runtime.handle().clone();
            let timer = cx.background_executor().timer(youtube_delay);
            self._youtube_task = Some(cx.spawn(async move |this, cx| {
                timer.await;
                let result = youtube_search(&handle, youtube, query).await;
                let _ = this.update(cx, |this, cx| {
                    this.apply(generation, result, "YouTube Music");
                    cx.notify();
                });
            }));
        }
        cx.notify();
    }

    fn apply(&mut self, generation: u64, result: Result<Vec<Track>, String>, source: &str) {
        if generation != self.generation {
            return;
        }
        self.remaining = self.remaining.saturating_sub(1);
        match result {
            Ok(tracks) => merge_tracks(&mut self.tracks, tracks),
            Err(error) => self.errors.push(format!("{source}: {error}")),
        }
    }
}

/// Runs on the tokio runtime; the task is aborted if a newer search replaces it.
async fn youtube_search(
    handle: &tokio::runtime::Handle,
    youtube: Arc<YoutubeMusic>,
    query: String,
) -> Result<Vec<Track>, String> {
    struct AbortOnDrop(tokio::task::AbortHandle);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let task = handle.spawn(async move {
        youtube
            .search(&query)
            .await
            .map_err(|error| error.to_string())
    });
    let _guard = AbortOnDrop(task.abort_handle());
    match task.await {
        Ok(result) => result,
        Err(error) if error.is_cancelled() => Ok(Vec::new()),
        Err(_) => Err("Search failed.".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(id: &str, source: SourceId) -> Track {
        Track {
            id: id.into(),
            source,
            title: id.into(),
            artist: "a".into(),
            album: None,
            duration_seconds: None,
            artwork_url: None,
            explicit: false,
        }
    }

    #[test]
    fn merge_puts_local_first_and_dedups() {
        let mut current = vec![track("y1", SourceId::YoutubeMusic)];
        merge_tracks(
            &mut current,
            vec![
                track("l1", SourceId::Local),
                track("y1", SourceId::YoutubeMusic),
            ],
        );
        let ids: Vec<_> = current.iter().map(|track| track.id.as_str()).collect();
        assert_eq!(ids, ["l1", "y1"]);
        merge_tracks(
            &mut current,
            vec![
                track("y2", SourceId::YoutubeMusic),
                track("l2", SourceId::Local),
            ],
        );
        let ids: Vec<_> = current.iter().map(|track| track.id.as_str()).collect();
        assert_eq!(ids, ["l1", "l2", "y1", "y2"]);
    }
}
