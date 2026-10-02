use std::{env, process::Command};

fn set_build_revision() {
    if env::var("BUILD_REVISION").is_ok_and(|r| !r.is_empty()) {
        return;
    }

    // Outside a git checkout (e.g. building from a source tarball), fall
    // back to the package version instead of failing the build.
    let output = match Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => {
            let revision = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string());
            println!("cargo:rustc-env=BUILD_REVISION={revision}");
            return;
        }
    };

    let build_revision = String::from_utf8(output.stdout).expect("Invalid UTF-8 sequence");

    // `git status --porcelain` reports both modified and untracked files,
    // unlike `git diff --quiet`.
    let is_dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .is_ok_and(|o| !o.stdout.is_empty());

    let dirty_suffix = if is_dirty { "-dirty" } else { "" };

    let final_build_revision = format!("{}{}", build_revision.trim(), dirty_suffix);
    println!("cargo:rustc-env=BUILD_REVISION={final_build_revision}");
}

fn main() {
    set_build_revision()
}
