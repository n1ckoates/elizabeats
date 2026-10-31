//! Manual listening test.
//!
//! ```sh
//! cargo run -p elizabeats-audio --example play -- a.mp3 b.m4a
//! ```
//!
//! Plays the first file, then after `FIRST_SECS` seconds (default 10, or
//! when the first file is about to end) crossfades over 3 s into the next
//! one, and so on. Exits after the last file ends.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use elizabeats_audio::{AudioEngine, AudioEvent};

const CROSSFADE: Duration = Duration::from_secs(3);

fn main() {
    let files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if files.is_empty() {
        eprintln!("usage: play <file> [<file> ...]");
        std::process::exit(2);
    }
    let first_secs: f64 = std::env::var("FIRST_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10.0);

    let (tx, rx) = mpsc::channel();
    let engine = AudioEngine::new(move |event| {
        let _ = tx.send(event);
    });

    let mut current: u64 = 0;
    engine.play(current, files[0].clone());
    println!("playing #0 {}", files[0].display());
    let mut started = Instant::now();
    let mut crossfading = false;
    let mut last_print = Instant::now();

    loop {
        let Ok(event) = rx.recv_timeout(Duration::from_millis(50)) else {
            continue;
        };
        match event {
            AudioEvent::Progress {
                id,
                position_secs,
                duration_secs,
            } => {
                if last_print.elapsed() > Duration::from_secs(1) {
                    last_print = Instant::now();
                    println!("  #{id} {position_secs:6.2}s / {duration_secs:?}");
                }
                let near_end =
                    duration_secs.is_some_and(|d| d - position_secs <= CROSSFADE.as_secs_f64());
                let next = current as usize + 1;
                if id == current
                    && !crossfading
                    && next < files.len()
                    && (started.elapsed().as_secs_f64() >= first_secs || near_end)
                {
                    crossfading = true;
                    println!("crossfading to #{next} {}", files[next].display());
                    engine.crossfade(next as u64, files[next].clone(), CROSSFADE);
                }
            }
            AudioEvent::CrossfadeComplete { outgoing, incoming } => {
                println!("crossfade complete: #{outgoing} -> #{incoming}");
                engine.promote_incoming(incoming);
                current = incoming;
                crossfading = false;
                started = Instant::now();
            }
            AudioEvent::Ended { id } => {
                println!("ended #{id}");
                if crossfading && id == current {
                    // Outgoing ran out mid-fade; the engine promoted the next one.
                    current += 1;
                    crossfading = false;
                    started = Instant::now();
                } else if id == current {
                    break;
                }
            }
            AudioEvent::Failed { id, message } => {
                eprintln!("failed #{id}: {message}");
                break;
            }
        }
    }
    engine.stop();
}
