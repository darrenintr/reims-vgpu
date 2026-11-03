#!/usr/bin/env python3
"""Assemble an isolated candidate; never install it or touch Stable refs.

All upstream inputs are immutable commits. Changed lines must match exactly.
Outer unchanged context may be shortened only to an unambiguous anchored match.
Optional translator ports are transactional and reported if deferred.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile

BASE = "c8838347f0e5500c0950d2b15695a78a2ff165f8"
QEMU = "b9d3bd5a9dd427ac42f6a2249c26a0b342f8f16e"
M2V_BASE = "6d6f5d92326361562ac5d482eb017f41d80c82d9"
DECODER = "43b7caa200b8ed49ac56c8528b89a65cc346e583"
FIFO = "422eab5b82ebbb085cbff3dc434384406c0fffc3"
DEBUG = "510cb01c4cad473ade0e661b8c08a69a6e9e3eee"
UNIVERSAL = "ec6b322bb5e794d42ee62498f60c9c35b78aa9c8"
CORE_IMAGE = "bf3ce3813c029c96d02594f8568f2ee8343c0ed8"
MARKER = Path("docs/upstream-stack-20261103.json")
ROOT = Path(__file__).resolve().parents[1]
AUDIT: list[dict[str, object]] = []


def git(repo: Path, *args: str) -> str:
    p = subprocess.run(["git", "-C", str(repo), *args], text=True,
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if p.returncode:
        raise RuntimeError(f"git {' '.join(args)}: {p.stderr[-4000:]}")
    return p.stdout


def occurrences(lines: list[str], needle: list[str]) -> list[int]:
    if not needle:
        return []
    return [i for i in range(len(lines) - len(needle) + 1)
            if lines[i:i + len(needle)] == needle]


def match_hunk(lines: list[str], hunk: list[str]) -> tuple[list[str], tuple[int, int]]:
    lead = 0
    for line in hunk:
        if not line.startswith(" "):
            break
        lead += 1
    tail = 0
    for line in reversed(hunk):
        if not line.startswith(" "):
            break
        tail += 1
    # Prefer the greatest amount of original context. All equally strong
    # anchors must produce the same result. An insertion never matches empty
    # text, and deletion/replacement lines are never shortened or ignored.
    for total in range(lead + tail + 1):
        results: dict[tuple[str, ...], tuple[int, int]] = {}
        for left in range(lead + 1):
            right = total - left
            if not 0 <= right <= tail:
                continue
            slim = hunk[left:len(hunk) - right if right else len(hunk)]
            old = [s[1:] for s in slim if s[0] in " -"]
            new = [s[1:] for s in slim if s[0] in " +"]
            found = occurrences(lines, old)
            if len(found) != 1:
                continue
            start = found[0]
            result = tuple(lines[:start] + new + lines[start + len(old):])
            results[result] = (left, right)
        if len(results) > 1:
            raise RuntimeError("Equally strong context anchors disagree")
        if results:
            result, trimmed = next(iter(results.items()))
            return list(result), trimmed
    changed = [s for s in hunk if not s.startswith(" ")]
    raise RuntimeError(f"No unique exact source match; changes={changed[:5]!r}")


def apply_patch(source: str, patch: str, filename: str) -> str:
    hunks: list[list[str]] = []
    for line in patch.splitlines():
        if line.startswith("@@ "):
            hunks.append([])
        elif hunks and line[:1] in (" ", "+", "-"):
            hunks[-1].append(line)
    if not hunks:
        raise RuntimeError(f"No text hunks: {filename}")
    lines = source.splitlines()
    for number, hunk in enumerate(hunks, 1):
        try:
            lines, trimmed = match_hunk(lines, hunk)
        except RuntimeError as error:
            raise RuntimeError(f"Conflict in {filename}, hunk {number}: {error}") from error
        if trimmed != (0, 0):
            AUDIT.append({"file": filename, "hunk": number,
                          "resolution": "unique exact anchored match",
                          "outer_context_removed": list(trimmed)})
    return "\n".join(lines) + "\n"


def port(target: Path, upstream: Path, commit: str, *, translator: bool = False) -> list[str]:
    files = git(upstream, "diff", "--name-status", commit + "^", commit).splitlines()
    writes: list[tuple[Path, str]] = []
    for entry in files:
        status, name = entry.split("\t", 1)
        if not name.startswith("crates/") or not name.endswith(".rs"):
            continue
        path = target / name
        if status == "A":
            if path.exists():
                raise RuntimeError(f"New upstream file already exists: {name}")
            writes.append((path, git(upstream, "show", f"{commit}:{name}")))
            continue
        if status != "M":
            raise RuntimeError(f"Unsupported change {status}: {name}")
        patch = git(upstream, "diff", "--no-ext-diff", "--unified=3", commit + "^", commit, "--", name)
        if translator and commit == DEBUG:
            # Debug metadata needs Md, not the later unrelated TypeLayouts API.
            patch = patch.replace("MdId, Module, Type, TypeId, TypeLayouts",
                                  "MdId, Module, Type, TypeId")
            if name == "crates/metal2vulkan/src/cache/mod.rs":
                old = path.read_text()
                new, count = re.subn(r"pub const SEMANTICS_VERSION: u32 = \d+;",
                                    "pub const SEMANTICS_VERSION: u32 = 2026110301;", old)
                if count != 1:
                    raise RuntimeError("Cannot assign fork-specific shader cache version")
                writes.append((path, new))
                continue
        writes.append((path, apply_patch(path.read_text(), patch, name)))
    if not writes:
        raise RuntimeError(f"No source changes selected for {commit}")
    for path, content in writes:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
    return [str(path.relative_to(target)) for path, _ in writes]


def fetch(repo: Path, remote: str, *refs: str) -> None:
    # Each port needs its parent tree, not the complete upstream history.
    git(repo, "fetch", "--no-tags", "--depth=2", remote, *refs)


def require(path: str, *strings: str) -> None:
    text = (ROOT / path).read_text()
    for s in strings:
        if s not in text:
            raise RuntimeError(f"Missing invariant in {path}: {s}")


def self_test() -> None:
    assert match_hunk(["a", "trace", "old", "b"], [" a", "-old", "+new", " b"])[0] == ["a", "trace", "new", "b"]
    assert match_hunk(["mod old_a;", "mod common;"], [" mod upstream_only;", "+mod inserted;", " mod common;"])[0] == ["mod old_a;", "mod inserted;", "mod common;"]
    for lines in (["old", "old"], ["newer"]):
        try:
            match_hunk(lines, ["-old", "+new"])
        except RuntimeError:
            continue
        raise RuntimeError("Patch matcher accepted ambiguous or mismatched input")


def main() -> None:
    self_test()
    os.chdir(ROOT)
    if MARKER.exists():
        manifest = json.loads(MARKER.read_text())
        if manifest.get("assembled") is not True or manifest.get("qemu") != QEMU:
            raise RuntimeError("Unexpected candidate manifest; refusing to rebuild")
        print("Already assembled; compiling the committed candidate unchanged.")
        return
    git(ROOT, "merge-base", "--is-ancestor", BASE, "HEAD")
    partial = set(git(ROOT, "diff", "--name-only", BASE, "HEAD", "--", "crates/reims-vgpu-core").splitlines())
    expected = {
        "crates/reims-vgpu-core/src/collections.rs",
        "crates/reims-vgpu-core/src/collections/hasher.rs",
        "crates/reims-vgpu-core/src/collections/list_order.rs",
        "crates/reims-vgpu-core/src/collections/lookup.rs",
        "crates/reims-vgpu-core/src/deserializer.rs",
    }
    if not partial.issubset(expected):
        raise RuntimeError(f"Unexpected pre-existing core edits: {partial - expected}")
    git(ROOT, "restore", "--source=" + BASE, "--staged", "--worktree", "--", "crates/reims-vgpu-core")
    fetch(ROOT, "https://github.com/steelbrain/reims-vgpu.git", DECODER, FIFO)
    applied = []
    for commit in (DECODER, FIFO):
        paths = port(ROOT, ROOT, commit)
        applied.append({"component": "reims-vgpu", "commit": commit, "files": paths})
        print(f"Applied Reims {commit}: {len(paths)} source files", flush=True)
    require("crates/reims-vgpu-core/src/rt/thread.rs", "pub fn set_before_block", "576 KiB")
    require("crates/reims-vgpu-core/src/tasks/remote.rs", "self.link.send_here(ToService::ExecIndirect3", '"exec-send"')
    require("crates/reims-vgpu-core/src/tasks/isolation/link.rs", "VecDeque", "fn send_here", "hold_if_idle")
    git(ROOT, "update-index", "--cacheinfo", f"160000,{QEMU},vendor/qemu")
    deferred = []
    with tempfile.TemporaryDirectory(prefix="reims-m2v-") as tmp:
        m2v = Path(tmp)
        git(m2v, "init", "-q")
        fetch(m2v, "https://github.com/darrenintr/metal2vulkan.git", M2V_BASE)
        git(m2v, "checkout", "--detach", M2V_BASE)
        fetch(m2v, "https://github.com/steelbrain/metal2vulkan.git", DEBUG, UNIVERSAL, CORE_IMAGE)
        for commit in (DEBUG, UNIVERSAL, CORE_IMAGE):
            try:
                paths = port(m2v, m2v, commit, translator=True)
            except RuntimeError as error:
                if commit == DEBUG:
                    raise
                deferred.append({"component": "metal2vulkan", "commit": commit, "reason": str(error)})
                print(f"DEFERRED {commit}: {error}", flush=True)
                continue
            applied.append({"component": "metal2vulkan", "commit": commit, "files": paths})
            print(f"Applied Metal2Vulkan {commit}: {len(paths)} source files", flush=True)
        git(m2v, "add", "crates")
        git(m2v, "-c", "user.name=Reims candidate builder", "-c", "user.email=41898282+github-actions[bot]@users.noreply.github.com", "commit", "-m", "Assemble pinned translator compatibility backports")
        m2v_tree = git(m2v, "rev-parse", "HEAD^{tree}").strip()
        archive = ROOT / ".m2v-source.tar"
        git(m2v, "archive", "--format=tar", "--output=" + str(archive), "HEAD")
        vendor = ROOT / "vendor/metal2vulkan"
        vendor.mkdir(parents=True, exist_ok=False)
        with tarfile.open(archive) as tar:
            tar.extractall(vendor, filter="data")
        archive.unlink()
    toml = (ROOT / "Cargo.toml").read_text()
    for name in ("metal2vulkan", "metal2vulkan-runtime"):
        pattern = rf'^{name} = \{{ git = "https://github.com/darrenintr/metal2vulkan", rev = "{M2V_BASE}" \}}$'
        toml, count = re.subn(pattern, f'{name} = {{ path = "vendor/metal2vulkan/crates/{name}" }}', toml, flags=re.M)
        if count != 1:
            raise RuntimeError(f"Unexpected {name} dependency pin")
    toml = toml.replace('[workspace]\n', '[workspace]\nexclude = ["vendor/metal2vulkan"]\n', 1)
    (ROOT / "Cargo.toml").write_text(toml)
    lock = (ROOT / "Cargo.lock").read_text()
    lock = re.sub(r'^source = "git\+https://github.com/darrenintr/metal2vulkan[^"\n]*"\n', '', lock, flags=re.M)
    (ROOT / "Cargo.lock").write_text(lock)
    manifest = {
        "assembled": True, "baseline": BASE, "qemu": QEMU,
        "qemu_changes": ["THP alignment", "cfa70f3a", "59cb5109", "a8633856"],
        "metal2vulkan_base": M2V_BASE, "metal2vulkan_tree": m2v_tree,
        "translator_cache_semantics": 2026110301, "applied": applied,
        "deferred": deferred, "context_adaptations": AUDIT,
        "compiler": {"rust": "1.98.1", "target_cpu": "x86-64-v3", "lto": "fat", "codegen_units": 1},
        "not_included": ["APV guest-driver / IOSurface migration", "video acceleration devices", "KVM dirty-page copying", "pipeline timeout behavior", "Rust / SDL upgrades"],
        "validation": "Source assembly only. CI compilation and tests are separate gates. No RX 7600 or Tahoe boot claim.",
    }
    MARKER.write_text(json.dumps(manifest, indent=2) + "\n")
    git(ROOT, "add", "crates/reims-vgpu-core", "vendor/metal2vulkan", "vendor/qemu", "Cargo.toml", "Cargo.lock", str(MARKER))
    git(ROOT, "diff", "--cached", "--check", "--", "crates/reims-vgpu-core", "Cargo.toml", "Cargo.lock")
    print(json.dumps(manifest, indent=2), flush=True)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(f"ASSEMBLY FAILED: {error}", file=sys.stderr)
        sys.exit(1)
