// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// SPDX-License-Identifier: MPL-2.0

// Build scripts fail on errors.
#![allow(
    clippy::unwrap_used,
    clippy::unnecessary_wraps,
    clippy::uninlined_format_args
)]

use std::env;
use std::fs::File;
use std::io::{Result, Write};
use std::path::Path;
use std::process::Command;

fn main() -> Result<()> {
    rerun_if_git_changed();

    // A build without git, for example in Docker, gets the version from `UC_VERSION`.
    let (git_version, git_dirty) = match env::var("UC_VERSION") {
        Ok(v) if !v.trim().is_empty() => (Some(v.trim().trim_start_matches('v').to_owned()), false),
        _ => (get_git_version(), is_git_dirty()),
    };
    let release_date = env::var("UC_RELEASE_DATE")
        .ok()
        .filter(|d| !d.trim().is_empty());

    built::write_built_file().expect("Failed to acquire build-time information");

    // Write additional git info to built.rs file to supplement what built crate provides
    write_git_info(git_version, git_dirty, release_date);

    Ok(())
}

fn get_git_version() -> Option<String> {
    // First, try to get version from git
    if let Ok(output) = Command::new("git")
        .args(["describe", "--match", "v[0-9]*", "--tags", "HEAD"])
        .output()
        && output.status.success()
        && let Ok(version) = String::from_utf8(output.stdout)
    {
        return Some(version.trim().trim_start_matches('v').to_string());
    }

    // Fallback: try to get commit hash only
    if let Ok(output) = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        && output.status.success()
        && let Ok(commit) = String::from_utf8(output.stdout)
    {
        return Some(commit.trim().to_string());
    }

    None
}

/// Run the build script again when the commit or tags change.
///
/// Any `rerun-if-changed` turns off the default check of all package files, so the files used by
/// this script and `built` are listed too.
fn rerun_if_git_changed() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=Cargo.lock");
    // Changed source files, for the dirty flag. Cargo checks all files in a folder.
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=resources");
    println!("cargo:rerun-if-env-changed=UC_VERSION");
    println!("cargo:rerun-if-env-changed=UC_RELEASE_DATE");

    let Ok(output) = Command::new("git")
        .args(["rev-parse", "--git-dir", "--git-common-dir"])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return;
    };
    let mut lines = text.lines();
    let (Some(git_dir), Some(common_dir)) = (lines.next(), lines.next()) else {
        return;
    };
    let git_dir = Path::new(git_dir);
    let common_dir = Path::new(common_dir);

    let head = git_dir.join("HEAD");
    rerun_if_exists(&head);
    // The file of the current branch, for example `refs/heads/main`.
    if let Ok(head) = std::fs::read_to_string(&head)
        && let Some(reference) = head.strip_prefix("ref: ")
    {
        rerun_if_exists(&common_dir.join(reference.trim()));
    }
    rerun_if_exists(&common_dir.join("refs/tags"));
    rerun_if_exists(&common_dir.join("packed-refs"));
    // Changes to tracked files, for the dirty flag.
    rerun_if_exists(&git_dir.join("index"));
}

fn rerun_if_exists(path: &Path) {
    if path.exists() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn is_git_dirty() -> bool {
    // Check if there are any uncommitted changes. `git diff` compares the content, so files that
    // were touched but not changed are not dirty. It does not write the index.
    Command::new("git")
        .args(["--no-optional-locks", "diff", "--quiet", "HEAD", "--"])
        .status()
        .is_ok_and(|s| s.code() == Some(1))
}

fn write_git_info(git_version: Option<String>, git_dirty: bool, release_date: Option<String>) {
    let out_dir = env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("git_built.rs");
    let mut f = File::create(&dest_path).unwrap();

    writeln!(f, "// Git information generated at build time").unwrap();

    if let Some(git_version) = git_version {
        writeln!(
            f,
            "pub const GIT_VERSION: Option<&'static str> = Some({:?});",
            git_version
        )
        .unwrap();
    } else {
        writeln!(f, "pub const GIT_VERSION: Option<&'static str> = None;").unwrap();
    }

    if git_dirty {
        writeln!(f, "pub const GIT_DIRTY: Option<bool> = Some(true);").unwrap();
    } else {
        writeln!(f, "pub const GIT_DIRTY: Option<bool> = None;").unwrap();
    }

    writeln!(
        f,
        "pub const RELEASE_DATE: Option<&'static str> = {:?};",
        release_date.as_deref().map(str::trim)
    )
    .unwrap();
}
