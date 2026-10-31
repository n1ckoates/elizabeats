//! Builds the webview helper's JS bundles (youtubei.js, bgutils-js) from
//! `js/` into Cargo's `OUT_DIR`, where `src/helper.rs` embeds them.
//!
//! Needs Node 22+ and pnpm. `pnpm install` only runs when `js/pnpm-lock.yaml`
//! changed since the last install; afterwards builds work offline.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap_or_default());
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or_default());
    let js = manifest_dir.join("js");

    for path in [
        "js/src",
        "js/build.mjs",
        "js/package.json",
        "js/pnpm-lock.yaml",
        "js/pnpm-workspace.yaml",
    ] {
        println!("cargo::rerun-if-changed={path}");
    }

    if let Err(message) = install_if_needed(&js).and_then(|()| bundle(&js, &out_dir)) {
        // One `cargo::error` per line: Cargo doesn't show a build script's
        // own output when it reports errors this way.
        for line in message.lines() {
            println!("cargo::error={line}");
        }
    }
}

/// Runs `pnpm install` unless `js/node_modules` matches the current lockfile.
fn install_if_needed(js: &Path) -> Result<(), String> {
    let lockfile = std::fs::read(js.join("pnpm-lock.yaml"))
        .map_err(|error| format!("could not read js/pnpm-lock.yaml: {error}"))?;
    // A copy of the lockfile that was last installed.
    let stamp = js.join("node_modules").join(".elizabeats-installed-lock");
    if std::fs::read(&stamp).is_ok_and(|installed| installed == lockfile) {
        return Ok(());
    }
    run("pnpm", &["install", "--frozen-lockfile"], js)?;
    std::fs::write(&stamp, &lockfile)
        .map_err(|error| format!("could not write {}: {error}", stamp.display()))
}

fn bundle(js: &Path, out_dir: &Path) -> Result<(), String> {
    let out_dir = out_dir.to_string_lossy();
    run("node", &["build.mjs", &out_dir], js)
}

/// Runs `name` in `dir`. On failure, the error includes the end of the
/// program's output, so CI logs show what went wrong.
fn run(name: &str, args: &[&str], dir: &Path) -> Result<(), String> {
    let output = spawn(name, args, dir).map_err(|error| {
        format!(
            "could not run `{name}` ({error}). Building Elizabeats needs Node 22+ and pnpm \
             to bundle the YouTube JavaScript (crates/botguard/js); install them and build again."
        )
    })?;
    if output.status.success() {
        return Ok(());
    }
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(30)..].join("\n");
    Err(format!(
        "`{name}` failed ({}) while building crates/botguard/js:\n{tail}",
        output.status
    ))
}

/// Starts `name`, falling back to its `.cmd` shim on Windows (where tools
/// installed through npm are `.cmd` files rather than executables).
fn spawn(name: &str, args: &[&str], dir: &Path) -> std::io::Result<std::process::Output> {
    let output = Command::new(name).args(args).current_dir(dir).output();
    match output {
        Err(error) if cfg!(windows) && error.kind() == std::io::ErrorKind::NotFound => {
            Command::new(format!("{name}.cmd"))
                .args(args)
                .current_dir(dir)
                .output()
        }
        other => other,
    }
}
