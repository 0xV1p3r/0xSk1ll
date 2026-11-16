//! Records the commit the binaries were built from (`SK1LL_COMMIT`): taken
//! from the environment if set (release and Docker builds), else from git.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=SK1LL_COMMIT");
    let git_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.git");
    for file in ["HEAD", "packed-refs"] {
        println!("cargo:rerun-if-changed={}", git_dir.join(file).display());
    }
    if let Ok(head) = std::fs::read_to_string(git_dir.join("HEAD"))
        && let Some(reference) = head.trim().strip_prefix("ref: ")
    {
        println!("cargo:rerun-if-changed={}", git_dir.join(reference).display());
    }
    let commit = std::env::var("SK1LL_COMMIT").ok().filter(|c| !c.trim().is_empty()).or_else(|| {
        let out = Command::new("git").args(["rev-parse", "--short=10", "HEAD"]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    });
    println!("cargo:rustc-env=SK1LL_COMMIT={}", commit.as_deref().map_or("unknown", str::trim));
}
