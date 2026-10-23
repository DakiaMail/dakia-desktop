#!/usr/bin/env python3
"""Guarded local fictional-mailbox helper for native translation acceptance.

This helper never launches Dakia and never contacts a non-loopback host.  It
has two explicit actions: prepare-models installs already verified public
model bytes into one owned native fixture profile, and submit-arrival submits
one fixed-ID fictional message exactly once after confirming the original
three-message mailbox state.
"""

import argparse
import email.message
import hashlib
import imaplib
import json
import os
from pathlib import Path
import smtplib
import ssl
import subprocess
import sys
import tempfile
import time
import uuid

EXPECTED_REPO = Path("/Users/arsalan/code/codex-worktrees/36f9/dakia-desktop")
EXPECTED_MANIFEST = EXPECTED_REPO / "apps/desktop/src-tauri/src/translation_models.json"
EXPECTED_CACHE = Path("/private/tmp/dakia-bergamot-privacy-cache")
ROOT_MARKER = "dakia-native-mail-safety-fixture-v1\n"
PROFILE_MARKER = ROOT_MARKER
RECIPIENT = "fictional-reader@localhost"
USERNAME = RECIPIENT
PASSWORD = "synthetic-only-password"
MESSAGE_ID = "<dakia-estonian-translation-arrival-20261021@localhost>"
MODEL_FILES = ("model.bin", "shortlist.bin", "vocab-0.spm")
MODEL_SOURCE = "et"
MODEL_TARGET = "en"
JOURNAL_NAME = ".dakia-estonian-translation-arrival-journal.json"


def die(message):
    raise SystemExit(f"guarded helper refused: {message}")


def canonical(path):
    value = Path(path).expanduser()
    if not value.is_absolute():
        die(f"path must be absolute: {path}")
    return value.resolve(strict=False)


def sha256(path):
    digest = hashlib.sha256()
    size = 0
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
            size += len(chunk)
    return {"bytes": size, "sha256": digest.hexdigest()}


def read_json(path):
    try:
        with open(path, encoding="utf-8") as stream:
            return json.load(stream)
    except Exception as error:
        die(f"could not read {path}: {error}")


def require_owned(path, root, label):
    raw_path = Path(path)
    if raw_path.is_symlink():
        die(f"{label} must not be a symlink: {raw_path}")
    path = canonical(path)
    if not path.is_file() or not path.is_relative_to(root):
        die(f"{label} is not an owned file inside the harness root: {path}")
    return path


def guard_root(root_arg):
    root = canonical(root_arg)
    if not root.is_dir():
        die(f"harness root does not exist: {root}")
    if (root / ".dakia-native-mail-safety-root").read_text() != ROOT_MARKER:
        die("harness root marker is missing or wrong")
    profile = root / "profile"
    if not profile.is_dir() or (profile / ".dakia-native-mail-safety-profile").read_text() != PROFILE_MARKER:
        die("profile marker is missing or wrong")

    plan_path = require_owned(root / "native-acceptance-plan.json", root, "acceptance plan")
    plan = read_json(plan_path)
    if plan.get("schemaVersion") != 3:
        die("acceptance plan is not schema version 3")
    listener = plan.get("listeners", {}).get("http", {})
    if listener.get("host") != "127.0.0.1" or not isinstance(listener.get("port"), int):
        die("acceptance plan does not identify the owned HTTP loopback listener")

    launch_path = require_owned(root / "sleep-wake-fixture.json", root, "runtime metadata")
    launch = read_json(launch_path)
    pid = launch.get("pid")
    executable = require_owned(launch.get("appExecutable", ""), root, "runtime executable")
    if not isinstance(pid, int) or pid <= 0:
        die("runtime PID is invalid")
    if not executable.is_file() or not os.access(executable, os.X_OK):
        die("runtime executable is missing or not executable")
    actual_sha = sha256(executable)["sha256"]
    if actual_sha != launch.get("appSha256"):
        die("runtime executable SHA-256 changed after launch")
    command = subprocess.run(
        ["ps", "-p", str(pid), "-o", "command="],
        capture_output=True,
        text=True,
        check=False,
        timeout=3,
    ).stdout.strip()
    if executable.name not in command:
        die("runtime PID is not the recorded owned executable")

    for key in ("imapPort", "smtpPort"):
        if not isinstance(launch.get(key), int) or not (1 <= launch[key] <= 65535):
            die(f"{key} is not a valid loopback port")
    # Python's stdlib SSL loader expects the fixture's PEM CA. The app itself
    # receives tls.der via DAKIA_NATIVE_MAIL_SAFETY_CA_DER_PATH.
    ca = require_owned(root / "tls" / "ca.crt", root, "fixture CA PEM")
    source = require_owned(root / "source-before-build.json", root, "source snapshot")
    source_record = read_json(source)
    if (
        source_record.get("algorithm") != "sha256-path-and-content-manifest"
        or not isinstance(source_record.get("digest"), str)
        or len(source_record.get("digest", "")) != 64
        or not isinstance(source_record.get("fileCount"), int)
        or source_record.get("fileCount", 0) <= 0
    ):
        die("source snapshot is empty")
    return {
        "root": root,
        "profile": profile,
        "plan": plan,
        "launch": launch,
        "ca": ca,
        "source": source_record,
    }


def model_expectations():
    manifest_path = EXPECTED_MANIFEST
    if not manifest_path.is_file() or manifest_path.is_symlink():
        die(f"the exact checkout manifest is unavailable: {manifest_path}")
    manifest = read_json(canonical(manifest_path))
    models = [m for m in manifest.get("models", []) if m.get("source") == MODEL_SOURCE and m.get("target") == MODEL_TARGET]
    if len(models) != 1:
        die("manifest does not contain exactly one Estonian-to-English model")
    model = models[0]
    artifacts = [model["files"]["model"], model["files"]["shortlist"], *model["files"]["vocabs"]]
    if len(artifacts) != 3:
        die("Estonian model does not contain exactly three required artifacts")
    for artifact in artifacts:
        url, size, digest = artifact
        if not url.startswith("https://firefox-settings-attachments.cdn.mozilla.net/") or not isinstance(size, int) or len(digest) != 64:
            die("manifest artifact is not an approved pinned Mozilla asset")
    return model, {name: artifact for name, artifact in zip(MODEL_FILES, artifacts)}


def atomic_write_exclusive(path, payload):
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    fd = os.open(path, flags, 0o600)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as stream:
            json.dump(payload, stream, indent=2)
            stream.write("\n")
            stream.flush()
            os.fsync(stream.fileno())
    except Exception:
        try:
            os.unlink(path)
        except FileNotFoundError:
            pass
        raise


def prepare_models(context):
    model, expected = model_expectations()
    cache = canonical(EXPECTED_CACHE)
    if not cache.is_dir():
        die(f"verified model cache does not exist: {cache}")
    cache_records = {}
    for name, (_url, size, digest) in expected.items():
        source = cache / name
        if not source.is_file() or sha256(source) != {"bytes": size, "sha256": digest}:
            die(f"cached {name} is absent or fails the pinned size/hash")
        cache_records[name] = {"source": str(source), **sha256(source)}

    model_root = context["profile"] / "translation-models"
    model_root.mkdir(mode=0o700, exist_ok=True)
    os.chmod(model_root, 0o700)
    final = model_root / "et-en"
    if final.exists():
        die(f"refusing to overwrite existing model directory: {final}")
    staging = Path(tempfile.mkdtemp(prefix=".et-en.", dir=model_root))
    os.chmod(staging, 0o700)
    try:
        for name in MODEL_FILES:
            target = staging / name
            source = cache / name
            fd = os.open(target, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "wb") as output, open(source, "rb") as input_stream:
                for chunk in iter(lambda: input_stream.read(1024 * 1024), b""):
                    output.write(chunk)
                output.flush()
                os.fsync(output.fileno())
        os.rename(staging, final)
    except Exception:
        if staging.exists():
            for child in staging.iterdir():
                child.unlink()
            staging.rmdir()
        raise
    report = {
        "schemaVersion": 1,
        "action": "prepare-models",
        "source": MODEL_SOURCE,
        "target": MODEL_TARGET,
        "modelVersion": model.get("version"),
        "repository": str(EXPECTED_REPO),
        "manifest": str(EXPECTED_MANIFEST),
        "modelDirectory": str(final),
        "artifacts": {name: {"path": str(final / name), **sha256(final / name)} for name in MODEL_FILES},
        "cache": cache_records,
        "status": "prepared",
    }
    report_path = context["root"] / "translation-model-preparation.json"
    if report_path.exists():
        die("model preparation report already exists; refusing a second preparation")
    atomic_write_exclusive(report_path, report)
    print(json.dumps(report, indent=2))


def assert_initial_mailbox(context):
    launch = context["launch"]
    ssl_context = ssl.create_default_context(cafile=str(context["ca"]))
    with imaplib.IMAP4_SSL("127.0.0.1", launch["imapPort"], ssl_context=ssl_context, timeout=10) as client:
        client.login(USERNAME, PASSWORD)
        status, _ = client.select("INBOX", readonly=True)
        if status != "OK":
            die("could not select the owned fictional INBOX")
        status, data = client.uid("search", None, "ALL")
        if status != "OK":
            die("could not inspect the owned fictional INBOX")
        uids = [int(value) for value in data[0].split()] if data and data[0] else []
        if uids != [1, 2, 3]:
            die(f"arrival guard requires the original three UIDs [1, 2, 3], found {uids}")
        status, existing = client.uid("search", None, "HEADER", "Message-ID", MESSAGE_ID)
        if status != "OK" or (existing and existing[0].strip()):
            die("fixed translation arrival Message-ID already exists; refusing to resend")


def arrival_observed(context):
    launch = context["launch"]
    ssl_context = ssl.create_default_context(cafile=str(context["ca"]))
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        with imaplib.IMAP4_SSL("127.0.0.1", launch["imapPort"], ssl_context=ssl_context, timeout=10) as client:
            client.login(USERNAME, PASSWORD)
            status, _ = client.select("INBOX", readonly=True)
            if status != "OK":
                die("could not select the owned fictional INBOX after SMTP acceptance")
            status, data = client.uid("search", None, "ALL")
            uids = [int(value) for value in data[0].split()] if status == "OK" and data and data[0] else []
            status, matching = client.uid("search", None, "HEADER", "Message-ID", MESSAGE_ID)
            if status == "OK" and uids == [1, 2, 3, 4] and matching and matching[0].strip():
                return {"uids": uids, "messageIdObserved": True}
        time.sleep(0.5)
    return {"uids": None, "messageIdObserved": False}


def update_journal(path, payload):
    temporary = path.with_name(f".{path.name}.{uuid.uuid4().hex}.tmp")
    atomic_write_exclusive(temporary, payload)
    os.replace(temporary, path)


def submit_arrival(context):
    model_dir = context["profile"] / "translation-models" / "et-en"
    if not model_dir.is_dir() or any(not (model_dir / name).is_file() for name in MODEL_FILES):
        die("verified Estonian model must be prepared before the arrival")
    assert_initial_mailbox(context)
    journal_path = context["root"] / JOURNAL_NAME
    if journal_path.exists():
        die("arrival journal already exists; refusing any retry")
    planned = {
        "schemaVersion": 1,
        "status": "planned",
        "messageId": MESSAGE_ID,
        "recipient": RECIPIENT,
        "createdAt": time.time(),
        "transport": {"host": "127.0.0.1", "smtpPort": context["launch"]["smtpPort"], "imapPort": context["launch"]["imapPort"]},
    }
    atomic_write_exclusive(journal_path, planned)

    http_port = context["plan"]["listeners"]["http"]["port"]
    body = "Tere, see on turvaline fiktiivne kiri. Palun kinnita, et Dakia tõlgib selle sõnumi kohapeal ilma sisu võrku saatmata."
    html = f'<p>{body}</p><img src="http://127.0.0.1:{http_port}/remote-image.svg?via=translation-arrival" alt="remote content remains blocked">'
    message = email.message.EmailMessage()
    message["From"] = "Fictional Provider <provider@example.test>"
    message["To"] = RECIPIENT
    message["Subject"] = "Dakia fictional Estonian translation arrival"
    message["Message-ID"] = MESSAGE_ID
    message["X-Dakia-Fictional-Fixture"] = "estonian-translation-arrival"
    message.set_content(body)
    message.add_alternative(html, subtype="html")
    try:
        ssl_context = ssl.create_default_context(cafile=str(context["ca"]))
        with smtplib.SMTP_SSL("127.0.0.1", context["launch"]["smtpPort"], context=ssl_context, timeout=10) as client:
            client.login(USERNAME, PASSWORD)
            response = client.send_message(message)
        arrival = arrival_observed(context)
        if not arrival["messageIdObserved"]:
            update_journal(path=journal_path, payload={**planned, "status": "accepted-no-arrival-confirmation", "smtpResponse": repr(response), "arrival": arrival, "recordedAt": time.time()})
            die("SMTP accepted the message but UID4 was not observed; no retry was attempted")
        update_journal(path=journal_path, payload={**planned, "status": "accepted", "smtpResponse": repr(response), "arrival": arrival, "acceptedAt": time.time()})
        print(json.dumps({"status": "accepted", "messageId": MESSAGE_ID, "arrival": arrival, "journal": str(journal_path)}, indent=2))
    except Exception as error:
        update_journal(path=journal_path, payload={**planned, "status": "possibly-submitted", "errorType": type(error).__name__, "error": str(error), "recordedAt": time.time()})
        die(f"SMTP outcome is possibly submitted; no retry was attempted: {type(error).__name__}: {error}")


def main():
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="action", required=True)
    for name in ("prepare-models", "submit-arrival"):
        sub = subparsers.add_parser(name)
        sub.add_argument("--root", required=True)
    prepare = subparsers.choices["prepare-models"]
    args = parser.parse_args()
    context = guard_root(args.root)
    if args.action == "prepare-models":
        prepare_models(context)
    else:
        submit_arrival(context)


if __name__ == "__main__":
    main()
