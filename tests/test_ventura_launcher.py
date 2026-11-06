"""Offline and GitHub release-selection regression tests for the Ventura launcher."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

REPO = Path(__file__).resolve().parents[1]
LAUNCH = REPO / "scripts" / "ventura-launch-remote"

FAKE_MANAGER = r"""#!/usr/bin/env python3
import json, os, pathlib, shutil, sys
root = pathlib.Path(os.environ["REIMS_CTL_DATA_DIR"]) / "qemu"
log = pathlib.Path(os.environ["VENTURA_TEST_MANAGER_CALLS"])
with log.open("a") as f:
    f.write(" ".join(sys.argv[1:]) + "\n")
if "--prune-invalid" in sys.argv:
    print(json.dumps({"marked": [], "removed": [], "skipped": []}))
    sys.exit(0)
if "--list" in sys.argv:
    offline = "--offline" in sys.argv
    installed = []
    for p in sorted(root.iterdir()):
        if p.name != "current" and p.is_dir():
            installed.append({
                "tag": p.name, "name": p.name, "test_label": p.name,
                "installed": True, "current": (root / "current").resolve() == p.resolve()
            })
    if not offline:
        installed.append({
            "tag": "testing-combined-1-0123456789ab",
            "name": "New combined testing", "test_label": "New combined testing",
            "installed": False, "current": False
        })
    print(json.dumps(installed))
    sys.exit(0)
if "--tag" not in sys.argv:
    sys.exit(2)
tag = sys.argv[sys.argv.index("--tag") + 1]
dest = root / tag
if not dest.is_dir():
    if "--no-check" in sys.argv:
        sys.exit(3)
    shutil.copytree(root / "v0.1.3", dest)
current = root / "current"
if current.is_symlink():
    current.unlink()
current.symlink_to(tag)
print("Using " + tag)
"""

FAKE_CTL = """#!/usr/bin/env bash
case "$1" in
  --version) echo 'reims-ctl 0.1.3' ;;
  start|stop) echo "$1:$2:$0" >> "$VENTURA_TEST_VM_CALLS" ;;
  *) exit 2 ;;
esac
"""


class VenturaLauncherTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        self.data = self.home / "data"
        self.qemu = self.data / "qemu"
        self.seed = self.qemu / "v0.1.3" / "bin"
        self.seed.mkdir(parents=True)
        ctl = self.seed / "reims-ctl"
        ctl.write_text(FAKE_CTL)
        ctl.chmod(0o755)
        for binary in ("qemu-system-x86_64", "qemu-img"):
            p = self.seed / binary
            p.write_text("#!/bin/sh\nexit 0\n")
            p.chmod(0o755)
        vm_dir = self.data / "vms" / "ventura"
        vm_dir.mkdir(parents=True)
        self.manager = self.home / "fake-manager.py"
        self.manager.write_text(FAKE_MANAGER)
        self.manager.chmod(0o755)
        self.manager_log = self.home / "manager.log"
        self.vm_log = self.home / "vm.log"
        self.env = dict(os.environ)
        self.env.update({
            "HOME": str(self.home),
            "REIMS_CTL_DATA_DIR": str(self.data),
            "XDG_STATE_HOME": str(self.home / "state"),
            "VENTURA_RELEASE_MANAGER": str(self.manager),
            "VENTURA_TEST_MANAGER_CALLS": str(self.manager_log),
            "VENTURA_TEST_VM_CALLS": str(self.vm_log),
        })

    def run_launcher(self, *args):
        return subprocess.run(["bash", str(LAUNCH), *args],
                              env=self.env, text=True, capture_output=True, timeout=15)

    def manager_calls(self):
        return self.manager_log.read_text() if self.manager_log.exists() else ""

    def vm_calls(self):
        return self.vm_log.read_text() if self.vm_log.exists() else ""

    def test_local_build_starts_ventura_without_network_lookup(self):
        result = self.run_launcher("--local", "v0.1.3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--tag v0.1.3 --no-check --install-only", self.manager_calls())
        self.assertEqual(self.vm_calls().count("start:ventura:"), 1)
        self.assertEqual((self.qemu / "current").resolve(), self.qemu / "v0.1.3")

    def test_remote_build_downloads_and_starts_exact_tag(self):
        tag = "testing-combined-1-0123456789ab"
        result = self.run_launcher("--tag", tag)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--tag " + tag + " --install-only", self.manager_calls())
        self.assertNotIn("--no-check", self.manager_calls())
        self.assertEqual((self.qemu / "current").resolve(), self.qemu / tag)
        self.assertEqual(self.vm_calls().count("start:ventura:"), 1)

    def test_local_list_is_offline_and_does_not_start_vm(self):
        result = self.run_launcher("--list-local")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("v0.1.3", result.stdout)
        self.assertIn("--list --offline --json", self.manager_calls())
        self.assertFalse(self.vm_log.exists())

    def test_remote_list_shows_downloadable_release(self):
        result = self.run_launcher("--list-remote")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("testing-combined-1-0123456789ab", result.stdout)
        self.assertFalse(self.vm_log.exists())

    def test_missing_ventura_vm_blocks_activation(self):
        (self.data / "vms" / "ventura").rmdir()
        result = self.run_launcher("--local", "v0.1.3")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("No reims-ctl VM", result.stderr)
        self.assertNotIn("--tag", self.manager_calls())

    def test_bad_tag_does_not_reach_release_manager(self):
        result = self.run_launcher("--local", "../outside")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Invalid version tag", result.stderr)
        self.assertNotIn("--tag", self.manager_calls())

    def test_running_vm_does_not_switch_binary(self):
        (self.data / "vms" / "ventura" / "qemu.pid").write_text(str(os.getpid()))
        result = self.run_launcher("--tag", "testing-combined-1-0123456789ab")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("already running", result.stderr)
        self.assertNotIn("--tag", self.manager_calls())

    def test_stop_preserves_current_build(self):
        (self.qemu / "current").symlink_to("v0.1.3")
        (self.data / "vms" / "ventura" / "qemu.pid").write_text(str(os.getpid()))
        result = self.run_launcher("--stop")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("stop:ventura:", self.vm_calls())
        self.assertEqual((self.qemu / "current").resolve(), self.qemu / "v0.1.3")


    def test_remote_selection_of_cached_version_never_redownloads(self):
        result = self.run_launcher("--tag", "v0.1.3")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--tag v0.1.3 --no-check --install-only", self.manager_calls())
        self.assertIn("start:ventura:", self.vm_calls())

    def test_automatic_cleanup_is_requested_before_catalog(self):
        result = self.run_launcher("--list-remote")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.manager_calls().startswith("--prune-invalid --json\\n"))
        self.assertIn("--list --json", self.manager_calls())

    def test_single_live_menu_selects_cached_version(self):
        gui = self.home / "bin"
        gui.mkdir()
        kdialog = gui / "kdialog"
        kdialog.write_text("#!/bin/sh\\nprintf 'v0.1.3\\n'\\n")
        kdialog.chmod(0o755)
        self.env["PATH"] = str(gui) + os.pathsep + self.env.get("PATH", "")
        result = self.run_launcher()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--list --json", self.manager_calls())
        self.assertIn("--tag v0.1.3 --no-check --install-only", self.manager_calls())
        self.assertEqual(self.vm_calls().count("start:ventura:"), 1)



if __name__ == "__main__":
    unittest.main()
