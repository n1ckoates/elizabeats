//! Serial persistence worker: recovery snapshots, Play Log entries, settings.
//!
//! Writes run on one background thread in the order they were requested, so
//! a later recovery snapshot can never be overwritten by an earlier one.
//! Failures are reported back to the UI instead of being dropped.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use elizabeats_core::{AppSettings, PlayLogEntry, RecoverySnapshot};

type Reply = futures::channel::oneshot::Sender<Result<(), String>>;

enum Job {
    SaveRecovery(RecoverySnapshot),
    DiscardRecovery,
    SavePlayLog(PlayLogEntry),
    ClearPlayLog(Option<Reply>),
    SaveSettings(AppSettings, Reply),
    Flush(mpsc::Sender<()>),
}

#[derive(Clone)]
pub struct Persistence {
    jobs: mpsc::Sender<Job>,
}

impl Persistence {
    /// `on_error` receives user-facing messages for failed background writes.
    pub fn start(dir: PathBuf, on_error: impl Fn(String) + Send + 'static) -> Self {
        let (jobs, receiver) = mpsc::channel::<Job>();
        let spawned = thread::Builder::new()
            .name("elizabeats-persist".into())
            .spawn(move || run(&dir, &receiver, &on_error));
        if let Err(error) = spawned {
            tracing::error!("could not start persistence thread: {error}");
        }
        Self { jobs }
    }

    fn send(&self, job: Job) {
        if self.jobs.send(job).is_err() {
            tracing::error!("persistence thread is not running");
        }
    }

    pub fn save_recovery(&self, snapshot: RecoverySnapshot) {
        self.send(Job::SaveRecovery(snapshot));
    }

    pub fn discard_recovery(&self) {
        self.send(Job::DiscardRecovery);
    }

    pub fn save_play_log_entry(&self, entry: PlayLogEntry) {
        self.send(Job::SavePlayLog(entry));
    }

    pub fn clear_play_log(&self) -> futures::channel::oneshot::Receiver<Result<(), String>> {
        let (reply, receiver) = futures::channel::oneshot::channel();
        self.send(Job::ClearPlayLog(Some(reply)));
        receiver
    }

    pub fn save_settings(
        &self,
        settings: AppSettings,
    ) -> futures::channel::oneshot::Receiver<Result<(), String>> {
        let (reply, receiver) = futures::channel::oneshot::channel();
        self.send(Job::SaveSettings(settings, reply));
        receiver
    }

    /// Blocks until every earlier write has finished (or `timeout` passes).
    pub fn flush_blocking(&self, timeout: Duration) -> bool {
        let (done, wait) = mpsc::channel();
        self.send(Job::Flush(done));
        wait.recv_timeout(timeout).is_ok()
    }
}

fn run(dir: &std::path::Path, jobs: &mpsc::Receiver<Job>, on_error: &dyn Fn(String)) {
    while let Ok(job) = jobs.recv() {
        match job {
            Job::SaveRecovery(mut snapshot) => {
                // Only the newest queued snapshot matters; skip superseded ones.
                let mut deferred = Vec::new();
                while let Ok(next) = jobs.try_recv() {
                    match next {
                        Job::SaveRecovery(newer) => snapshot = newer,
                        other => {
                            deferred.push(other);
                            break;
                        }
                    }
                }
                if let Err(error) = elizabeats_store::save_recovery(dir, &snapshot) {
                    on_error(format!("Queue recovery could not be saved: {error}"));
                }
                for job in deferred {
                    handle_other(dir, job, on_error);
                }
            }
            other => handle_other(dir, other, on_error),
        }
    }
}

fn handle_other(dir: &std::path::Path, job: Job, on_error: &dyn Fn(String)) {
    match job {
        Job::SaveRecovery(snapshot) => {
            if let Err(error) = elizabeats_store::save_recovery(dir, &snapshot) {
                on_error(format!("Queue recovery could not be saved: {error}"));
            }
        }
        Job::DiscardRecovery => {
            if let Err(error) = elizabeats_store::discard_recovery(dir) {
                on_error(format!("Queue recovery could not be discarded: {error}"));
            }
        }
        Job::SavePlayLog(entry) => {
            if let Err(error) = elizabeats_store::append_play_log(dir, std::slice::from_ref(&entry))
            {
                on_error(format!("Play log could not be saved: {error}"));
            }
        }
        Job::ClearPlayLog(reply) => {
            let result = elizabeats_store::clear_play_log(dir).map_err(|error| error.to_string());
            if let Some(reply) = reply {
                let _ = reply.send(result);
            } else if let Err(error) = result {
                on_error(format!("Play log could not be cleared: {error}"));
            }
        }
        Job::SaveSettings(settings, reply) => {
            let result =
                elizabeats_store::save_settings(dir, &settings).map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
        Job::Flush(done) => {
            let _ = done.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use elizabeats_core::{RecoveryStatus, StoredQueueItem};

    fn snapshot(id: &str) -> RecoverySnapshot {
        RecoverySnapshot {
            version: 1,
            status: RecoveryStatus::Interrupted,
            queue: vec![StoredQueueItem::Break { id: id.into() }],
            session_started_at: None,
            updated_at: "2026-01-01T00:00:00.000Z".into(),
        }
    }

    #[test]
    fn writes_in_order_and_keeps_newest_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let persistence = Persistence::start(dir.path().to_path_buf(), |error| panic!("{error}"));
        for index in 0..50 {
            persistence.save_recovery(snapshot(&format!("b{index}")));
        }
        assert!(persistence.flush_blocking(Duration::from_secs(5)));
        let saved = elizabeats_store::read_recovery(dir.path())
            .unwrap()
            .unwrap();
        assert_eq!(saved.queue[0].id(), "b49");

        persistence.discard_recovery();
        assert!(persistence.flush_blocking(Duration::from_secs(5)));
        assert!(
            elizabeats_store::read_recovery(dir.path())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("file");
        std::fs::write(&blocked, b"x").unwrap();
        let (sender, receiver) = mpsc::channel();
        // A data dir that is actually a file makes every write fail.
        let persistence = Persistence::start(blocked, move |error| {
            let _ = sender.send(error);
        });
        persistence.save_recovery(snapshot("b"));
        assert!(persistence.flush_blocking(Duration::from_secs(5)));
        let error = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(error.starts_with("Queue recovery could not be saved"));
    }
}
