"""Preserve upstream assertions while adapting test-only scratch helpers.

The old fork has a private, differently shaped Scratch helper. New upstream
shader tests expect a labeled RAII scratch directory. Add a test-local helper
rather than changing the legacy helper or importing unrelated production code.
"""
from __future__ import annotations

import json

SCRATCH = r'''
// Test-only compatibility with the retained translator's helper API.
struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new(label: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(0);
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "{}-{}-{}", label, std::process::id(), id
        ));
        std::fs::create_dir(&path).expect("unique test scratch directory");
        Self(path)
    }
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
'''


def adapt_test_helpers(engine) -> None:
    manifest_path = engine.ROOT / engine.MARKER
    manifest = json.loads(manifest_path.read_text())
    paths = ["vendor/metal2vulkan/crates/metal2vulkan/src/tests/engine_debug_info.rs"]
    if any(p["commit"] == engine.CORE_IMAGE for p in manifest["applied"]):
        paths.append("vendor/metal2vulkan/crates/metal2vulkan/src/tests/engine_aggregate_access.rs")
    changes = []
    for relative in paths:
        path = engine.ROOT / relative
        text = path.read_text()
        old = "AssembleError, Scratch"
        if old not in text:
            if "struct Scratch(" not in text:
                raise RuntimeError(f"Unexpected test helper layout: {relative}")
            continue
        if text.count(old) != 1 or "struct Scratch(" in text:
            raise RuntimeError(f"Ambiguous test helper layout: {relative}")
        changes.append((relative, path, text.replace(old, "AssembleError", 1) + SCRATCH))
    if not changes:
        return
    for relative, path, text in changes:
        path.write_text(text)
    manifest["test_adaptations"] = [{
        "file": relative,
        "reason": "Use test-local labeled RAII scratch helper; upstream assertions unchanged."
    } for relative in paths]
    manifest["metal2vulkan_tree_scope"] = (
        "Pinned port tree before documented test-only adaptations; final exact sources are in the candidate Git commit."
    )
    manifest_path.write_text(json.dumps(manifest, indent=2) + "\n")
    engine.git(engine.ROOT, "add", *paths, str(engine.MARKER))
    print(f"Adapted {len(changes)} test-only scratch helpers; no assertions removed.", flush=True)
