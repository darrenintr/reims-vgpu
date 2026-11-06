"""Live GitHub catalog, explicit invalid-version marks, and safe automatic pruning."""
import importlib.util
import io
import json
import tempfile
from pathlib import Path
from unittest import TestCase, mock

SRC = Path(__file__).resolve().parents[1] / "scripts" / "reims-release.py"
spec = importlib.util.spec_from_file_location("reims_live_tested", SRC)
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class LiveCatalogTests(TestCase):
    def test_paginates_full_rest_catalog(self):
        def response(url):
            self.assertIn("per_page=100", url)
            page = int(url.split("page=")[-1])
            return json.dumps([{"tag_name": f"test-{page}-{i}"} for i in
                               range(100 if page == 1 else 3)]).encode()

        with mock.patch.object(release.shutil, "which", return_value=None), \
             mock.patch.object(release, "fetch", side_effect=response) as fetch:
            rows = release.release_index()
        self.assertEqual(len(rows), 103)
        self.assertEqual(fetch.call_count, 2)

    def test_catalog_includes_releases_without_binaries(self):
        with mock.patch.object(release, "release_index", return_value=[
            {"tag_name": "v1.0.0", "published_at": "2026-11-06T00:00:00Z",
             "assets": []},
            {"tag_name": "testing-5", "published_at": "2026-11-07T00:00:00Z",
             "assets": [{"name": release.ASSET}, {"name": "SHA256SUMS"}]},
        ]):
            rows = release.catalog_releases()
        self.assertEqual([x[0]["tag_name"] for x in rows], ["testing-5", "v1.0.0"])
        self.assertFalse(release.record(rows[1][0], rows[1][1], {})["downloadable"])

    def test_manifest_rejects_traversal_and_nonexplicit_tags(self):
        self.assertEqual(release.parse_invalid_manifest({
            "schema_version": 1, "invalid_tags": ["testing-12-aabbcc"]
        }), {"testing-12-aabbcc"})
        for value in (["../vm"], ["bad/path"], [1], "not-list"):
            with self.assertRaises((RuntimeError, ValueError)):
                release.parse_invalid_manifest({
                    "schema_version": 1, "invalid_tags": value
                })

    def test_local_mark_is_explicit_and_persistent(self):
        with tempfile.TemporaryDirectory() as temp, \
             mock.patch.object(release, "LOCAL_INVALID", Path(temp) / "local.json"):
            release.set_local_invalid("testing-1", True)
            self.assertEqual(release.read_invalid_file(release.LOCAL_INVALID),
                             {"testing-1"})
            release.set_local_invalid("testing-1", False)
            self.assertEqual(release.read_invalid_file(release.LOCAL_INVALID), set())

    def test_cleanup_removes_only_marked_nonselected_complete_bundle(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "qemu"
            root.mkdir()
            for tag in ("v0.1.3", "testing-bad", "testing-good"):
                folder = root / tag / "bin"
                folder.mkdir(parents=True)
                for name in ("reims-ctl", "qemu-system-x86_64", "qemu-img"):
                    (folder / name).write_bytes(b"test")
            (root / "current").symlink_to("v0.1.3")
            (root / "vm.qcow2").write_bytes(b"keep")
            state = Path(temp) / "state.json"
            state.write_text("{}")
            with mock.patch.object(release, "ROOT", root), \
                 mock.patch.object(release, "STATE", state), \
                 mock.patch.object(release, "invalid_tags", return_value={"testing-bad"}), \
                 mock.patch.object(release, "qemu_running", return_value=False):
                report = release.prune_invalid_installs(offline=True)
            self.assertEqual(report["removed"], ["testing-bad"])
            self.assertFalse((root / "testing-bad").exists())
            self.assertTrue((root / "v0.1.3").exists())
            self.assertTrue((root / "testing-good").exists())
            self.assertEqual((root / "vm.qcow2").read_bytes(), b"keep")

    def test_cleanup_skips_when_vm_running(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / "testing-bad" / "bin").mkdir(parents=True)
            for name in ("reims-ctl", "qemu-system-x86_64", "qemu-img"):
                (root / "testing-bad" / "bin" / name).write_bytes(b"test")
            with mock.patch.object(release, "ROOT", root), \
                 mock.patch.object(release, "invalid_tags", return_value={"testing-bad"}), \
                 mock.patch.object(release, "qemu_running", return_value=True):
                result = release.prune_invalid_installs(offline=True)
            self.assertEqual(result["removed"], [])
            self.assertTrue((root / "testing-bad").exists())

    def test_cleanup_never_follows_symlink_or_deletes_vm_data(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp) / "qemu"
            root.mkdir()
            data = Path(temp) / "important"
            data.mkdir()
            (data / "bin").mkdir()
            for name in ("reims-ctl", "qemu-system-x86_64", "qemu-img"):
                (data / "bin" / name).write_bytes(b"test")
            (root / "testing-bad").symlink_to(data, target_is_directory=True)
            with mock.patch.object(release, "ROOT", root), \
                 mock.patch.object(release, "invalid_tags", return_value={"testing-bad"}), \
                 mock.patch.object(release, "qemu_running", return_value=False):
                result = release.prune_invalid_installs(offline=True)
            self.assertFalse(result["removed"])
            self.assertTrue(data.exists())

    def test_selected_release_not_deleted(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            folder = root / "testing-bad" / "bin"
            folder.mkdir(parents=True)
            for name in ("reims-ctl", "qemu-system-x86_64", "qemu-img"):
                (folder / name).write_bytes(b"test")
            (root / "current").symlink_to("testing-bad")
            with mock.patch.object(release, "ROOT", root), \
                 mock.patch.object(release, "invalid_tags", return_value={"testing-bad"}), \
                 mock.patch.object(release, "qemu_running", return_value=False):
                result = release.prune_invalid_installs(offline=True)
            self.assertEqual(result["removed"], [])
            self.assertTrue((root / "testing-bad").exists())
