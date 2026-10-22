#!/usr/bin/env python3
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

HELPER = Path("/private/tmp/dakia-native-translation-arrival.py")


class GuardedHelperTests(unittest.TestCase):
    def run_helper(self, *args):
        return subprocess.run(
            [sys.executable, str(HELPER), *args],
            text=True,
            capture_output=True,
            timeout=5,
        )

    def make_root(self):
        root = Path(tempfile.mkdtemp(prefix="dakia-native-translation-guard-test-"))
        (root / ".dakia-native-mail-safety-root").write_text("wrong\n")
        (root / "profile").mkdir()
        (root / "profile" / ".dakia-native-mail-safety-profile").write_text(
            "dakia-native-mail-safety-fixture-v1\n"
        )
        return root

    def test_missing_root_marker_is_rejected(self):
        root = self.make_root()
        result = self.run_helper("submit-arrival", "--root", str(root))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("root marker", result.stderr)

    def test_action_requires_plan_three_and_runtime_metadata(self):
        root = self.make_root()
        (root / ".dakia-native-mail-safety-root").write_text(
            "dakia-native-mail-safety-fixture-v1\n"
        )
        (root / "native-acceptance-plan.json").write_text(json.dumps({"schemaVersion": 2}))
        result = self.run_helper("prepare-models", "--root", str(root))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("schema version 3", result.stderr)

    def test_source_has_fixed_synthetic_boundary(self):
        source = HELPER.read_text()
        self.assertIn('RECIPIENT = "fictional-reader@localhost"', source)
        self.assertIn('PASSWORD = "synthetic-only-password"', source)
        self.assertIn('"127.0.0.1"', source)
        self.assertNotIn("smtp.gmail.com", source)
        self.assertNotIn("imap.gmail.com", source)
        self.assertIn("refusing any retry", source)
        self.assertIn("possibly-submitted", source)
        self.assertIn("accepted-no-arrival-confirmation", source)
        self.assertIn("EXPECTED_MANIFEST", source)
        self.assertIn("EXPECTED_CACHE", source)
        self.assertIn("must not be a symlink", source)
        self.assertIn('uids == [1, 2, 3, 4]', source)
        self.assertIn('HEADER", "Message-ID", MESSAGE_ID', source)
        self.assertIn('if not arrival["messageIdObserved"]', source)


if __name__ == "__main__":
    unittest.main()
