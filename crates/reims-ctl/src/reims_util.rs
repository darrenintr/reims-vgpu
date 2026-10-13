//! Runs reims-util to make OpenCore images and installs the pinned version beside reims-ctl.
//! `REIMS_CTL_UTIL` selects another binary; its JSON events become progress messages.

use crate::opencore::Identity;
use crate::paths;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const REPO: &str = "https://github.com/steelbrain/reims-util";
/// The commit [`install`] installs.
pub const REV: &str = "5405b2d13b6bea4c6db33f68cdfc751489c26855";

/// The reims-util binary: `REIMS_CTL_UTIL`, else the one beside reims-ctl, where both are
/// installed, else the first on PATH.
pub fn binary() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("REIMS_CTL_UTIL") {
        return Ok(PathBuf::from(p));
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.join("reims-util")));
    let on_path: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| {
            std::env::split_paths(&p)
                .map(|d| d.join("reims-util"))
                .collect()
        })
        .unwrap_or_default();
    beside
        .into_iter()
        .chain(on_path)
        .find(|p| p.is_file())
        .context("reims-util is not installed: run reims-ctl update, which installs it")
}

/// `reims-util --version`: "reims-util 0.1.0".
pub fn version(bin: &Path) -> Option<String> {
    let o = Command::new(bin).arg("--version").output().ok()?;
    let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
    (o.status.success() && !v.is_empty()).then_some(v)
}

/// Installs reims-util at [`REV`] with `cargo install`, into the bin directory it installed
/// reims-ctl into, building in the cache directory from `repo`, the reims-vgpu checkout, whose
/// rust-toolchain.toml picks the toolchain. cargo leaves an install of the same commit as it is.
pub fn install(repo: &Path) -> Result<()> {
    let cargo = which("cargo")
        .or_else(|| {
            let home = std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| paths::home().join(".cargo"));
            Some(home.join("bin/cargo")).filter(|p| p.is_file())
        })
        .context("cargo is not installed")?;
    let status = Command::new(&cargo)
        .args([
            "install", "--locked", "--quiet", "--git", REPO, "--rev", REV,
        ])
        .arg("reims-util")
        .env("CARGO_TARGET_DIR", paths::cache_dir().join("util-target"))
        .current_dir(repo)
        .stdin(Stdio::null())
        .status()
        .with_context(|| format!("run {}", cargo.display()))?;
    if !status.success() {
        bail!("cargo install of reims-util failed ({status})");
    }
    Ok(())
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

#[derive(Deserialize)]
struct OpencoreResult {
    smbios: Smbios,
}

#[derive(Deserialize)]
struct Smbios {
    model: String,
    serial: String,
}

/// Makes an OpenCore image at `image`, with `OVMF_CODE.fd` and `OVMF_VARS.fd` beside it, for a VM
/// whose network card is `mac`, which becomes its ROM, as a Mac's ROM is its first network
/// card's address, and which installs macOS `version` where it is known.
pub fn opencore(
    image: &Path,
    mac: &str,
    version: Option<&str>,
    progress: &mut dyn FnMut(&str),
) -> Result<()> {
    let mut args = vec!["--rom", mac];
    if let Some(m) = version.and_then(model_for) {
        args.extend(["--model", m]);
    }
    make_opencore(image, &args, progress)
}

/// Makes an OpenCore image at `image` presenting the Mac `id`, for a VM that has its OVMF: no
/// OVMF is written.
pub fn opencore_as(image: &Path, id: &Identity, progress: &mut dyn FnMut(&str)) -> Result<()> {
    let args = [
        "--model",
        &id.model,
        "--serial",
        &id.serial,
        "--mlb",
        &id.mlb,
        "--uuid",
        &id.uuid,
        "--rom",
        &id.rom,
        "--no-ovmf",
    ];
    make_opencore(image, &args, progress)
}

fn make_opencore(image: &Path, args: &[&str], progress: &mut dyn FnMut(&str)) -> Result<()> {
    let mut all: Vec<&OsStr> = vec!["opencore".as_ref(), "-o".as_ref(), image.as_os_str()];
    all.extend(args.iter().map(OsStr::new));
    let r = run(&all, progress)?;
    if let Ok(o) = serde_json::from_value::<OpencoreResult>(r) {
        progress(&format!(
            "OpenCore presents the VM as an {}, serial {}",
            o.smbios.model, o.smbios.serial
        ));
    }
    Ok(())
}

/// The Mac model an OpenCore image presents for macOS `version` ("10.14", "26.6.2"), where it is
/// not reims-util's default: the iMac20,1 runs macOS 10.15.6 to 26, and the iMacPro1,1 10.13 to
/// 15.
pub fn model_for(version: &str) -> Option<&'static str> {
    let mut parts = version.trim().split('.');
    let minor = parts.nth(1).and_then(|m| m.parse::<u32>().ok());
    match (major(version), minor) {
        (10, Some(m)) if m < 15 => Some("iMacPro1,1"),
        _ => None,
    }
}

fn major(version: &str) -> u32 {
    version
        .trim()
        .split('.')
        .next()
        .and_then(|m| m.parse().ok())
        .unwrap_or(0)
}

#[derive(Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Step { message: String },
    Progress { done: u64, total: Option<u64> },
    Note { message: String },
    Done { result: serde_json::Value },
    Error { message: String },
}

/// Runs reims-util with `--json` and `args`, passing its steps and notes to `progress`, and its
/// progress as a whole percent of its step, at most once a second but for the first and the
/// last; returns its result.
fn run(args: &[&OsStr], progress: &mut dyn FnMut(&str)) -> Result<serde_json::Value> {
    let bin = binary()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("--json")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    end_with_parent(&mut cmd);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("run {}", bin.display()))?;
    let stderr = child.stderr.take();
    let err_thread = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            let _ = e.read_to_string(&mut s);
        }
        s
    });
    let mut step = String::new();
    let mut shown: Option<(u64, Instant)> = None;
    let mut result = None;
    let stdout = child.stdout.take().context("reims-util's output")?;
    for line in BufReader::new(stdout).lines() {
        let Ok(event) = serde_json::from_str::<Event>(&line?) else {
            continue;
        };
        match event {
            Event::Step { message } => {
                step = lower_first(&message);
                shown = None;
                progress(&step);
            }
            Event::Note { message } => progress(&message),
            Event::Progress { done, total } => {
                let (at, last, text) = match total {
                    Some(t) if t > 0 => {
                        let pct = done.min(t) * 100 / t;
                        (pct, pct == 100, format!("{step}: {pct}% of {}", size(t)))
                    }
                    // Nothing yet of a size not known yet says nothing.
                    _ if done == 0 => continue,
                    _ => (done / 100_000_000, false, format!("{step}: {}", size(done))),
                };
                let due = match shown {
                    None => true,
                    Some((a, t)) => a != at && (last || t.elapsed() >= Duration::from_secs(1)),
                };
                if due {
                    shown = Some((at, Instant::now()));
                    progress(&text);
                }
            }
            Event::Done { result: r } => result = Some(Ok(r)),
            Event::Error { message } => result = Some(Err(message)),
        }
    }
    let status = child.wait()?;
    let err = err_thread.join().unwrap_or_default();
    match result {
        Some(Ok(r)) if status.success() => Ok(r),
        Some(Err(m)) => bail!("{m}"),
        _ => bail!("reims-util failed ({status}): {}", err.trim()),
    }
}

/// "Downloading OpenCore 1.0.8" as reims-ctl's steps read: "downloading OpenCore 1.0.8". A step
/// that starts with a name, "OVMF …", stays as it is.
fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    match (c.next(), c.next()) {
        (Some(a), Some(b)) if a.is_uppercase() && b.is_lowercase() => {
            a.to_lowercase().chain(s[a.len_utf8()..].chars()).collect()
        }
        _ => s.to_string(),
    }
}

/// "6 MB", "1.1 GB".
fn size(bytes: u64) -> String {
    if bytes < 1_000_000_000 {
        format!("{} MB", bytes.div_ceil(1_000_000))
    } else {
        format!("{:.1} GB", bytes as f64 / 1e9)
    }
}

/// On Linux, has reims-util end when the reims-ctl thread that started it does, which waits for
/// it, so with reims-ctl: a download stops where it is when the interface quits, and the next
/// one continues it.
fn end_with_parent(cmd: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: prctl, getppid and _exit are async-signal-safe, as pre_exec requires.
        unsafe {
            cmd.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent ended before the signal was asked for.
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = cmd;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_macos_gets_a_model_that_runs_it() {
        assert_eq!(model_for("10.13"), Some("iMacPro1,1"));
        assert_eq!(model_for("10.14"), Some("iMacPro1,1"));
        assert_eq!(model_for("10.15"), None);
        assert_eq!(model_for("26"), None);
        assert_eq!(model_for("26.6.2"), None);
        assert_eq!(model_for(""), None);
    }

    #[test]
    fn steps_read_as_reims_ctls() {
        assert_eq!(
            lower_first("Downloading OpenCore 1.0.8"),
            "downloading OpenCore 1.0.8"
        );
        assert_eq!(lower_first("OVMF's code"), "OVMF's code");
        assert_eq!(size(6_200_000), "7 MB");
        assert_eq!(size(1_100_000_000), "1.1 GB");
    }

    /// A stand-in for reims-util that prints `lines` and exits with `code`.
    fn fake(dir: &Path, lines: &str, code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("reims-util");
        std::fs::write(
            &p,
            format!("#!/bin/sh\ncat <<'EOF'\n{lines}\nEOF\necho oops >&2\nexit {code}\n"),
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn run_passes_steps_and_percents_a_second_apart_and_returns_the_result() {
        let d = std::env::temp_dir().join(format!("reims-ctl-util-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let lines = r#"{"event":"step","message":"Downloading the macOS Tahoe 26 recovery image"}
{"event":"progress","done":0,"total":2000000000}
{"event":"progress","done":10000000,"total":2000000000}
{"event":"progress","done":20000000,"total":2000000000}
{"event":"progress","done":40000000,"total":2000000000}
{"event":"progress","done":2000000000,"total":2000000000}
{"event":"step","message":"Downloading OpenCore 1.0.8"}
{"event":"progress","done":0,"total":null}
{"event":"progress","done":11000000,"total":11000000}
{"event":"note","message":"Wrote it"}
not json
{"event":"done","result":{"title":"macOS Tahoe 26","image":"/x.dmg"}}"#;
        let bin = fake(&d, lines, 0);
        // SAFETY: the only test that sets it, before its own reads.
        unsafe { std::env::set_var("REIMS_CTL_UTIL", &bin) };
        let mut seen = Vec::new();
        let r = run(&["online-installer".as_ref()], &mut |s| {
            seen.push(s.to_string())
        })
        .unwrap();
        assert_eq!(r["image"], "/x.dmg");
        assert_eq!(
            seen,
            [
                "downloading the macOS Tahoe 26 recovery image",
                "downloading the macOS Tahoe 26 recovery image: 0% of 2.0 GB",
                "downloading the macOS Tahoe 26 recovery image: 100% of 2.0 GB",
                "downloading OpenCore 1.0.8",
                "downloading OpenCore 1.0.8: 100% of 11 MB",
                "Wrote it",
            ]
        );
        // Its error event is the error; without one, its stderr.
        fake(
            &d,
            r#"{"event":"error","message":"no online installer for \"x\""}"#,
            1,
        );
        let e = run(&[], &mut |_| {}).unwrap_err();
        assert_eq!(format!("{e:#}"), "no online installer for \"x\"");
        fake(&d, "", 1);
        let e = format!("{:#}", run(&[], &mut |_| {}).unwrap_err());
        assert!(e.ends_with(": oops"), "{e}");
        unsafe { std::env::remove_var("REIMS_CTL_UTIL") };
        std::fs::remove_dir_all(&d).unwrap();
    }
}
