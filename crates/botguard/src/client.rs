//! App side of the webview helper: starts the helper process on first use,
//! sends mint and resolve requests to its stdin, matches the replies on its
//! stdout, enforces timeouts and replaces the helper when it crashes or hangs.
//!
//! All of this runs on one plain thread (the "manager") that never blocks on
//! the helper: pipes are read and written by small per-helper threads that
//! report back through the manager's queue. Async callers just await a
//! oneshot channel, so the app's async runtime is never blocked.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::protocol::{
    HelperReply, HelperRequest, RequestAction, decode_line, encode_line, read_bounded_line,
};
use crate::{AudioStream, BotguardError, MintedTokens};

/// Called exactly once with the outcome of a mint request.
pub(crate) type MintReply = Box<dyn FnOnce(Result<MintedTokens, BotguardError>) + Send>;
/// Called exactly once with the outcome of a resolve request.
pub(crate) type StreamReply = Box<dyn FnOnce(Result<AudioStream, BotguardError>) + Send>;

/// One request for the helper, with the callback for its answer.
pub(crate) enum Job {
    /// `identifiers` go to BotGuard as they are; an empty list only makes
    /// sure the helper is ready (prewarm).
    Mint {
        identifiers: Vec<String>,
        reply: MintReply,
    },
    Resolve {
        video_id: String,
        reply: StreamReply,
    },
}

impl Job {
    fn action(&self) -> RequestAction {
        match self {
            Job::Mint { identifiers, .. } => RequestAction::Mint {
                identifiers: identifiers.clone(),
            },
            Job::Resolve { video_id, .. } => RequestAction::Resolve {
                resolve: video_id.clone(),
            },
        }
    }

    fn answer(self, reply: HelperReply) {
        match self {
            Job::Mint {
                identifiers,
                reply: callback,
            } => {
                callback(reply.into_tokens(identifiers.len()));
            }
            Job::Resolve {
                reply: callback, ..
            } => callback(reply.into_stream()),
        }
    }

    fn fail(self, error: BotguardError) {
        match self {
            Job::Mint { reply, .. } => reply(Err(error)),
            Job::Resolve { reply, .. } => reply(Err(error)),
        }
    }
}

/// How to start the helper process. Public (via `HelperOptions`) so tests
/// can point it at a test binary or a fake.
#[derive(Clone, Debug)]
pub(crate) struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// How long one mint request may take, including starting the helper.
    pub mint_timeout: Duration,
    /// The same for one resolve request.
    pub resolve_timeout: Duration,
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
    pub fn start(launch: Launch) -> Result<Self, BotguardError> {
        let (events, receiver) = mpsc::channel();
        let mut manager = Manager {
            launch,
            events: events.clone(),
            helper: None,
            next_id: 1,
            next_generation: 1,
        };
        std::thread::Builder::new()
            .name("webview-minter".into())
            .spawn(move || manager.run(&receiver))?;
        Ok(Self { events })
    }

    /// Queues a request; its callback is always called, by its deadline.
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

struct Manager {
    launch: Launch,
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
                Ok(Event::Exited { generation }) => self.on_exit(generation),
                Ok(Event::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                    self.stop(Stop::Shutdown);
                    return;
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.expire_overdue();
        }
    }

    fn request(&mut self, job: Job) {
        if self.helper.is_none() {
            match self.spawn() {
                Ok(helper) => self.helper = Some(helper),
                Err(error) => {
                    tracing::warn!("could not start the webview helper: {error}");
                    job.fail(BotguardError::Other(format!(
                        "could not start the webview minter: {error}"
                    )));
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
            Err(error) => {
                job.fail(error);
                return;
            }
        };
        let timeout = match job {
            Job::Mint { .. } => self.launch.mint_timeout,
            Job::Resolve { .. } => self.launch.resolve_timeout,
        };
        helper.pending.insert(
            id,
            Pending {
                deadline: Instant::now() + timeout,
                job,
            },
        );
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
            pending.job.answer(reply);
        }
    }

    fn on_exit(&mut self, generation: u64) {
        if self
            .helper
            .as_ref()
            .is_some_and(|h| h.generation == generation)
        {
            tracing::warn!("the webview minter exited; it will be restarted on the next request");
            self.stop(Stop::Failed("the webview minter exited unexpectedly"));
        }
    }

    /// Fails overdue requests. An overdue request means the helper hangs, so
    /// it is killed; the next request starts a fresh one.
    fn expire_overdue(&mut self) {
        let now = Instant::now();
        let overdue = self
            .helper
            .as_ref()
            .is_some_and(|h| h.pending.values().any(|p| p.deadline <= now));
        if overdue {
            tracing::warn!("the webview minter did not answer in time; restarting it");
            self.stop(Stop::Timeout);
        }
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
        let Helper {
            child,
            stdin,
            pending,
            ..
        } = helper;
        for (_, request) in pending {
            request.job.fail(match reason {
                Stop::Timeout => BotguardError::Timeout,
                Stop::Failed(message) => BotguardError::Other(message.to_owned()),
                Stop::Shutdown => BotguardError::ServiceStopped,
            });
        }
        // Closing stdin asks the helper to exit by itself.
        drop(stdin);
        retire(child, matches!(reason, Stop::Shutdown));
    }

    fn spawn(&mut self) -> std::io::Result<Helper> {
        let generation = self.next_generation;
        self.next_generation += 1;

        let mut command = Command::new(&self.launch.program);
        command
            .args(&self.launch.args)
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
        let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
            retire(child, false);
            return Err(std::io::Error::other("helper pipes missing"));
        };
        let (lines, queue) = mpsc::channel();
        let threads = spawn_writer(stdin, queue)
            .and_then(|()| spawn_reader(stdout, generation, self.events.clone()))
            .and_then(|()| spawn_log_forwarder(stderr));
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

#[derive(Clone, Copy)]
enum Stop {
    Timeout,
    Failed(&'static str),
    Shutdown,
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
    if std::thread::Builder::new()
        .name("webview-minter-reaper".into())
        .spawn(reap)
        .is_err()
    {
        tracing::warn!("could not reap the webview minter process");
    }
}

fn spawn_writer(mut stdin: ChildStdin, queue: mpsc::Receiver<Vec<u8>>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("webview-minter-stdin".into())
        .spawn(move || {
            for line in queue {
                if stdin.write_all(&line).and_then(|()| stdin.flush()).is_err() {
                    // The helper is gone; the stdout reader reports that.
                    return;
                }
            }
        })
        .map(drop)
}

fn spawn_reader(
    stdout: std::process::ChildStdout,
    generation: u64,
    events: mpsc::Sender<Event>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("webview-minter-stdout".into())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match read_bounded_line(&mut reader, &mut line) {
                    Ok(true) => {
                        let line = String::from_utf8_lossy(&line).into_owned();
                        if events.send(Event::Line { generation, line }).is_err() {
                            return;
                        }
                    }
                    Ok(false) | Err(_) => break,
                }
            }
            let _ = events.send(Event::Exited { generation });
        })
        .map(drop)
}

/// Copies the helper's stderr (its diagnostics) into our log.
fn spawn_log_forwarder(stderr: std::process::ChildStderr) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("webview-minter-stderr".into())
        .spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut line = Vec::new();
            while let Ok(true) = read_bounded_line(&mut reader, &mut line) {
                let text = String::from_utf8_lossy(&line);
                let text = text.trim_end();
                if !text.is_empty() {
                    tracing::info!(target: "elizabeats_botguard::helper", "{text:.500}");
                }
                line.clear();
            }
        })
        .map(drop)
}
