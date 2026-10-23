#!/usr/bin/env python3
"""Run the explicitly approved, bounded Gmail provider-identity proof.

The wrapper deliberately parses a named private env file without sourcing it,
performs only IMAP read commands, and sends no SMTP command.  `--run` is
required so preparation and review cannot accidentally contact Gmail.
"""

from __future__ import annotations

import argparse
import hashlib
import imaplib
import json
import os
from pathlib import Path
import re
import shlex
import stat
import subprocess
import sys
import tempfile
import shutil
from email.parser import BytesHeaderParser

ROUNDTRIP_ID = "45bacd384f0c4a74893f14e4fea23d9a"
FIXTURE = f"Dakia disposable provider roundtrip {ROUNDTRIP_ID}"
MAX_ENV_BYTES = 32 * 1024
MAX_OUTPUT_BYTES = 64 * 1024
INPUT_PATH_ENV = "DAKIA_GMAIL_PROVIDER_IDENTITY_INPUT"
OUTPUT_PATH_ENV = "DAKIA_GMAIL_PROVIDER_IDENTITY_OUTPUT"
SOURCE_DIGEST_ENV = "DAKIA_GMAIL_PROVIDER_IDENTITY_SOURCE_DIGEST"
RUN_AUTHORIZATION_ENV = "DAKIA_GMAIL_PROVIDER_IDENTITY_AUTHORIZED"


class SafeFailure(Exception):
    pass


def sha256(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def private_read(path: Path, maximum: int) -> bytes:
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or stat.S_IMODE(info.st_mode) not in (0o400, 0o600) or info.st_uid != os.geteuid() or info.st_nlink != 1:
        raise SafeFailure("private input is not an owned 0400 or 0600 regular file")
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    try:
        now = os.fstat(fd)
        if (now.st_dev, now.st_ino) != (info.st_dev, info.st_ino):
            raise SafeFailure("private input changed during open")
        data = os.read(fd, maximum + 1)
        if len(data) > maximum:
            raise SafeFailure("private input exceeds its bound")
        return data
    finally:
        os.close(fd)


def credentials(path: Path) -> tuple[str, str, bytes]:
    raw = private_read(path, MAX_ENV_BYTES)
    fields: dict[str, str] = {}
    for line in raw.decode("utf-8").splitlines():
        match = re.fullmatch(r"\s*(?:export\s+)?(GMAIL_ACCOUNT|GMAIL_PASSWORD)\s*=\s*(.*?)\s*", line)
        if not match:
            continue
        key, encoded = match.groups()
        if key in fields:
            raise SafeFailure("credential file has duplicate required values")
        parser = shlex.shlex(encoded, posix=True)
        parser.whitespace_split, parser.commenters = True, "#"
        parsed = list(parser)
        if len(parsed) != 1 or any(char in parsed[0] for char in "\x00\r\n"):
            raise SafeFailure("credential file has invalid required values")
        fields[key] = parsed[0]
    account, password = fields.get("GMAIL_ACCOUNT", ""), fields.get("GMAIL_PASSWORD", "")
    if not re.fullmatch(r"[A-Za-z0-9._+-]+@gmail\.com", account) or not password or len(password) > 256:
        raise SafeFailure("credential file does not contain an approved Gmail account")
    return account, password, raw


def programme_source_snapshot(repo: Path) -> dict:
    programme = repo / "scripts" / "run-mail-safety.mjs"
    code = "import { pathToFileURL } from 'node:url'; process.chdir(process.argv[1]); const { sourceSnapshot } = await import(pathToFileURL(process.argv[2])); process.stdout.write(JSON.stringify(sourceSnapshot()));"
    completed = subprocess.run(
        ["node", "--input-type=module", "-e", code, str(repo), str(programme)],
        capture_output=True,
        check=False,
        timeout=30,
    )
    if completed.returncode != 0 or len(completed.stdout) > 4096:
        raise SafeFailure("programme source snapshot is unavailable")
    try:
        snapshot = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        raise SafeFailure("programme source snapshot is invalid") from error
    if snapshot is None:
        raise SafeFailure("programme source snapshot is unavailable")
    return snapshot


def source_digest(repo: Path, manifest_path: Path) -> str:
    manifest = json.loads(private_read(manifest_path, 2 * 1024 * 1024))
    if set(manifest) != {"algorithm", "digest", "fileCount"} or manifest["algorithm"] != "sha256-path-and-content-manifest":
        raise SafeFailure("source manifest has an invalid schema")
    actual = programme_source_snapshot(repo)
    if actual != manifest or not re.fullmatch(r"[0-9a-f]{64}", str(actual.get("digest", ""))):
        raise SafeFailure("source manifest digest differs from checkout")
    return actual["digest"]


def quote(value: str) -> str:
    return '"' + value.replace("\\", "\\\\").replace('"', '\\"') + '"'


def mailbox_info(line: bytes) -> tuple[frozenset[str], str] | None:
    try:
        text = line.decode("ascii")
    except UnicodeDecodeError:
        return None
    match = re.fullmatch(r'\* LIST \(([^)]*)\) (?:NIL|"(?:[^"\\]|\\.)*") "((?:[^"\\]|\\.)*)"', text)
    if not match:
        return None
    flags, name = match.groups()
    return frozenset(flags.split()), name.replace('\\"', '"').replace('\\\\', '\\')


def readonly_snapshot(client: imaplib.IMAP4_SSL, role: str, mailbox: str, message_id: str | None) -> dict:
    if client.select(quote(mailbox), readonly=True)[0] != "OK":
        raise SafeFailure("read-only mailbox selection failed")
    exists = int(client.untagged_responses.get("EXISTS", [b"0"])[-1])
    validity = int(client.untagged_responses.get("UIDVALIDITY", [b"0"])[-1])
    if not exists or not validity:
        raise SafeFailure("mailbox identity is incomplete")
    if message_id is None:
        status, data = client.uid("SEARCH", None, "HEADER", "SUBJECT", quote(FIXTURE))
    else:
        status, data = client.uid("SEARCH", None, "HEADER", "MESSAGE-ID", quote(message_id))
    if status != "OK" or len(data) != 1:
        raise SafeFailure("bounded target search failed")
    matches = data[0].split()
    if len(matches) != 1 or not matches[0].isdigit():
        raise SafeFailure("approved target did not resolve to one physical copy")
    uid = int(matches[0])
    status, data = client.uid("FETCH", str(uid), "(UID FLAGS X-GM-MSGID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID SUBJECT)])")
    if status != "OK" or len(data) < 2 or not isinstance(data[0], tuple):
        raise SafeFailure("bounded target header fetch failed")
    metadata, header = data[0]
    if not isinstance(header, bytes):
        raise SafeFailure("bounded target header literal is invalid")
    match = re.search(rb"X-GM-MSGID ([0-9]{1,20})", metadata)
    if not match:
        raise SafeFailure("Gmail provider identity was unavailable")
    parsed = BytesHeaderParser().parsebytes(header)
    observed_id = parsed.get("Message-ID", "")
    if not re.fullmatch(r"<[^\s<>]+>", observed_id) or parsed.get("Subject") != FIXTURE:
        raise SafeFailure("bounded target headers differ from the approved message")
    return {"localMailbox": role, "remoteWireName": mailbox, "uid": uid, "uidValidity": validity, "exists": exists, "flags": sha256(metadata), "flagsUnchanged": True, "gmailMessageId": match.group(1).decode(), "messageId": observed_id}


def observations(account: str, password: str) -> dict:
    client = imaplib.IMAP4_SSL("imap.gmail.com", 993, timeout=20)
    try:
        if client.login(account, password)[0] != "OK":
            raise SafeFailure("authentication failed")
        status, listing = client.list()
        if status != "OK" or not listing:
            raise SafeFailure("mailbox listing failed")
        parsed = [mailbox_info(line) for line in listing if line]
        parsed = [entry for entry in parsed if entry is not None]
        inboxes = [name for flags, name in parsed if name == "INBOX"]
        sent_mailboxes = [name for flags, name in parsed if "\\Sent" in flags]
        if len(inboxes) != 1 or len(sent_mailboxes) != 1 or inboxes[0] == sent_mailboxes[0]:
            raise SafeFailure("required Inbox and Sent mailboxes were not uniquely resolved")
        inbox, sent = inboxes[0], sent_mailboxes[0]
        first_sent = readonly_snapshot(client, "Sent", sent, None)
        message_id = first_sent["messageId"]
        first_inbox = readonly_snapshot(client, "INBOX", inbox, message_id)
        # Read the two snapshots twice before and once after the Dakia core
        # path. The wrapper compares all immutable locators and flags.
        before = {"INBOX": readonly_snapshot(client, "INBOX", inbox, message_id), "Sent": readonly_snapshot(client, "Sent", sent, message_id)}
        if before["INBOX"] != first_inbox or before["Sent"] != first_sent or before["INBOX"]["gmailMessageId"] != before["Sent"]["gmailMessageId"]:
            raise SafeFailure("pre-run target or flags were not invariant")
        return {"schemaVersion": 1, "roundtripId": ROUNDTRIP_ID, "expectedMessageId": message_id, "observations": [{key: value for key, value in item.items() if key not in ("flags", "messageId")} for item in (before["INBOX"], before["Sent"])], "_before": before}
    finally:
        try:
            client.logout()
        except Exception:
            pass


def validate_core_evidence(evidence: dict, digest: str) -> None:
    expected = {
        "schemaVersion", "outcome", "resultCode", "sourceDigest", "providerScope", "roundtripId",
        "productionFullFetch", "parserVerified", "profileSchemaVersion",
        "rfcMessageIdMatchedAcrossCopies", "providerIdentityMatchedAcrossCopies",
        "providerIdentitySelectedThreadKey", "providerIdentitySurvivedReopen",
        "labelCopiesDeduplicated", "epochFencedStore", "cachePublished",
        "storeReopenVerified", "physicalCopies", "displayRepresentatives",
        "distinctPhysicalLocators", "expectedSubjectSha256", "expectedBodySha256", "limitations",
    }
    if set(evidence) != expected or evidence.get("schemaVersion") != 2 or evidence.get("outcome") != "passed" or evidence.get("resultCode") != "b004_production_gmail_identity_store_reopen_label_dedup" or evidence.get("sourceDigest") != digest or evidence.get("providerScope") != "dedicated_disposable_gmail" or evidence.get("roundtripId") != ROUNDTRIP_ID:
        raise SafeFailure("core proof evidence is invalid")
    if evidence.get("profileSchemaVersion") != 3 or evidence.get("physicalCopies") != 2 or evidence.get("displayRepresentatives") != 1:
        raise SafeFailure("core proof evidence is invalid")
    flags = ("productionFullFetch", "parserVerified", "rfcMessageIdMatchedAcrossCopies", "providerIdentityMatchedAcrossCopies", "providerIdentitySelectedThreadKey", "providerIdentitySurvivedReopen", "labelCopiesDeduplicated", "epochFencedStore", "cachePublished", "storeReopenVerified", "distinctPhysicalLocators")
    limitations = [
        "does_not_exercise_sync_all_or_snapshot_finalization",
        "does_not_validate_dakia_list_parser_or_mailbox_plan_resolution",
        "live_copies_share_both_rfc_and_gmail_ids_but_thread_key_proves_gmail_identity_precedence",
        "does_not_prove_reader_frontend_or_native_webview_rendering",
    ]
    if any(evidence.get(flag) is not True for flag in flags) or evidence.get("expectedSubjectSha256") != sha256(FIXTURE.encode()) or evidence.get("expectedBodySha256") != sha256(FIXTURE.encode()) or evidence.get("limitations") != limitations:
        raise SafeFailure("core proof evidence is invalid")


def build_private_test_runner(repo: Path, root: Path) -> tuple[Path, str]:
    build_env = {key: os.environ[key] for key in ("PATH", "HOME", "CARGO_HOME", "RUSTUP_HOME", "TMPDIR") if key in os.environ}
    completed = subprocess.run(
        ["cargo", "test", "--offline", "--locked", "-p", "dakia-core", "--lib", "--no-run", "--message-format=json"],
        cwd=repo, env=build_env, capture_output=True, timeout=300, check=False,
    )
    if completed.returncode != 0 or len(completed.stdout) > 4 * 1024 * 1024:
        raise SafeFailure("credential-free core test build failed")
    executable = None
    for line in completed.stdout.splitlines():
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        if record.get("reason") == "compiler-artifact" and record.get("target", {}).get("name") == "dakia_core" and record.get("executable"):
            executable = Path(record["executable"])
    if executable is None or not executable.is_file():
        raise SafeFailure("credential-free core test runner is unavailable")
    private = root / "dakia-core-gmail-proof-tests"
    shutil.copyfile(executable, private)
    os.chmod(private, 0o500)
    return private, sha256(private.read_bytes())


def main() -> int:
    previous_umask = os.umask(0o077)
    parser = argparse.ArgumentParser()
    parser.add_argument("--env-file", type=Path, required=True)
    parser.add_argument("--source-manifest", type=Path, required=True)
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--run", action="store_true")
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    receipt = {"schemaVersion": 1, "outcome": "failed", "resultCode": "wrapper_failed", "roundtripId": ROUNDTRIP_ID, "remoteMutationRequested": False, "rawProviderObservationsRetained": False}
    try:
        digest = source_digest(repo, args.source_manifest)
        receipt.update(sourceDigest=digest)
        if not args.run:
            receipt.update(outcome="prepared", resultCode="review_required_before_live_provider_run")
        else:
            with tempfile.TemporaryDirectory(prefix="dakia-gmail-provider-proof-", dir="/private/tmp") as root:
                os.chmod(root, 0o700)
                runner, runner_sha = build_private_test_runner(repo, Path(root))
                account, password, original_env = credentials(args.env_file)
                input_data = observations(account, password)
                input_path, output_path = Path(root) / "input.json", Path(root) / "evidence.json"
                input_path.write_text(
                    json.dumps({key: value for key, value in input_data.items() if key != "_before"}),
                    encoding="utf-8",
                )
                os.chmod(input_path, 0o600)
                child_env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "GMAIL_ACCOUNT": account, "GMAIL_PASSWORD": password, INPUT_PATH_ENV: str(input_path), OUTPUT_PATH_ENV: str(output_path), SOURCE_DIGEST_ENV: digest, RUN_AUTHORIZATION_ENV: "approved-read-only-b004"}
                completed = subprocess.run([str(runner), "gmail_provider_identity_proof_tests::production_gmail_provider_identity_store_reopen_label_dedup_is_read_only", "--ignored", "--exact"], cwd=repo, env=child_env, capture_output=True, timeout=180, check=False)
                output = completed.stdout + completed.stderr
                sensitive = [account.encode(), password.encode(), input_data["expectedMessageId"].encode(), input_data["_before"]["INBOX"]["gmailMessageId"].encode()]
                if len(output) > MAX_OUTPUT_BYTES or any(value in output for value in sensitive):
                    raise SafeFailure("core proof output was unsafe to retain")
                if completed.returncode != 0 or not output_path.exists():
                    raise SafeFailure("core proof did not pass")
                evidence = json.loads(private_read(output_path, MAX_OUTPUT_BYTES))
                validate_core_evidence(evidence, digest)
                after = observations(account, password)["_before"]
                if after != input_data["_before"]:
                    raise SafeFailure("target locators or flags changed during the proof")
                if credentials(args.env_file)[2] != original_env:
                    raise SafeFailure("credentials file changed")
                if source_digest(repo, args.source_manifest) != digest:
                    raise SafeFailure("source changed during the provider proof")
                receipt.update(outcome="passed", resultCode="b004_provider_identity_proof_wrapper_passed", credentialsFileUnchanged=True, runnerSha256=runner_sha, coreEvidence=evidence)
    except (SafeFailure, imaplib.IMAP4.error, subprocess.SubprocessError, OSError, ValueError, json.JSONDecodeError):
        receipt["resultCode"] = "read_only_provider_proof_failed"
    args.receipt.parent.mkdir(parents=True, exist_ok=True)
    with args.receipt.open("x", encoding="utf-8") as stream:
        json.dump(receipt, stream, sort_keys=True, indent=2)
        stream.write("\n")
    print(json.dumps({"outcome": receipt["outcome"], "resultCode": receipt["resultCode"], "roundtripId": ROUNDTRIP_ID, "remoteMutationRequested": False}, sort_keys=True))
    os.umask(previous_umask)
    return 0 if receipt["outcome"] in ("passed", "prepared") else 1


if __name__ == "__main__":
    raise SystemExit(main())
