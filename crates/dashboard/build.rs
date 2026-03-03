#![allow(clippy::disallowed_methods, reason = "build scripts are exempt")]
use std::process::Command;

fn main() {
    // Extract ProTools version from git tags (v1.0.0 → 1.0.0).
    // Falls back to CARGO_PKG_VERSION if no tags exist.
    println!("cargo:rerun-if-changed=../../.git/refs/tags");
    println!("cargo:rerun-if-changed=../../.git/logs/HEAD");
    let protools_version = if let Ok(output) = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .output()
        && output.status.success()
    {
        let tag = String::from_utf8_lossy(&output.stdout).trim().to_string();
        tag.strip_prefix('v').unwrap_or(&tag).to_string()
    } else {
        std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0-dev".to_string())
    };
    println!("cargo:rustc-env=PROTOOLS_VERSION={protools_version}");
}
