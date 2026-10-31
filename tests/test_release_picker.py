"""Regression tests for the GitHub Release catalog used by the Tahoe menu."""
import contextlib
import importlib.util
import io
import json
import tempfile
from pathlib import Path
import unittest
from unittest import mock

SOURCE = Path(__file__).resolve().parents[1] / "scripts" / "reims-release.py"
SPEC = importlib.util.spec_from_file_location("reims_release_tested", SOURCE)
release = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(release)


def row(tag, published, *, current=False):
    if tag.startswith("v"):
        channel = "stable"
    elif tag.startswith("testing-"):
        channel = "testing"
    else:
        channel = "experimental"
    return release.describe_release({
        "tag": tag,
        "name": tag,
        "channel": channel,
        "published_at": published,
        "installed": current,
        "current": current,
        "prerelease": channel != "stable",
        "description": "",
        "size_bytes": 1000,
    })


class ReleasePickerTests(unittest.TestCase):
    def sample_rows(self, old_selected=False):
        return [
            row("testing-12-f36b720da5d1", "2026-10-31T03:00:00Z"),
            row("v0.1.0", "2026-10-31T04:30:00Z"),
            row("testing-14-e41957269871", "2026-10-31T04:40:00Z"),
            row("exp-native-v3-1-2a63d397e686", "2026-10-31T05:00:00Z",
                current=old_selected),
            row("exp-native-v3-3-e4a42df79f14", "2026-10-31T05:20:00Z"),
            row("exp-fat-lto-1-2a63d397e686", "2026-10-31T05:01:00Z"),
            row("exp-fat-lto-3-e4a42df79f14", "2026-10-31T05:25:00Z"),
            row("exp-profiler-1-2a63d397e686", "2026-10-31T05:02:00Z"),
            row("exp-profiler-3-e4a42df79f14", "2026-10-31T05:30:00Z"),
        ]

    def test_curated_keeps_latest_per_purpose(self):
        result = release.curated_releases(self.sample_rows())
        self.assertEqual([x["tag"] for x in result], [
            "v0.1.0",
            "testing-14-e41957269871",
            "exp-fat-lto-3-e4a42df79f14",
            "exp-native-v3-3-e4a42df79f14",
        ])
        self.assertEqual(release.diagnostic_releases(self.sample_rows())[0]["tag"],
                         "exp-profiler-3-e4a42df79f14")

    def test_current_old_build_remains_reachable(self):
        result = release.curated_releases(self.sample_rows(old_selected=True))
        self.assertNotIn("exp-native-v3-1-2a63d397e686",
                         [x["tag"] for x in result])
        self.assertEqual(result[0]["tag"], "v0.1.0")
        self.assertTrue(any(x["current"] for x in self.sample_rows(old_selected=True)))

    def test_full_history_still_available(self):
        rows = self.sample_rows()
        with mock.patch.object(release, "cached_records", return_value=rows):
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                status = release.show_releases(offline=True, as_json=True, curated=False)
            all_rows = json.loads(out.getvalue())
            self.assertEqual(status, 0)
            self.assertEqual(len(all_rows), len(rows))
            old = next(x for x in all_rows if x["tag"].startswith("testing-12-"))
            self.assertTrue(old["historical"])
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                release.show_releases(offline=True, as_json=True, curated=True)
            self.assertEqual(len(json.loads(out.getvalue())), 4)

    def test_new_qemu_thp_candidate_is_high_priority(self):
        rows = self.sample_rows()
        rows.append(row("exp-qemu-thp-7-bf4a3a511ecb", "2026-10-31T06:00:00Z"))
        tags = [x["tag"] for x in release.curated_releases(rows)]
        self.assertEqual(tags[2], "exp-qemu-thp-7-bf4a3a511ecb")
        self.assertIn("QEMU", release.describe_release(rows[-1])["test_label"])

    def test_combined_native_v3_fat_lto_candidate_is_distinct(self):
        rows = self.sample_rows()
        rows.extend([
            row("exp-qemu-thp-7-bf4a3a511ecb", "2026-10-31T06:00:00Z"),
            row("exp-native-v3-fat-lto-8-abcdef123456", "2026-10-31T08:25:00Z"),
        ])
        selected = release.curated_releases(rows)
        tags = [r["tag"] for r in selected]
        self.assertEqual(tags[2], "exp-qemu-thp-7-bf4a3a511ecb")
        self.assertEqual(tags[3], "exp-native-v3-fat-lto-8-abcdef123456")
        self.assertIn("组合优化", selected[3]["test_label"])
        self.assertEqual(release.experiment_variant(tags[3]), "native-v3-fat-lto")
        self.assertIn("Fat-LTO", selected[3]["test_label"])

    def test_known_and_unknown_experiments(self):
        self.assertEqual(release.experiment_variant("exp-native-v3-3-e4a42df79f14"),
                         "native-v3")
        self.assertEqual(release.experiment_variant("exp-qemu-thp-7-bf4a3a511ecb"),
                         "qemu-thp")
        self.assertIsNone(release.experiment_variant("exp-fake-no-sha"))
        unknown = release.describe_release(row("exp-abc-2-abcdef123456", ""))
        self.assertIn("未分类", unknown["test_label"])

    def test_native_v3_stable_and_portable_rollback_labels(self):
        stable = row("v0.1.1", "2026-10-31T08:06:26Z")
        rollback = row("v0.1.0", "2026-10-31T04:34:40Z")
        self.assertIn("需 x86-64-v3", stable["test_label"])
        self.assertIn("Native-v3", stable["test_label"])
        self.assertIn("回滚", rollback["test_label"])
        selected = release.curated_releases([rollback, stable])
        self.assertEqual(selected[0]["tag"], "v0.1.1")

    def test_stable_v0_1_2_archives_promoted_experiments(self):
        rows = self.sample_rows()
        rows.extend([
            row("v0.1.1", "2026-10-31T08:06:26Z"),
            row("v0.1.2", "2026-10-31T09:00:22Z"),
            row("exp-native-v3-fat-lto-1-c8838347f0e5",
                "2026-10-31T08:49:51Z"),
        ])
        selected = release.curated_releases(rows)
        tags = [r["tag"] for r in selected]
        self.assertEqual(tags, ["v0.1.2"])
        latest = selected[0]
        self.assertIn("Native-v3 + Fat-LTO", latest["test_label"])
        self.assertIn("x86-64-v3", latest["test_label"])
        historical = release.describe_release(
            row("exp-native-v3-fat-lto-1-c8838347f0e5", ""))
        self.assertIn("已晋升 Stable v0.1.2", historical["test_label"])

        # A newly built stacked experiment is visible, rather than hidden
        # because some older stacked experiment became Stable.
        rows.append(row("exp-native-v3-fat-lto-2-abcdef123456",
                        "2026-11-01T08:00:00Z"))
        tags = [r["tag"] for r in release.curated_releases(rows)]
        self.assertIn("exp-native-v3-fat-lto-2-abcdef123456", tags)

    def test_selected_promoted_experiment_is_still_reachable(self):
        rows = self.sample_rows()
        rows.extend([
            row("v0.1.2", "2026-10-31T09:00:22Z"),
            row("exp-native-v3-fat-lto-1-c8838347f0e5",
                "2026-10-31T08:49:51Z", current=True),
        ])
        selected = release.curated_releases(rows)
        self.assertNotIn("exp-native-v3-fat-lto-1-c8838347f0e5",
                         [r["tag"] for r in selected])
        self.assertEqual(selected[0]["tag"], "v0.1.2")
        self.assertTrue(rows[-1]["current"])  # remains in full history

    def test_cleanup_removes_only_obsolete_local_releases(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "qemu"
            root.mkdir()
            tags = [
                "v0.1.2", "v0.1.1", "v0.1.0",
                "testing-14-e41957269871",
                "exp-native-v3-fat-lto-1-c8838347f0e5",
                "exp-profiler-3-e4a42df79f14",
                "exp-profiler-1-2a63d397e686",
                "exp-qemu-thp-1-abcdef123456",
            ]
            dates = ["2026-10-31T09:00:22Z", "2026-10-31T08:00:00Z",
                     "2026-10-31T04:00:00Z", "2026-10-31T04:40:00Z",
                     "2026-10-31T08:49:51Z", "2026-10-31T05:35:00Z",
                     "2026-10-31T05:01:00Z", "2026-10-31T10:00:00Z"]
            releases = []
            for tag, date in zip(tags, dates):
                binary = root / tag / "bin"
                binary.mkdir(parents=True)
                for file in ("reims-ctl", "qemu-system-x86_64", "qemu-img"):
                    (binary / file).write_bytes(b"fake-test-binary")
                releases.append(({
                    "tag_name": tag, "name": tag, "published_at": date,
                    "prerelease": not tag.startswith("v"),
                }, {release.ASSET: {"size": 800}, "SHA256SUMS": {}}))
            (root / "current").symlink_to("v0.1.2")
            (root / "macos-disk.qcow2").write_bytes(b"never touch vm disk")
            state_file = Path(tmp) / "release-manager.json"
            state_file.write_text(json.dumps({
                "stable": "v0.1.2", "testing": "testing-14-e41957269871",
                "current_tag": "v0.1.2", "current_channel": "stable"
            }))
            with (mock.patch.object(release, "ROOT", root),
                  mock.patch.object(release, "STATE", state_file),
                  mock.patch.object(release, "selectable_releases",
                                    return_value=releases),
                  mock.patch.object(release, "qemu_running", return_value=False)):
                plan = release.cleanup_plan()
                names = {r["tag"] for r in plan["candidates"]}
                self.assertEqual(names, {
                    "testing-14-e41957269871",
                    "exp-native-v3-fat-lto-1-c8838347f0e5",
                    "exp-profiler-1-2a63d397e686",
                })
                self.assertGreater(plan["total_bytes"], 0)
                with self.assertRaisesRegex(RuntimeError, "changed since preview"):
                    release.apply_cleanup("0" * 64)
                result = release.apply_cleanup(plan["plan_id"])
                self.assertEqual(set(result["removed"]), names)
                for tag in names:
                    self.assertFalse((root / tag).exists())
                for tag in set(tags) - names:
                    self.assertTrue((root / tag / "bin/reims-ctl").exists())
                self.assertTrue((root / "macos-disk.qcow2").exists())
                self.assertEqual((root / "current").resolve(),
                                 (root / "v0.1.2").resolve())
                self.assertNotIn("testing", json.loads(state_file.read_text()))

    def test_cleanup_never_deletes_selected_or_running_version(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "qemu"
            root.mkdir()
            current = "exp-native-v3-fat-lto-1-c8838347f0e5"
            baseline = "v0.1.2"
            dates = ["2026-10-31T08:49:51Z", "2026-10-31T09:00:22Z"]
            rows = []
            for tag, date in zip([current, baseline], dates):
                bin_dir = root / tag / "bin"
                bin_dir.mkdir(parents=True)
                for name in ("reims-ctl", "qemu-system-x86_64", "qemu-img"):
                    (bin_dir / name).write_bytes(b"a")
                rows.append(({"tag_name": tag, "published_at": date,
                              "prerelease": not tag.startswith("v")},
                             {release.ASSET: {"size": 1}, "SHA256SUMS": {}}))
            (root / "current").symlink_to(current)
            with (mock.patch.object(release, "ROOT", root),
                  mock.patch.object(release, "STATE", Path(tmp) / "state.json"),
                  mock.patch.object(release, "selectable_releases", return_value=rows)):
                plan = release.cleanup_plan()
                self.assertEqual(plan["candidates"], [])
                with mock.patch.object(release, "qemu_running", return_value=True):
                    with self.assertRaisesRegex(RuntimeError, "running"):
                        release.apply_cleanup("0" * 64)
                self.assertTrue((root / current / "bin/reims-ctl").exists())

    def test_cleanup_refuses_qemu_root_symlink(self):
        with tempfile.TemporaryDirectory() as tmp:
            target = Path(tmp) / "other-data"
            target.mkdir()
            (Path(tmp) / "qemu").symlink_to(target, target_is_directory=True)
            with mock.patch.object(release, "ROOT", Path(tmp) / "qemu"):
                with self.assertRaisesRegex(RuntimeError, "symlink"):
                    release.cleanup_plan()

    def test_safe_tag_blocks_paths(self):
        for bad in ("", "../escape", "a/b", "a b"):
            with self.subTest(bad=bad), self.assertRaises(RuntimeError):
                release.safe_tag(bad)


if __name__ == "__main__":
    unittest.main()
