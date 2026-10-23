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
LIST_FIXTURE = SCRIPT.parent.parent / "testdata" / "gmail-provider-identity-list-payload.fixture.json"
spec = importlib.util.spec_from_file_location("gmail_provider_proof", SCRIPT)
wrapper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(wrapper)


class ReadOnlyFakeImap:
    """Strict imaplib-shaped fake: unexpected commands fail the test."""

    def __init__(self, listing, mutate_second_sent_snapshot=False):
        self.listing = [entry.encode("ascii") for entry in listing]
        self.mutate_second_sent_snapshot = mutate_second_sent_snapshot
        self.records = []
        self.current_mailbox = None
        self.untagged_responses = {}
        self.fetches_by_mailbox = {"INBOX": 0, "[Dakia.test]/Sent.test": 0}

    def login(self, _account, _password):
        self.records.append(("LOGIN",))
        return "OK", [b"logged in"]

    def list(self):
        self.records.append(("LIST",))
        return "OK", self.listing

    def select(self, mailbox, readonly=False):
        self.records.append(("SELECT", mailbox, readonly))
        if not readonly or mailbox not in ('"INBOX"', '"[Dakia.test]/Sent.test"'):
            raise AssertionError("only exact quoted readonly EXAMINE selections are permitted")
        self.current_mailbox = mailbox[1:-1]
        validity = b"111" if self.current_mailbox == "INBOX" else b"222"
        self.untagged_responses = {"EXISTS": [b"2"], "UIDVALIDITY": [validity]}
        return "OK", [b"2"]

    def uid(self, command, *args):
        self.records.append(("UID", command, *args))
        if self.current_mailbox is None:
            raise AssertionError("UID commands require a selected mailbox")
        if command == "SEARCH":
            expected = (None, "HEADER", "SUBJECT", wrapper.quote(wrapper.FIXTURE))
            if args != expected and args != (None, "HEADER", "MESSAGE-ID", '"<fictional-provider-proof@example.test>"'):
                raise AssertionError(f"unexpected bounded search: {args!r}")
            return "OK", [b"11" if self.current_mailbox == "INBOX" else b"22"]
        if command == "FETCH":
            expected_uid = "11" if self.current_mailbox == "INBOX" else "22"
            if args != (expected_uid, "(UID FLAGS X-GM-MSGID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID SUBJECT)])"):
                raise AssertionError(f"unexpected read-only fetch: {args!r}")
            self.fetches_by_mailbox[self.current_mailbox] += 1
            flags = b"\\Seen"
            if self.mutate_second_sent_snapshot and self.current_mailbox == "[Dakia.test]/Sent.test" and self.fetches_by_mailbox[self.current_mailbox] == 2:
                flags = b"\\Flagged"
            metadata = b"2 (UID " + expected_uid.encode("ascii") + b" FLAGS (" + flags + b") X-GM-MSGID 900100)"
            header = (
                b"Message-ID: <fictional-provider-proof@example.test>\r\n"
                + b"Subject: " + wrapper.FIXTURE.encode("ascii") + b"\r\n"
            )
            return "OK", [(metadata, header), b")"]
        raise AssertionError(f"forbidden IMAP command: {command}")

    def logout(self):
        self.records.append(("LOGOUT",))
        return "BYE", [b"logged out"]

    def __getattr__(self, name):
        raise AssertionError(f"forbidden IMAP method: {name}")


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
        fixture = json.loads(LIST_FIXTURE.read_text(encoding="utf-8"))["listPayloads"]
        self.assertEqual(
            wrapper.mailbox_info(fixture[0].encode("ascii")),
            (frozenset({"\\HasNoChildren"}), "INBOX"),
        )
        self.assertEqual(
            wrapper.mailbox_info(fixture[1].encode("ascii")),
            (frozenset({"\\HasNoChildren", "\\Sent"}), "[Dakia.test]/Sent.test"),
        )
        # The complete wire line is not what imaplib.list() returns, but the
        # optional prefix is harmless for focused parser callers.
        self.assertEqual(
            wrapper.mailbox_info(b'* LIST (\\HasNoChildren) "/" "INBOX"'),
            (frozenset({"\\HasNoChildren"}), "INBOX"),
        )
        self.assertIsNone(wrapper.mailbox_info(b'* LIST (\\HasNoChildren) NIL INBOX'))

    def test_observations_execute_only_the_bounded_read_only_imap_contract(self):
        listing = json.loads(LIST_FIXTURE.read_text(encoding="utf-8"))["listPayloads"]
        client = ReadOnlyFakeImap(listing)
        with mock.patch.object(wrapper.imaplib, "IMAP4_SSL", return_value=client):
            observed = wrapper.observations("fictional.proof@gmail.com", "fictional-password")

        self.assertEqual(observed["schemaVersion"], 1)
        self.assertEqual(observed["roundtripId"], wrapper.ROUNDTRIP_ID)
        self.assertEqual(len(observed["observations"]), 2)
        self.assertEqual({item["localMailbox"] for item in observed["observations"]}, {"INBOX", "Sent"})
        for item in observed["observations"]:
            self.assertNotIn("flags", item)
            self.assertNotIn("messageId", item)
            self.assertTrue(item["flagsUnchanged"])
        self.assertEqual(
            [record[0] for record in client.records],
            ["LOGIN", "LIST", "SELECT", "UID", "UID", "SELECT", "UID", "UID", "SELECT", "UID", "UID", "SELECT", "UID", "UID", "LOGOUT"],
        )
        selected = [record for record in client.records if record[0] == "SELECT"]
        self.assertEqual(len(selected), 4)
        self.assertTrue(all(record[2] is True for record in selected))
        searches = [record for record in client.records if record[:2] == ("UID", "SEARCH")]
        fetches = [record for record in client.records if record[:2] == ("UID", "FETCH")]
        self.assertEqual(len(searches), 4)
        self.assertEqual(len(fetches), 4)
        self.assertEqual(searches[0][2:], (None, "HEADER", "SUBJECT", wrapper.quote(wrapper.FIXTURE)))
        self.assertTrue(all(record[2:4] == (None, "HEADER") for record in searches[1:]))
        self.assertTrue(all("BODY.PEEK" in record[-1] for record in fetches))
        self.assertFalse(any("STORE" in str(record) or "APPEND" in str(record) or "EXPUNGE" in str(record) for record in client.records))

    def test_observations_rejects_duplicate_special_mailboxes_before_selection(self):
        fixture = json.loads(LIST_FIXTURE.read_text(encoding="utf-8"))["listPayloads"]
        duplicate_inbox = ReadOnlyFakeImap([fixture[0], fixture[0], fixture[1]])
        with mock.patch.object(wrapper.imaplib, "IMAP4_SSL", return_value=duplicate_inbox), self.assertRaises(wrapper.SafeFailure):
            wrapper.observations("fictional.proof@gmail.com", "fictional-password")
        self.assertEqual([record[0] for record in duplicate_inbox.records], ["LOGIN", "LIST", "LOGOUT"])

        duplicate_sent = ReadOnlyFakeImap([fixture[0], fixture[1], fixture[1]])
        with mock.patch.object(wrapper.imaplib, "IMAP4_SSL", return_value=duplicate_sent), self.assertRaises(wrapper.SafeFailure):
            wrapper.observations("fictional.proof@gmail.com", "fictional-password")
        self.assertEqual([record[0] for record in duplicate_sent.records], ["LOGIN", "LIST", "LOGOUT"])

    def test_observations_rejects_a_changed_flag_without_mutating_the_mailbox(self):
        listing = json.loads(LIST_FIXTURE.read_text(encoding="utf-8"))["listPayloads"]
        client = ReadOnlyFakeImap(listing, mutate_second_sent_snapshot=True)
        with mock.patch.object(wrapper.imaplib, "IMAP4_SSL", return_value=client), self.assertRaises(wrapper.SafeFailure):
            wrapper.observations("fictional.proof@gmail.com", "fictional-password")
        self.assertEqual(client.records[-1], ("LOGOUT",))
        self.assertFalse(any("STORE" in str(record) or "APPEND" in str(record) or "EXPUNGE" in str(record) for record in client.records))

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
