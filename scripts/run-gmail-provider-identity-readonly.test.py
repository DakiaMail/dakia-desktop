#!/usr/bin/env python3
"""Offline unit tests for the bounded Gmail provider-proof wrapper."""

import importlib.util
import io
import os
from pathlib import Path
import json
import subprocess
import sys
import tempfile
import unittest
from unittest import mock
from contextlib import redirect_stdout


SCRIPT = Path(__file__).with_name("run-gmail-provider-identity-readonly.py")
spec = importlib.util.spec_from_file_location("gmail_provider_proof", SCRIPT)
wrapper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wrapper)


class GmailProviderProofWrapperTests(unittest.TestCase):
    def temporary_programme_repo(self):
        directory = tempfile.TemporaryDirectory()
        root = Path(directory.name)
        (root / "scripts").mkdir()
        (root / "scripts" / "run-mail-safety.mjs").write_text(
            SCRIPT.with_name("run-mail-safety.mjs").read_text(encoding="utf-8"),
            encoding="utf-8",
        )
        (root / "scripts" / "owned.mjs").write_text("export const value = 1;\n", encoding="utf-8")
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        subprocess.run(["git", "add", "."], cwd=root, check=True)
        return directory, root

    def write_manifest(self, root):
        snapshot = wrapper.programme_source_snapshot(root)
        path = root / "manifest.json"
        path.write_text(json.dumps(snapshot), encoding="utf-8")
        os.chmod(path, 0o600)
        return path, snapshot

    def test_fixture_is_the_one_authorized_existing_message(self):
        self.assertEqual(
            wrapper.FIXTURE,
            "Dakia disposable provider roundtrip 45bacd384f0c4a74893f14e4fea23d9a",
        )

    def test_credentials_parse_literals_without_sourcing_shell_content(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "credentials.env"
            path.write_text(
                "GMAIL_ACCOUNT='fictional.proof@gmail.com'\nGMAIL_PASSWORD='fictional value'\n",
                encoding="utf-8",
            )
            os.chmod(path, 0o600)
            account, password, original = wrapper.credentials(path)
        self.assertEqual(account, "fictional.proof@gmail.com")
        self.assertEqual(password, "fictional value")
        self.assertNotIn(b"$", original)

    def test_credentials_reject_duplicate_and_unowned_modes(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "credentials.env"
            path.write_text(
                "GMAIL_ACCOUNT=a@gmail.com\nGMAIL_ACCOUNT=b@gmail.com\nGMAIL_PASSWORD=value\n",
                encoding="utf-8",
            )
            os.chmod(path, 0o600)
            with self.assertRaises(wrapper.SafeFailure):
                wrapper.credentials(path)
            path.write_text("GMAIL_ACCOUNT=a@gmail.com\nGMAIL_PASSWORD=value\n", encoding="utf-8")
            os.chmod(path, 0o644)
            with self.assertRaises(wrapper.SafeFailure):
                wrapper.credentials(path)

    def test_mailbox_parser_accepts_plain_inbox_and_real_sent_flag_only(self):
        self.assertEqual(
            wrapper.mailbox_info(b'* LIST (\\HasNoChildren) "/" "INBOX"'),
            (frozenset({"\\HasNoChildren"}), "INBOX"),
        )
        self.assertEqual(
            wrapper.mailbox_info(b'* LIST (\\HasNoChildren \\Sent) "/" "[Gmail]/&ZeVnLIqe-"'),
            (frozenset({"\\HasNoChildren", "\\Sent"}), "[Gmail]/&ZeVnLIqe-"),
        )
        self.assertIsNone(wrapper.mailbox_info(b'* LIST (\\HasNoChildren) NIL INBOX'))

    def test_source_manifest_accepts_the_programme_snapshot_and_detects_mutation(self):
        directory, root = self.temporary_programme_repo()
        with directory:
            manifest, snapshot = self.write_manifest(root)
            self.assertEqual(wrapper.source_digest(root, manifest), snapshot["digest"])
            (root / "scripts" / "owned.mjs").write_text("export const value = 2;\n", encoding="utf-8")
            with self.assertRaises(wrapper.SafeFailure):
                wrapper.source_digest(root, manifest)

    def test_source_manifest_detects_untracked_programme_inventory_addition(self):
        directory, root = self.temporary_programme_repo()
        with directory:
            manifest, _ = self.write_manifest(root)
            (root / "scripts" / "untracked-proof.mjs").write_text("export {};\n", encoding="utf-8")
            with self.assertRaises(wrapper.SafeFailure):
                wrapper.source_digest(root, manifest)

    def test_builder_selects_copies_and_hashes_the_real_core_test_binary(self):
        with tempfile.TemporaryDirectory(dir="/private/tmp") as directory:
            runner, digest = wrapper.build_private_test_runner(SCRIPT.parent.parent, Path(directory))
            self.assertTrue(runner.is_file())
            self.assertEqual(runner.stat().st_mode & 0o777, 0o500)
            self.assertEqual(digest, wrapper.sha256(runner.read_bytes()))

    def test_imap_protocol_failure_writes_only_a_private_sanitized_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env_file = root / "credentials.env"
            env_file.write_text("GMAIL_ACCOUNT=fake@gmail.com\nGMAIL_PASSWORD=private-password\n", encoding="utf-8")
            os.chmod(env_file, 0o600)
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps(wrapper.programme_source_snapshot(SCRIPT.parent.parent)), encoding="utf-8")
            os.chmod(manifest, 0o600)
            receipt = root / "failed-receipt.json"
            runner = root / "sealed-runner"
            runner.write_bytes(b"fixture runner")
            output = io.StringIO()
            with mock.patch.object(wrapper, "build_private_test_runner", return_value=(runner, "fixture-runner-sha")), mock.patch.object(wrapper, "observations", side_effect=wrapper.imaplib.IMAP4.error("provider response private-password")), mock.patch.object(sys, "argv", [str(SCRIPT), "--env-file", str(env_file), "--source-manifest", str(manifest), "--receipt", str(receipt), "--run"]), redirect_stdout(output):
                self.assertEqual(wrapper.main(), 1)
            result = receipt.read_text(encoding="utf-8")
            self.assertEqual(json.loads(result)["resultCode"], "read_only_provider_proof_failed")
            self.assertNotIn("private-password", result)
            self.assertNotIn("provider response", output.getvalue())
            self.assertEqual(receipt.stat().st_mode & 0o777, 0o600)

    def test_mocked_run_passes_only_with_redacted_valid_child_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env_file = root / "credentials.env"
            env_file.write_text("GMAIL_ACCOUNT=fake@gmail.com\nGMAIL_PASSWORD=fake-password\n", encoding="utf-8")
            os.chmod(env_file, 0o600)
            manifest = root / "manifest.json"
            manifest.write_text(json.dumps(wrapper.programme_source_snapshot(SCRIPT.parent.parent)), encoding="utf-8")
            os.chmod(manifest, 0o600)
            receipt = root / "receipt.json"
            real_run = subprocess.run
            runner = root / "sealed-runner"
            runner.write_bytes(b"fixture runner")
            observation = {"schemaVersion": 1, "roundtripId": wrapper.ROUNDTRIP_ID, "expectedMessageId": "<fake@example.test>", "observations": [{"localMailbox": "INBOX", "remoteWireName": "INBOX", "uid": 1, "uidValidity": 2, "exists": 1, "flags": "a", "flagsUnchanged": True, "gmailMessageId": "9001", "messageId": "<fake@example.test>"}, {"localMailbox": "Sent", "remoteWireName": "Sent", "uid": 2, "uidValidity": 3, "exists": 1, "flags": "b", "flagsUnchanged": True, "gmailMessageId": "9001", "messageId": "<fake@example.test>"}]}
            def fake_child(args, **kwargs):
                if args[0] != str(runner):
                    return real_run(args, **kwargs)
                output = Path(kwargs["env"][wrapper.OUTPUT_PATH_ENV])
                digest = kwargs["env"][wrapper.SOURCE_DIGEST_ENV]
                evidence = {"schemaVersion": 2, "outcome": "passed", "resultCode": "b004_production_gmail_identity_store_reopen_label_dedup", "sourceDigest": digest, "providerScope": "dedicated_disposable_gmail", "roundtripId": wrapper.ROUNDTRIP_ID, "productionFullFetch": True, "parserVerified": True, "profileSchemaVersion": 3, "rfcMessageIdMatchedAcrossCopies": True, "providerIdentityMatchedAcrossCopies": True, "providerIdentitySelectedThreadKey": True, "providerIdentitySurvivedReopen": True, "labelCopiesDeduplicated": True, "epochFencedStore": True, "cachePublished": True, "storeReopenVerified": True, "physicalCopies": 2, "displayRepresentatives": 1, "distinctPhysicalLocators": True, "expectedSubjectSha256": wrapper.sha256(wrapper.FIXTURE.encode()), "expectedBodySha256": wrapper.sha256(wrapper.FIXTURE.encode()), "limitations": ["does_not_exercise_sync_all_or_snapshot_finalization", "does_not_validate_dakia_list_parser_or_mailbox_plan_resolution", "live_copies_share_both_rfc_and_gmail_ids_but_thread_key_proves_gmail_identity_precedence", "does_not_prove_reader_frontend_or_native_webview_rendering"]}
                output.write_text(json.dumps(evidence), encoding="utf-8")
                os.chmod(output, 0o600)
                return subprocess.CompletedProcess(args, 0, b"", b"")
            with mock.patch.object(wrapper, "observations", return_value={**observation, "_before": {"INBOX": observation["observations"][0], "Sent": observation["observations"][1]}}), mock.patch.object(wrapper, "build_private_test_runner", return_value=(runner, "fixture-runner-sha")), mock.patch.object(wrapper.subprocess, "run", side_effect=fake_child), mock.patch.object(sys, "argv", [str(SCRIPT), "--env-file", str(env_file), "--source-manifest", str(manifest), "--receipt", str(receipt), "--run"]):
                self.assertEqual(wrapper.main(), 0)
            result = json.loads(receipt.read_text(encoding="utf-8"))
            self.assertEqual(result["outcome"], "passed")
            self.assertNotIn("fake@gmail.com", receipt.read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
