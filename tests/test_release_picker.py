"""Regression tests for the GitHub Release catalog used by the Tahoe menu."""
import contextlib
import importlib.util
import io
import json
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
            "exp-native-v3-3-e4a42df79f14",
            "exp-fat-lto-3-e4a42df79f14",
            "exp-profiler-3-e4a42df79f14",
        ])
        self.assertIn("不是加速", result[-1]["test_label"])

    def test_current_old_build_remains_reachable(self):
        result = release.curated_releases(self.sample_rows(old_selected=True))
        self.assertEqual(len(result), 6)
        self.assertEqual(result[-1]["tag"], "exp-native-v3-1-2a63d397e686")
        self.assertIn("当前选中旧版", result[-1]["test_label"])

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
            self.assertEqual(len(json.loads(out.getvalue())), 5)

    def test_new_qemu_thp_candidate_is_high_priority(self):
        rows = self.sample_rows()
        rows.append(row("exp-qemu-thp-7-bf4a3a511ecb", "2026-10-31T06:00:00Z"))
        tags = [x["tag"] for x in release.curated_releases(rows)]
        self.assertEqual(tags[2], "exp-qemu-thp-7-bf4a3a511ecb")
        self.assertIn("QEMU", release.describe_release(rows[-1])["test_label"])

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

    def test_safe_tag_blocks_paths(self):
        for bad in ("", "../escape", "a/b", "a b"):
            with self.subTest(bad=bad), self.assertRaises(RuntimeError):
                release.safe_tag(bad)


if __name__ == "__main__":
    unittest.main()
