//! Stamps the version the app shows, and on Windows embeds the app icon.
//!
//! Version: release builds (built with `ELIZABEATS_RELEASE=1`, which only the
//! release workflow sets) show the Cargo version, e.g. `1.0.0`. Every other
//! build shows `1.0.0-dev (a1b2c3d)` with its commit, and doesn't update
//! itself (the `elizabeats_release` cfg is off).
//!
//! Icon: embeds `assets/icon.ico` into `elizabeats.exe`, so the taskbar,
//! Start menu and installer shortcuts show it.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-env-changed=ELIZABEATS_RELEASE");
    println!("cargo::rerun-if-env-changed=GITHUB_SHA");
    println!("cargo::rustc-check-cfg=cfg(elizabeats_release)");

    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let release = std::env::var("ELIZABEATS_RELEASE").is_ok_and(|value| value == "1");
    let label = if release {
        println!("cargo::rustc-cfg=elizabeats_release");
        version
    } else {
        format!("{version}-dev ({})", commit())
    };
    println!("cargo::rustc-env=ELIZABEATS_VERSION_LABEL={label}");

    #[cfg(windows)]
    {
        println!("cargo::rerun-if-changed=../../assets/icon.ico");
        let mut resource = winresource::WindowsResource::new();
        resource.set_icon("../../assets/icon.ico");
        if let Err(error) = resource.compile() {
            println!("cargo::warning=could not embed the app icon: {error}");
        }
    }
}

/// The short commit hash: from CI's `GITHUB_SHA`, else from git, else "unknown".
fn commit() -> String {
    if let Ok(sha) = std::env::var("GITHUB_SHA") {
        return sha.chars().take(7).collect();
    }
    // Rebuild when HEAD moves (new commit or branch switch) so it stays current.
    let git = Path::new("../../.git");
    if git.is_dir() {
        println!("cargo::rerun-if-changed=../../.git/HEAD");
        if let Ok(head) = std::fs::read_to_string(git.join("HEAD"))
            && let Some(reference) = head.strip_prefix("ref: ")
        {
            println!("cargo::rerun-if-changed=../../.git/{}", reference.trim());
        }
    }
    Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|sha| !sha.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}
