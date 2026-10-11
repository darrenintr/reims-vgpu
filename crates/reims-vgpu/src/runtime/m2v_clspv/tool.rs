//! The external tools the metal2vk route runs: `llvm-dis`, the patched
//! `clspv`, and `spirv-val`.
//!
//! The route's own runner, so no metal2vulkan code sits in the compute path.
//! A tool named without a path is looked for in the same places metal2vulkan's
//! runner looks (Homebrew and `/usr/local` LLVM first, then `PATH`), so a host
//! that runs one finds the same binaries for the other.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Wall-clock cap for one tool run. A kernel compiles in about a second; a
/// stuck tool must not hold the translation worker.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(120);

/// The Vulkan environment every module is validated against: the device's
/// baseline (Vulkan 1.2).
pub(crate) const VULKAN_TARGET_ENV: &str = "vulkan1.2";

/// Where `cmd` runs from: itself when it names a path, else the first of the
/// known LLVM directories that holds it, else `PATH`.
fn resolve(cmd: &str) -> PathBuf {
    if cmd.contains('/') {
        return PathBuf::from(cmd);
    }
    for dir in [
        "/opt/homebrew/opt/llvm/bin",
        "/usr/local/opt/llvm/bin",
        "/opt/homebrew/bin",
        "/usr/local/bin",
    ] {
        let candidate = Path::new(dir).join(cmd);
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from(cmd)
}

/// Run `cmd` with `args`; `Ok(stderr)` on a zero exit, `Err(message)` on a
/// spawn failure, a non-zero exit, a signal, or [`TIMEOUT`].
pub(crate) fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let bin = resolve(cmd);
    let mut child = Command::new(&bin)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {cmd} ({}): {e}", bin.display()))?;
    // Drained on a thread so a chatty tool cannot fill the pipe and stall.
    let Some(mut stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{cmd}: no stderr pipe"));
    };
    let drain = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        text
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{cmd} timed out after {}s", TIMEOUT.as_secs()));
            }
            Err(e) => return Err(format!("{cmd}: wait failed: {e}")),
        }
    };
    let stderr = drain.join().unwrap_or_default();
    if status.success() {
        Ok(stderr)
    } else {
        Err(format!("{cmd} failed ({status}):\n{stderr}"))
    }
}

/// `spirv-val` over in-memory SPIR-V, written to `dir` for the tool's lifetime.
pub(crate) fn spirv_val(spirv: &[u8], dir: &Path) -> Result<(), String> {
    let path = dir.join("k.clspv.val.spv");
    std::fs::write(&path, spirv).map_err(|e| format!("write {}: {e}", path.display()))?;
    let result = run(
        "spirv-val",
        &[
            "--target-env",
            VULKAN_TARGET_ENV,
            path.to_str().unwrap_or("k.clspv.val.spv"),
        ],
    );
    let _ = std::fs::remove_file(&path);
    result.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_tool_reports_its_exit_and_a_missing_one_its_name() {
        assert!(run("true", &[]).is_ok());
        let failed = run("false", &[]).unwrap_err();
        assert!(failed.starts_with("false failed"), "{failed}");
        let missing = run("m2v-no-such-tool", &[]).unwrap_err();
        assert!(
            missing.starts_with("cannot run m2v-no-such-tool"),
            "{missing}"
        );
    }
}
