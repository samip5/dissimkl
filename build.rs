//! Derives the version string the binary reports, exposed as `DISSIMKL_VERSION`.
//!
//! A release build is stamped with the tag CI is building, which makes the tag
//! the single authoritative version — `Cargo.toml` stays at `0.0.0-dev` and only
//! seeds dev builds. Everything else is stamped with the commit it came from, so
//! a binary built from `main` can never be mistaken for a release.
//!
//! Deriving this here rather than rewriting `Cargo.toml` in the workflow keeps
//! `cargo build --locked` usable: the lockfile pins the package's own version,
//! so editing the manifest mid-workflow would fail the lockfile check.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=DISSIMKL_RELEASE_VERSION");
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    watch_git_head();

    let version = release_version().unwrap_or_else(dev_version);
    println!("cargo:rustc-env=DISSIMKL_VERSION={version}");
}

/// The release tag, with any `v` prefix stripped (`v1.2.3` → `1.2.3`).
///
/// An empty value counts as absent so the workflow can pass the variable
/// unconditionally and let a single expression decide tag vs. dev.
fn release_version() -> Option<String> {
    let raw = std::env::var("DISSIMKL_RELEASE_VERSION").ok()?;
    let tag = raw.trim();
    let tag = tag.strip_prefix('v').unwrap_or(tag);
    (!tag.is_empty()).then(|| tag.to_string())
}

/// `<manifest version>+g<short sha>[.dirty]`, e.g. `0.0.0-dev+g2cd0b57`.
///
/// The commit is dropped when there is nothing to read it from (a source
/// tarball with no checkout), leaving the bare manifest version rather than
/// failing the build over a cosmetic string.
fn dev_version() -> String {
    let base = dev_base();
    let Some(sha) = short_sha() else {
        return base;
    };
    let dirty = if is_dirty() { ".dirty" } else { "" };
    format!("{base}+g{sha}{dirty}")
}

/// The manifest version, which carries its own `-dev` pre-release tag.
///
/// One is appended if it ever loses it, so that "a dev build never reports a
/// bare release number" holds no matter what the manifest says.
fn dev_base() -> String {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    if version.contains('-') {
        version
    } else {
        format!("{version}-dev")
    }
}

fn short_sha() -> Option<String> {
    if let Some(sha) = git(&["rev-parse", "--short=7", "HEAD"]) {
        return Some(sha);
    }
    // No usable checkout — fall back to what the CI runner tells us.
    let sha = std::env::var("GITHUB_SHA").ok()?;
    let sha = sha.trim();
    (sha.len() >= 7).then(|| sha[..7].to_string())
}

/// True when tracked files differ from HEAD, so a locally patched build is
/// labelled as such instead of claiming to be exactly its commit.
fn is_dirty() -> bool {
    git(&["status", "--porcelain", "--untracked-files=no"])
        .is_some_and(|out| !out.is_empty())
}

/// Run a git command, returning its trimmed stdout, or `None` if git is
/// missing or the command failed.
fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8(output.stdout).ok()?.trim().to_string())
}

/// Rebuild when the checked-out commit changes.
///
/// Printing any `rerun-if-changed` replaces Cargo's default "rerun when any
/// package file changed" heuristic, so without this a dev build would keep
/// reporting whatever commit it was first compiled at.
fn watch_git_head() {
    let git_dir = Path::new(".git");
    let head = git_dir.join("HEAD");
    if !head.exists() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());

    // On a branch, HEAD only names a ref — the commit lives in the ref file,
    // which is what actually changes on commit. A detached HEAD holds the sha
    // directly, and a packed ref has no file to watch; both are fine to skip.
    if let Ok(contents) = std::fs::read_to_string(&head) {
        if let Some(reference) = contents.trim().strip_prefix("ref: ") {
            let ref_path = git_dir.join(reference);
            if ref_path.exists() {
                println!("cargo:rerun-if-changed={}", ref_path.display());
            }
        }
    }
}
