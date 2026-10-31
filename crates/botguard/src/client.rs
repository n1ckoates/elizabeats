//! App side of the webview helper: starts the helper process on first use,
//! sends prewarm and resolve requests to its stdin, matches the replies on its
//! stdout, enforces timeouts and replaces the helper when it crashes or hangs.
//!
//! All of this runs on one plain thread (the "manager") that never blocks on
//! the helper: pipes are read and written by small per-helper threads that
//! report back through the manager's queue. Async callers just await a
//! oneshot channel, so the app's async runtime is never blocked.

use std::collections::HashMap;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

use crate::protocol::{
    AudioStream, HelperReply, HelperRequest, RequestAction, decode_line, encode_line, for_each_line,
};
use crate::{BotguardError, HelperOptions, spawn_thread};

/// One request for the helper.
pub(crate) enum Job {
    /// Makes sure the helper is ready (an empty mint); failures are logged.
    Prewarm,
    Resolve {
        video_id: String,
        reply: oneshot::Sender<Result<AudioStream, BotguardError>>,
    },
}

impl Job {
    fn action(&self) -> RequestAction {
        match self {
            Job::Prewarm => RequestAction::Mint {
                identifiers: Vec::new(),
            },
            Job::Resolve { video_id, .. } => RequestAction::Resolve {
                resolve: video_id.clone(),
            },
        }
    }

    /// Delivers the outcome; called exactly once per job.
    fn finish(self, reply: Result<HelperReply, BotguardError>) {
        match self {
            Job::Prewarm => {
                if let Err(error) = reply.and_then(HelperReply::into_prewarmed) {
                    tracing::warn!("could not prewarm the webview helper: {error}");
                }
            }
            Job::Resolve { reply: sender, .. } => {
                // The caller may have stopped waiting; nothing to do then.
                let _ = sender.send(reply.and_then(HelperReply::into_stream));
            }
        }
    }
}

enum Event {
    Request(Job),
    /// A line the helper with this generation wrote to stdout.
    Line {
        generation: u64,
        line: String,
    },
    /// The helper with this generation closed its stdout (exited or crashed).
    Exited {
        generation: u64,
    },
    Shutdown,
}

/// Handle to the manager thread. Dropping it stops the helper.
#[derive(Debug)]
pub(crate) struct HelperClient {
    events: mpsc::Sender<Event>,
}

impl HelperClient {
    pub fn start(options: HelperOptions) -> Result<Self, BotguardError> {
        let (events, receiver) = mpsc::channel();
        let mut manager = Manager {
            options,
            events: events.clone(),
            helper: None,
            next_id: 1,
            next_generation: 1,
        };
        spawn_thread("webview-minter", move || manager.run(&receiver))?;
        Ok(Self { events })
    }

    /// Queues a request; it is always finished, by its deadline.
    pub fn send(&self, job: Job) -> Result<(), BotguardError> {
        self.events
            .send(Event::Request(job))
            .map_err(|_| BotguardError::ServiceStopped)
    }
}

impl Drop for HelperClient {
    fn drop(&mut self) {
        let _ = self.events.send(Event::Shutdown);
    }
}

struct Pending {
    deadline: Instant,
    job: Job,
}

/// One running helper process.
struct Helper {
    generation: u64,
    child: Child,
    /// Lines for the stdin writer thread; dropping it closes the helper's stdin.
    stdin: mpsc::Sender<Vec<u8>>,
    pending: HashMap<u64, Pending>,
}

/// Why a helper is being stopped; decides what its pending requests get.
#[derive(Clone, Copy)]
enum Stop {
    Timeout,
    Failed(&'static str),
    Shutdown,
}

impl Stop {
    fn error(self) -> BotguardError {
        match self {
            Stop::Timeout => BotguardError::Timeout,
            Stop::Failed(message) => BotguardError::Other(message.to_owned()),
            Stop::Shutdown => BotguardError::ServiceStopped,
        }
    }
}

struct Manager {
    options: HelperOptions,
    /// Given to the pipe threads so they can report back.
    events: mpsc::Sender<Event>,
    helper: Option<Helper>,
    next_id: u64,
    next_generation: u64,
}

impl Manager {
    fn run(&mut self, receiver: &mpsc::Receiver<Event>) {
        loop {
            let event = match self.next_deadline() {
                Some(deadline) => {
                    receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                }
                None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
            };
            match event {
                Ok(Event::Request(job)) => self.request(job),
                Ok(Event::Line { generation, line }) => self.on_line(generation, &line),
                Ok(Event::Exited { generation }) => {
                    if self.is_current(generation) {
                        tracing::warn!(
                            "the webview minter exited; it will be restarted on the next request"
                        );
                        self.stop(Stop::Failed("the webview minter exited unexpectedly"));
                    }
                }
                Ok(Event::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    self.stop(Stop::Shutdown);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            // An overdue request means the helper hangs, so it is killed; the
            // next request starts a fresh one.
            if self.next_deadline().is_some_and(|d| d <= Instant::now()) {
                tracing::warn!("the webview minter did not answer in time; restarting it");
                self.stop(Stop::Timeout);
            }
        }
    }

    fn request(&mut self, job: Job) {
        if self.helper.is_none() {
            match self.spawn() {
                Ok(helper) => self.helper = Some(helper),
                Err(error) => {
                    tracing::warn!("could not start the webview helper: {error}");
                    job.finish(Err(BotguardError::Other(format!(
                        "could not start the webview minter: {error}"
                    ))));
                    return;
                }
            }
        }
        let Some(helper) = self.helper.as_mut() else {
            return;
        };
        let id = self.next_id;
        self.next_id += 1;
        let line = match encode_line(&HelperRequest {
            id,
            action: job.action(),
        }) {
            Ok(line) => line,
            Err(error) => return job.finish(Err(error)),
        };
        let timeout = match job {
            Job::Prewarm => self.options.mint_timeout,
            Job::Resolve { .. } => self.options.resolve_timeout,
        };
        let deadline = Instant::now() + timeout;
        helper.pending.insert(id, Pending { deadline, job });
        if helper.stdin.send(line).is_err() {
            // The writer thread is gone, so the helper is unusable.
            self.stop(Stop::Failed(
                "the webview minter stopped accepting requests",
            ));
        }
    }

    fn on_line(&mut self, generation: u64, line: &str) {
        let Some(helper) = self.helper.as_mut().filter(|h| h.generation == generation) else {
            return;
        };
        let Some(reply) = decode_line::<HelperReply>(line) else {
            tracing::debug!("ignoring unexpected output from the webview minter: {line:.200}");
            return;
        };
        if let Some(pending) = helper.pending.remove(&reply.id) {
            pending.job.finish(Ok(reply));
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        self.helper
            .as_ref()
            .is_some_and(|h| h.generation == generation)
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.helper
            .as_ref()
            .and_then(|h| h.pending.values().map(|p| p.deadline).min())
    }

    /// Ends the current helper, failing its outstanding requests.
    fn stop(&mut self, reason: Stop) {
        let Some(helper) = self.helper.take() else {
            return;
        };
        for (_, pending) in helper.pending {
            pending.job.finish(Err(reason.error()));
        }
        // Closing stdin asks the helper to exit by itself.
        drop(helper.stdin);
        retire(helper.child, matches!(reason, Stop::Shutdown));
    }

    fn spawn(&mut self) -> std::io::Result<Helper> {
        let generation = self.next_generation;
        self.next_generation += 1;

        let mut command = Command::new(&self.options.program);
        command
            .args(&self.options.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // Never flash a console window for the helper.
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }
        let started = Instant::now();
        let mut child = command.spawn()?;
        tracing::info!(
            "started the webview minter (pid {}) in {} ms",
            child.id(),
            started.elapsed().as_millis()
        );

        let pipes = (child.stdin.take(), child.stdout.take(), child.stderr.take());
        let (Some(mut stdin), Some(stdout), Some(stderr)) = pipes else {
            retire(child, false);
            return Err(std::io::Error::other("helper pipes missing"));
        };
        let (lines, queue) = mpsc::channel::<Vec<u8>>();
        let events = self.events.clone();
        let threads = spawn_thread("webview-minter-stdin", move || {
            for line in queue {
                if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
                    // The helper is gone; the stdout reader reports that.
                    return;
                }
            }
        })
        .and_then(|()| {
            spawn_thread("webview-minter-stdout", move || {
                for_each_line(stdout, |line| {
                    let line = line.to_owned();
                    events.send(Event::Line { generation, line }).is_ok()
                });
                let _ = events.send(Event::Exited { generation });
            })
        })
        .and_then(|()| {
            // Copies the helper's stderr (its diagnostics) into our log.
            spawn_thread("webview-minter-stderr", move || {
                for_each_line(stderr, |line| {
                    let text = line.trim_end();
                    if !text.is_empty() {
                        tracing::info!(target: "elizabeats_botguard::helper", "{text:.500}");
                    }
                    true
                });
            })
        });
        if let Err(error) = threads {
            retire(child, false);
            return Err(error);
        }
        Ok(Helper {
            generation,
            child,
            stdin: lines,
            pending: HashMap::new(),
        })
    }
}

/// Makes sure a helper process ends and is reaped, without blocking the
/// manager. A graceful stop gives it a moment to exit after stdin closed.
fn retire(mut child: Child, graceful: bool) {
    let reap = move || {
        if graceful {
            let until = Instant::now() + Duration::from_secs(2);
            while Instant::now() < until {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    };
    if spawn_thread("webview-minter-reaper", reap).is_err() {
        tracing::warn!("could not reap the webview minter process");
    }
}
