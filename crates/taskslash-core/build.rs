//! Sets `TASKSLASH_VERSION` to the crate's version and the short commit it is built from,
//! `0.1.0 (ccdb25a)`, or the version alone outside a git checkout, so `--version` and `status` can
//! name the build.

use std::path::PathBuf;
use std::process::Command;

/// What `git <args>` prints, trimmed, when it runs and exits 0.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn main() {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    let version = match git(&["rev-parse", "--short=7", "HEAD"]) {
        Some(commit) => format!("{version} ({commit})"),
        None => version,
    };
    println!("cargo:rustc-env=TASKSLASH_VERSION={version}");
    // Rebuilt when HEAD moves: HEAD itself (a checkout), the branch it names (a commit) and
    // packed-refs (a gc). A worktree's HEAD sits in its own git dir, its refs in the common one.
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
    }
    if let Some(common) = git(&["rev-parse", "--path-format=absolute", "--git-common-dir"]) {
        let common = PathBuf::from(common);
        // Only files that exist: cargo reruns a build script on every build while one it
        // watches is missing (no packed-refs before a gc, a branch ref that is only packed).
        let branch = git(&["symbolic-ref", "-q", "HEAD"]).map(|b| common.join(b));
        for path in std::iter::once(common.join("packed-refs")).chain(branch) {
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            }
        }
    }
    println!("cargo:rerun-if-changed=build.rs");
}
