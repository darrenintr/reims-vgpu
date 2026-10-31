#!/usr/bin/env python3
"""Download and switch signed-by-checksum reims-vgpu GitHub Release binaries.

No Git checkout, Rust, or QEMU build dependencies are required on the client.
GitHub release checksum is for transport corruption detection, not a signature.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.error
import urllib.request

REPO = "darrenintr/reims-vgpu"
ASSET = "reims-vgpu-linux-x86_64.tar.gz"
API = f"https://api.github.com/repos/{REPO}/releases"
DATA = Path(os.environ.get("REIMS_CTL_DATA_DIR") or
            Path(os.environ.get("XDG_DATA_HOME", str(Path.home() / ".local/share"))) / "reims-ctl")
ROOT = DATA / "qemu"
STATE = DATA / "release-manager.json"

def fetch(url):
    headers = {
        "User-Agent": "reims-vgpu-release-manager/1",
        "Accept": "application/vnd.github+json" if url.startswith(API) else "application/octet-stream",
    }
    # Never send a GitHub token to browser_download_url or its redirects.
    if url.startswith("https://api.github.com/"):
        token = os.environ.get("GH_TOKEN") or os.environ.get("GITHUB_TOKEN")
        if token:
            headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read()

def release_index():
    """Prefer the authenticated GitHub CLI to avoid the anonymous 60/hour IP limit.

    The CLI uses its existing credential store; no token is printed, persisted
    by this script or passed to asset-download redirects.
    """
    if shutil.which("gh"):
        try:
            result = subprocess.run(
                ["gh", "api", f"repos/{REPO}/releases?per_page=100"],
                check=True, capture_output=True, text=True, timeout=40,
            )
            return json.loads(result.stdout)
        except (subprocess.CalledProcessError, subprocess.TimeoutExpired, ValueError):
            # GitHub CLI may be installed but not logged in. A GH_TOKEN,
            # GITHUB_TOKEN, or unauthed API request can still work.
            pass
    try:
        return json.loads(fetch(API + "?per_page=100"))
    except urllib.error.HTTPError as exc:
        if exc.code in (403, 429):
            raise RuntimeError(
                "GitHub API rate limited (HTTP 403/429). Log in with "
                "`gh auth login` and retry; the downloader will use "
                "authenticated `gh api` automatically. Alternatively set "
                "GH_TOKEN or wait for GitHub's rate-limit reset."
            ) from exc
        raise

def fetch_releases(channel):
    items = release_index()
    for release in items:
        if release.get("draft"):
            continue
        tag = release.get("tag_name", "")
        if channel == "stable" and (release.get("prerelease") or not tag.startswith("v")):
            continue
        if channel == "testing" and (not release.get("prerelease") or not tag.startswith("testing-")):
            continue
        assets = {x["name"]: x for x in release.get("assets", [])}
        if ASSET in assets and "SHA256SUMS" in assets:
            return release, assets
    raise RuntimeError("No matching GitHub Release with Linux QEMU and SHA256SUMS was found")

def safe_tag(tag):
    if not tag or any(c not in "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._" for c in tag):
        raise RuntimeError("Unexpected release tag")
    return tag

def read_state():
    try:
        return json.loads(STATE.read_text())
    except (OSError, ValueError):
        return {}

def save_state(data):
    STATE.parent.mkdir(parents=True, exist_ok=True)
    temp = STATE.with_suffix(".tmp")
    temp.write_text(json.dumps(data, indent=2) + "\n")
    os.replace(temp, STATE)

def install(release, assets, channel):
    tag = safe_tag(release["tag_name"])
    dest = ROOT / tag
    checksum_file = fetch(assets["SHA256SUMS"]["browser_download_url"]).decode()
    expected = None
    for line in checksum_file.splitlines():
        chunks = line.split()
        if len(chunks) == 2 and chunks[1].lstrip("*") == ASSET:
            expected = chunks[0]
            break
    if not expected or len(expected) != 64:
        raise RuntimeError("Release SHA256SUMS is missing a valid archive checksum")
    ROOT.mkdir(parents=True, exist_ok=True)
    if not (dest / "bin/qemu-system-x86_64").is_file() or not (dest / "bin/reims-ctl").is_file():
        with tempfile.TemporaryDirectory(prefix=".download-", dir=ROOT) as work:
            work = Path(work)
            archive = work / ASSET
            archive.write_bytes(fetch(assets[ASSET]["browser_download_url"]))
            if hashlib.sha256(archive.read_bytes()).hexdigest().lower() != expected.lower():
                raise RuntimeError("SHA256 mismatch: refusing to install")
            unpack = work / "bundle"
            unpack.mkdir()
            with tarfile.open(archive, "r:gz") as tf:
                for member in tf.getmembers():
                    target = (unpack / member.name).resolve()
                    if not target.is_relative_to(unpack.resolve()):
                        raise RuntimeError("Archive path escapes installation directory")
                    if member.issym() or member.islnk() or member.isdev() or member.isfifo():
                        raise RuntimeError("Unexpected link or special file in archive")
                tf.extractall(unpack, filter="data")
            for path in ("bin/qemu-system-x86_64", "bin/qemu-img", "bin/reims-ctl"):
                if not (unpack / path).is_file():
                    raise RuntimeError(f"Missing release binary: {path}")
            if not any(unpack.rglob("ReimsVgpu.rom")):
                raise RuntimeError("Missing ReimsVgpu.rom")
            if dest.exists():
                raise RuntimeError(f"Incomplete prior install at {dest}; remove it manually")
            shutil.move(str(unpack), str(dest))
    link = ROOT / "current"
    tmp_link = ROOT / ".current-next"
    if tmp_link.is_symlink():
        tmp_link.unlink()
    if link.exists() and not link.is_symlink():
        raise RuntimeError("qemu/current exists and is not a symlink")
    tmp_link.symlink_to(dest.name)
    os.replace(tmp_link, link)
    state = read_state()
    state[channel] = tag
    state["current_channel"] = channel
    save_state(state)
    return dest

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--channel", choices=("stable", "testing"), help="Skip the channel prompt")
    parser.add_argument("--no-check", action="store_true", help="Use locally installed channel")
    parser.add_argument("--install-only", action="store_true", help="Do not launch reims-ctl")
    parser.add_argument("ctl_args", nargs=argparse.REMAINDER, help="Arguments for reims-ctl after --")
    args = parser.parse_args()
    if sys.platform != "linux" or os.uname().machine != "x86_64":
        parser.error("Prebuilt releases currently support Linux x86_64 only")
    state = read_state()
    if args.channel:
        channel = args.channel
    elif sys.stdin.isatty():
        choice = input("Choose release channel: [s]table / [t]esting "
                       f"(default {state.get('current_channel','stable')}): ").strip().lower()
        channel = {"s": "stable", "stable": "stable", "t": "testing", "testing": "testing"}.get(
            choice, state.get("current_channel", "stable"))
    else:
        channel = state.get("current_channel", "stable")
    installed = state.get(channel)
    target = ROOT / installed if installed else None
    if not args.no_check:
        try:
            release, assets = fetch_releases(channel)
            latest = safe_tag(release["tag_name"])
            if latest != installed or not target or not (target / "bin/reims-ctl").exists():
                print(f"{channel}: available {latest}; installed {installed or 'none'}")
                answer = "y" if not sys.stdin.isatty() else input("Download and activate? [Y/n] ").strip().lower()
                if answer in ("", "y", "yes"):
                    target = install(release, assets, channel)
                elif not target:
                    raise RuntimeError("No local installation available")
            else:
                target = install(release, assets, channel)
        except (OSError, ValueError, urllib.error.URLError, RuntimeError) as err:
            print(f"Update check unavailable: {err}", file=sys.stderr)
            if not target or not (target / "bin/reims-ctl").is_file():
                return 1
    if not target or not (target / "bin/reims-ctl").is_file():
        print("No cached release for this channel. Run without --no-check first.", file=sys.stderr)
        return 1
    # Activate requested cached channel even when offline.
    if args.no_check or (ROOT / "current").resolve() != target.resolve():
        link = ROOT / "current"
        tmp = ROOT / ".current-next"
        if tmp.is_symlink():
            tmp.unlink()
        tmp.symlink_to(target.name)
        os.replace(tmp, link)
        state["current_channel"] = channel
        save_state(state)
    print(f"Using {channel}: {target.name}")
    if args.install_only:
        return 0
    ctl = target / "bin/reims-ctl"
    forwarded = args.ctl_args[1:] if args.ctl_args[:1] == ["--"] else args.ctl_args
    return subprocess.call([str(ctl), *forwarded])

if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
    except Exception as exc:
        print(f"reims-release: {exc}", file=sys.stderr)
        sys.exit(1)
