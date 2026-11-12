//! Embeds the git commit into the binary (TIGHTWIRE_GIT_COMMIT) so log
//! headers, `doctor` and the tray menu tell builds apart. Falls back to
//! "unknown" when built outside a git checkout.

use std::process::Command;

mod winres;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    // Re-run when HEAD moves or the index changes (commits, checkouts, staging).
    for p in ["HEAD", "index"] {
        if let Some(path) = git(&["rev-parse", "--path-format=absolute", "--git-path", p]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let commit = git(&["rev-parse", "--short=10", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // Tracked-file changes only: untracked scratch files do not make a build "dirty".
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    let value = if dirty {
        format!("{commit}+dirty")
    } else {
        commit
    };
    println!("cargo:rustc-env=TIGHTWIRE_GIT_COMMIT={value}");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let manifest = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
        let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
        winres::embed_icon(
            &manifest.join("../assets/icon/tightwire.ico"),
            &out_dir,
            "tightwire-recv",
        );
    }
}
