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


def channel_for(release):
    tag = release.get("tag_name", "")
    if tag.startswith("v") and not release.get("prerelease"):
        return "stable"
    if tag.startswith("testing-"):
        return "testing"
    return "experimental"


def valid_install(tag):
    if not tag:
        return False
    try:
        tag = safe_tag(tag)
    except RuntimeError:
        return False
    folder = ROOT / tag
    return all((folder / file).is_file() for file in (
        "bin/reims-ctl", "bin/qemu-system-x86_64", "bin/qemu-img"
    ))


def selectable_releases():
    """All real GitHub Releases that contain a verified Linux bundle."""
    results = []
    for release in release_index():
        if release.get("draft"):
            continue
        tag = release.get("tag_name") or ""
        try:
            safe_tag(tag)
        except RuntimeError:
            continue
        assets = {asset["name"]: asset for asset in release.get("assets", [])}
        if ASSET in assets and "SHA256SUMS" in assets:
            results.append((release, assets))
    results.sort(key=lambda pair: pair[0].get("published_at") or "", reverse=True)
    return results


def record(release, assets, state):
    tag = release["tag_name"]
    return {
        "tag": tag,
        "name": release.get("name") or tag,
        "channel": channel_for(release),
        "prerelease": bool(release.get("prerelease")),
        "published_at": release.get("published_at"),
        "size_bytes": assets[ASSET].get("size", 0),
        "installed": valid_install(tag),
        "current": (ROOT / "current").resolve() == (ROOT / tag).resolve(),
        "description": (release.get("body") or "").splitlines()[0][:180] if release.get("body") else "",
    }


def cached_records():
    rows = []
    if not ROOT.is_dir():
        return rows
    for folder in ROOT.iterdir():
        if not folder.is_dir() or folder.is_symlink() or not valid_install(folder.name):
            continue
        tag = folder.name
        rows.append({
            "tag": tag, "name": tag, "channel": (
                "stable" if tag.startswith("v") else
                "testing" if tag.startswith("testing-") else "experimental"
            ),
            "prerelease": not tag.startswith("v"),
            "published_at": "", "size_bytes": 0, "installed": True,
            "current": (ROOT / "current").resolve() == folder.resolve(),
            "description": "Available offline (already installed)",
        })
    return sorted(rows, key=lambda item: (not item["current"], item["tag"]))


def show_releases(offline=False, as_json=False):
    if offline:
        rows = cached_records()
    else:
        try:
            rows = [record(r, assets, read_state()) for r, assets in selectable_releases()]
        except (OSError, RuntimeError, ValueError, urllib.error.URLError) as exc:
            print(f"GitHub release listing unavailable: {exc}; showing installed builds", file=sys.stderr)
            rows = cached_records()
    if as_json:
        print(json.dumps(rows, ensure_ascii=False))
    else:
        for item in rows:
            installed = " [downloaded]" if item["installed"] else ""
            active = " [active]" if item["current"] else ""
            print(f"{item['tag']:47} {item['channel']:12} {item['name']}{installed}{active}")
    return 0 if rows else 1


def fetch_releases(channel=None, tag=None):
    matches = selectable_releases()
    for release, assets in matches:
        if tag is not None:
            if release["tag_name"] == tag:
                return release, assets
        elif channel == channel_for(release):
            return release, assets
    raise RuntimeError(f"No downloadable GitHub Release for {tag or channel}")

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
    ROOT.mkdir(parents=True, exist_ok=True)
    if not valid_install(tag):
        checksum_file = fetch(assets["SHA256SUMS"]["browser_download_url"]).decode()
        expected = None
        for line in checksum_file.splitlines():
            chunks = line.split()
            if len(chunks) == 2 and chunks[1].lstrip("*") == ASSET:
                expected = chunks[0]
                break
        if not expected or len(expected) != 64 or not all(c in "0123456789abcdefABCDEF" for c in expected):
            raise RuntimeError("Release SHA256SUMS is missing a valid archive checksum")
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
                    if member.issym() or member.islnk():
                        if not member.linkname or os.path.isabs(member.linkname):
                            raise RuntimeError("Archive contains an unsafe symlink")
                        source = target.parent if member.issym() else unpack
                        linked = (source / member.linkname).resolve()
                        if not linked.is_relative_to(unpack.resolve()):
                            raise RuntimeError("Archive contains a link outside the installation directory")
                    elif not (member.isfile() or member.isdir()):
                        raise RuntimeError("Archive contains a special file")
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
    if channel in ("stable", "testing"):
        state[channel] = tag
        state["current_channel"] = channel
    state["current_tag"] = tag
    save_state(state)
    return dest

def activate_cached(tag, channel):
    if not valid_install(tag):
        raise RuntimeError(f"Build {tag} is not installed (use online mode first)")
    ROOT.mkdir(parents=True, exist_ok=True)
    link = ROOT / "current"
    pending = ROOT / ".current-next"
    if link.exists() and not link.is_symlink():
        raise RuntimeError("qemu/current is not a symlink; refusing to overwrite")
    if pending.is_symlink():
        pending.unlink()
    pending.symlink_to(tag)
    os.replace(pending, link)
    state = read_state()
    state["current_tag"] = tag
    if channel in ("stable", "testing"):
        state[channel] = tag
        state["current_channel"] = channel
    save_state(state)
    return ROOT / tag


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    selected = parser.add_mutually_exclusive_group()
    selected.add_argument("--tag", help="Exact GitHub Release tag (including experimental builds)")
    selected.add_argument("--channel", choices=("stable", "testing"),
                          help="Compatibility option: newest release in this channel")
    parser.add_argument("--list", action="store_true", help="List ALL releases with a Linux bundle")
    parser.add_argument("--json", action="store_true", help="Machine-readable --list output")
    parser.add_argument("--offline", action="store_true", help="Use only installed builds with --list")
    parser.add_argument("--no-check", action="store_true", help="Use a downloaded build without a network query")
    parser.add_argument("--install-only", action="store_true", help="Do not launch reims-ctl")
    parser.add_argument("ctl_args", nargs=argparse.REMAINDER,
                        help="Arguments for reims-ctl after --")
    args = parser.parse_args()
    if sys.platform != "linux" or os.uname().machine != "x86_64":
        parser.error("Prebuilt releases currently support Linux x86_64 only")
    if args.list:
        return show_releases(offline=args.offline or args.no_check, as_json=args.json)

    state = read_state()
    channel = args.channel
    tag = args.tag
    if tag:
        tag = safe_tag(tag)
    elif not channel:
        if sys.stdin.isatty():
            choice = input("Release: [s]table / [t]esting (or supply --tag): ").strip().lower()
            channel = "testing" if choice.startswith("t") else "stable"
        else:
            channel = state.get("current_channel", "stable")
    if not tag:
        tag = state.get(channel) if args.no_check else None
    target = ROOT / tag if tag else None
    if args.no_check:
        if not tag:
            print("No cached version for this channel", file=sys.stderr)
            return 1
        target = activate_cached(tag, channel)
    else:
        try:
            release, assets = fetch_releases(channel=channel, tag=tag)
            wanted = safe_tag(release["tag_name"])
            if valid_install(wanted):
                target = activate_cached(wanted, channel)
            else:
                print(f"Downloading {wanted} from GitHub Releases")
                if sys.stdin.isatty() and not args.tag:
                    answer = input("Download and activate? [Y/n] ").strip().lower()
                    if answer not in ("", "y", "yes"):
                        return 1
                target = install(release, assets, channel or channel_for(release))
        except (OSError, ValueError, urllib.error.URLError, RuntimeError) as err:
            print(f"GitHub unavailable: {err}", file=sys.stderr)
            cached = tag or state.get(channel or "", "")
            if valid_install(cached):
                target = activate_cached(cached, channel)
            else:
                return 1
    if not target or not valid_install(target.name):
        print("No valid local build; select a published GitHub release", file=sys.stderr)
        return 1
    print(f"Using {target.name}")
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
