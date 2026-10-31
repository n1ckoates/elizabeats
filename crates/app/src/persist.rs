//! Serial persistence worker: recovery snapshots, Play Log entries, settings.
//!
//! Writes run on one background thread in the order they were requested, so
//! a later recovery snapshot can never be overwritten by an earlier one.
//! Failures are reported back to the UI instead of being dropped.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use elizabeats_core::{AppSettings, PlayLogEntry, RecoverySnapshot};
use elizabeats_store::StoreError;
use futures::channel::oneshot;

type Reply = oneshot::Sender<Result<(), String>>;

enum Job {
    SaveRecovery(RecoverySnapshot),
    DiscardRecovery,
    SavePlayLog(PlayLogEntry),
    ClearPlayLog(Reply),
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

    /// Sends a job that answers with its result.
    fn request(&self, job: impl FnOnce(Reply) -> Job) -> oneshot::Receiver<Result<(), String>> {
        let (reply, receiver) = oneshot::channel();
        self.send(job(reply));
        receiver
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

    pub fn clear_play_log(&self) -> oneshot::Receiver<Result<(), String>> {
        self.request(Job::ClearPlayLog)
    }

    pub fn save_settings(&self, settings: AppSettings) -> oneshot::Receiver<Result<(), String>> {
        self.request(|reply| Job::SaveSettings(settings, reply))
    }

    /// Blocks until every earlier write has finished (or `timeout` passes).
    pub fn flush_blocking(&self, timeout: Duration) -> bool {
        let (done, wait) = mpsc::channel();
        self.send(Job::Flush(done));
        wait.recv_timeout(timeout).is_ok()
    }
}

fn run(dir: &Path, jobs: &mpsc::Receiver<Job>, on_error: &dyn Fn(String)) {
    let report = |what: &str, result: Result<(), StoreError>| {
        if let Err(error) = result {
            on_error(format!("{what}: {error}"));
        }
    };
    let reply = |reply: Reply, result: Result<(), StoreError>| {
        let _ = reply.send(result.map_err(|error| error.to_string()));
    };
    let mut next = None;
    while let Some(job) = next.take().or_else(|| jobs.recv().ok()) {
        match job {
            Job::SaveRecovery(mut snapshot) => {
                // Only the newest queued snapshot matters; skip superseded ones.
                while let Ok(job) = jobs.try_recv() {
                    match job {
                        Job::SaveRecovery(newer) => snapshot = newer,
                        other => {
                            next = Some(other);
                            break;
                        }
                    }
                }
                report(
                    "Queue recovery could not be saved",
                    elizabeats_store::save_recovery(dir, &snapshot),
                );
            }
            Job::DiscardRecovery => report(
                "Queue recovery could not be discarded",
                elizabeats_store::discard_recovery(dir),
            ),
            Job::SavePlayLog(entry) => report(
                "Play log could not be saved",
                elizabeats_store::append_play_log(dir, &entry),
            ),
            Job::ClearPlayLog(answer) => reply(answer, elizabeats_store::clear_play_log(dir)),
            Job::SaveSettings(settings, answer) => {
                reply(answer, elizabeats_store::save_settings(dir, &settings));
            }
            Job::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use elizabeats_core::{QueueItem, RecoveryStatus};

    fn snapshot(id: &str) -> RecoverySnapshot {
        RecoverySnapshot {
            version: 1,
            status: RecoveryStatus::Interrupted,
            queue: vec![QueueItem::new_break(id)],
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
        assert_eq!(saved.queue[0].id, "b49");

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
