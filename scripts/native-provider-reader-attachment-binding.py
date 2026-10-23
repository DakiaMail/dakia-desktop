#!/usr/bin/env python3
"""Create a pinned fictional UID3 attachment-interruption binding from real IMAP bytes."""
from __future__ import annotations

import argparse
import base64
import email.policy
import hashlib
import imaplib
import json
import os
import re
import ssl
import stat
import sys
from email.parser import BytesParser
from pathlib import Path

SENTINEL = b"dakia-native-mail-safety-fixture-v1\n"
ACCOUNT_ID = "62a9320c-04f5-47c2-b4fa-d4952f0b6121"
EMAIL = "fictional-reader@localhost"
COLLISION_EMAIL = "fictional-reader-two@localhost"
COLLISION_SUBJECT = "Account two collision safety check"
COLLISION_BODY = "Only account two owns this distinct cached body and attachment."
COLLISION_ATTACHMENT_NAME = "account-two-notes.txt"
COLLISION_ATTACHMENT_BYTES = b"fictional account two attachment bytes\n"
COLLISION_ATTACHMENT_SHA256 = "8d75a6c97173c8be55b14daccce4f7cc4cfb2821ce13d3cb98fe2b2b28796541"
MAILBOX = "INBOX"
UID = 3
EXPECTED_UIDNEXT = 4
MESSAGE_ID = "<fictional-provider-reader-acceptance@localhost>"
SUBJECT = "Remote content safety check"
SAFE_NAME = "meeting-notes.txt"
SAFE_BYTES = b"fictional safe attachment bytes\n"
SAFE_SHA256 = "666dec5256feac267cd6c46b97107397c0a87fcde80a932321819f5a1cf2955e"
UNSAFE_NAME = "../../quarterly-report.pdf.command"
UNSAFE_BYTES = b"fictional attachment bytes\x00\xff"
UNSAFE_SHA256 = "1679a56fbd964b0463ec644f56fa9a3189d1402343de932a66b210586d00c02f"
PREFIX_LENGTH = 12
MAX_RAW_BYTES = 1024 * 1024


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def owned_root(value: str) -> Path:
    supplied = Path(value).absolute()
    root = supplied.resolve(strict=True)
    info = supplied.lstat()
    require(root == supplied and stat.S_ISDIR(info.st_mode) and not supplied.is_symlink(), "run root is not canonical")
    require(info.st_uid == os.getuid() and stat.S_IMODE(info.st_mode) == 0o700, "run root is not private and owned")
    sentinel = root / ".dakia-native-mail-safety-root"
    sinfo = sentinel.lstat()
    require(stat.S_ISREG(sinfo.st_mode) and not sentinel.is_symlink() and sinfo.st_uid == os.getuid(), "run root sentinel is unsafe")
    require(sentinel.read_bytes() == SENTINEL, "run root sentinel is invalid")
    return root


def exclusive_write(path: Path, data: bytes) -> None:
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        with os.fdopen(fd, "wb", closefd=False) as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())
    finally:
        os.close(fd)
    os.chmod(path, 0o600)


def private_owned_file(path: Path, root: Path, label: str) -> None:
    info = path.lstat()
    require(
        root in path.parents
        and stat.S_ISREG(info.st_mode)
        and not path.is_symlink()
        and info.st_uid == os.getuid()
        and stat.S_IMODE(info.st_mode) == 0o600,
        f"{label} is not a private owned run-root file",
    )


def response_number(data, key: str) -> int:
    require(data and isinstance(data[-1], bytes), f"missing {key} response")
    match = re.fullmatch(rb"\d+", data[-1].strip())
    require(match is not None, f"invalid {key} response")
    return int(match.group(0))


def status_epoch(data) -> tuple[int, int]:
    require(data and len(data) == 1 and isinstance(data[0], bytes), "invalid STATUS response")
    uidv = re.search(rb"\bUIDVALIDITY\s+(\d+)\b", data[0], re.I)
    uidn = re.search(rb"\bUIDNEXT\s+(\d+)\b", data[0], re.I)
    require(uidv is not None and uidn is not None, "STATUS omitted UIDVALIDITY or UIDNEXT")
    return int(uidv.group(1)), int(uidn.group(1))


def raw_fetch(data) -> bytes:
    literals = [item for item in data or [] if isinstance(item, tuple) and len(item) == 2 and isinstance(item[0], bytes) and isinstance(item[1], bytes)]
    require(len(literals) == 1, "UID3 full fetch did not return exactly one literal")
    header, raw = literals[0]
    require(re.search(rb"\bUID\s+3\b", header, re.I) is not None, "full fetch returned the wrong UID")
    body = re.search(rb"\bBODY\[\]\s*\{(\d+)\}\s*$", header, re.I)
    require(body is not None, "full fetch returned the wrong body section")
    flags = re.search(rb"\bFLAGS\s*\(([^)]*)\)", header, re.I)
    require(flags is not None and br"\seen" not in flags.group(1).lower(), "full fetch was not read-neutral")
    require(int(body.group(1)) == len(raw), "UID3 raw literal length is inconsistent")
    require(len(raw) <= MAX_RAW_BYTES, "UID3 raw message exceeds the bounded fixture limit")
    return raw


def unread_uid3_flags(rows) -> bool:
    matches = []
    for row in rows or []:
        if not isinstance(row, bytes):
            continue
        uid = re.search(rb"\bUID\s+(\d+)\b", row, re.I)
        if uid is None or int(uid.group(1)) != UID:
            continue
        flags = re.search(rb"\bFLAGS\s*\(([^)]*)\)", row, re.I)
        require(flags is not None, "post-fetch UID3 response omitted FLAGS")
        matches.append(flags.group(1))
    require(len(matches) == 1, "post-fetch flags did not identify exactly one UID3 record")
    return br"\seen" not in matches[0].lower()


def walk_leaves(part, prefix: str = ""):
    if part.is_multipart():
        for index, child in enumerate(part.iter_parts(), 1):
            section = f"{prefix}.{index}" if prefix else str(index)
            yield from walk_leaves(child, section)
    else:
        yield prefix or "1", part


def inspect_raw(raw: bytes, *, account_id: str, uid_validity: int) -> dict:
    require(sha256(SAFE_BYTES) == SAFE_SHA256 and sha256(UNSAFE_BYTES) == UNSAFE_SHA256, "compiled fixture byte hashes changed")
    message = BytesParser(policy=email.policy.default).parsebytes(raw)
    require(str(message.get("Message-ID", "")).strip() == MESSAGE_ID, "UID3 Message-ID changed")
    require(str(message.get("Subject", "")).strip() == SUBJECT, "UID3 subject changed")
    attachments = []
    for section, part in walk_leaves(message):
        filename = part.get_filename()
        disposition = part.get_content_disposition()
        if filename is None and disposition != "attachment":
            continue
        payload = part.get_payload(decode=True)
        require(isinstance(payload, bytes), f"attachment {section} did not decode to bytes")
        require(str(part.get("Content-Transfer-Encoding", "")).lower() == "base64", f"attachment {section} is not base64")
        encoded = base64.b64encode(payload)
        offsets = [match.start() for match in re.finditer(re.escape(encoded), raw)]
        require(len(offsets) == 1, f"attachment {section} encoded bytes were not unique in raw MIME")
        attachments.append({"section": section, "filename": filename, "decoded": payload, "decodedSha256": sha256(payload), "encoded": encoded, "rawOffset": offsets[0]})
    require(len(attachments) == 2, "UID3 must have exactly two attachment candidates")
    unsafe = next((item for item in attachments if item["filename"] == UNSAFE_NAME), None)
    safe = next((item for item in attachments if item["filename"] == SAFE_NAME), None)
    require(unsafe is not None and safe is not None, "UID3 safe or unsafe attachment is missing")
    require(unsafe["section"] == "2" and unsafe["decoded"] == UNSAFE_BYTES and unsafe["decodedSha256"] == UNSAFE_SHA256, "UID3 unsafe attachment identity changed")
    require(safe["section"] == "3" and safe["decoded"] == SAFE_BYTES and safe["decodedSha256"] == SAFE_SHA256, "UID3 safe attachment identity changed")
    prefix = safe["encoded"][:PREFIX_LENGTH]
    require(len(prefix) == PREFIX_LENGTH and PREFIX_LENGTH < len(safe["encoded"]), "safe attachment interruption prefix is not partial")
    return {
        "schemaVersion": 1,
        "accountId": account_id,
        "email": EMAIL,
        "mailbox": MAILBOX,
        "uid": UID,
        "uidValidity": uid_validity,
        "messageId": MESSAGE_ID,
        "subject": SUBJECT,
        "rawSha256": sha256(raw),
        "rawBytes": len(raw),
        "fixtureProvenance": {
            "kind": "generated-fictional",
            "source": "scripts/native-provider-reader-acceptance.sh",
        },
        "attachment": {
            "id": f"{account_id}:INBOX:{UID}:mime-v1:{safe['section']}",
            "filename": SAFE_NAME,
            "section": safe["section"],
            "rawOffset": safe["rawOffset"],
            "wirePrefixLength": PREFIX_LENGTH,
            "wirePrefixSha256": sha256(prefix),
            "decodedBytes": len(safe["decoded"]),
            "decodedSha256": safe["decodedSha256"],
        },
        "unsafeAttachment": {
            "id": f"{account_id}:INBOX:{UID}:mime-v1:{unsafe['section']}",
            "filename": UNSAFE_NAME,
            "section": unsafe["section"],
            "decodedBytes": len(unsafe["decoded"]),
            "decodedSha256": unsafe["decodedSha256"],
        },
    }


def inspect_collision_raw(raw: bytes) -> dict:
    message = BytesParser(policy=email.policy.default).parsebytes(raw)
    require(str(message.get("Message-ID", "")).strip() == MESSAGE_ID, "collision UID3 Message-ID changed")
    require(str(message.get("Subject", "")).strip() == COLLISION_SUBJECT, "collision UID3 subject changed")
    leaves = list(walk_leaves(message))
    require(len(leaves) == 2, "collision UID3 MIME topology changed")
    body_section, body_part = leaves[0]
    attachment_section, attachment_part = leaves[1]
    body = body_part.get_content()
    attachment = attachment_part.get_payload(decode=True)
    require(body_section == "1" and isinstance(body, str) and body.strip() == COLLISION_BODY, "collision UID3 distinct body changed")
    require(
        attachment_section == "2"
        and attachment_part.get_filename() == COLLISION_ATTACHMENT_NAME
        and attachment == COLLISION_ATTACHMENT_BYTES
        and sha256(attachment) == COLLISION_ATTACHMENT_SHA256,
        "collision UID3 distinct attachment changed",
    )
    return {
        "email": COLLISION_EMAIL,
        "uid": UID,
        "messageId": MESSAGE_ID,
        "subject": COLLISION_SUBJECT,
        "rawSha256": sha256(raw),
        "attachmentSection": attachment_section,
        "attachmentFilename": COLLISION_ATTACHMENT_NAME,
        "attachmentSha256": COLLISION_ATTACHMENT_SHA256,
    }


def checked(status, data, label: str):
    require(status == "OK", f"IMAP {label} failed")
    return data


def build_binding(client, *, account_id: str) -> tuple[bytes, dict]:
    status, count = client.select(MAILBOX, readonly=True)
    checked(status, count, "EXAMINE")
    require(count == [b"3"], "account A INBOX does not contain exactly three messages")
    selected_uidv = response_number(client.response("UIDVALIDITY")[1], "UIDVALIDITY")
    status, data = client.status(MAILBOX, "(UIDVALIDITY UIDNEXT)")
    epoch_uidv, uid_next = status_epoch(checked(status, data, "STATUS"))
    require(selected_uidv == epoch_uidv and uid_next == EXPECTED_UIDNEXT, "mailbox epoch or UIDNEXT changed")
    status, data = client.uid("SEARCH", None, "HEADER", "Message-ID", MESSAGE_ID)
    search = checked(status, data, "UID SEARCH")
    require(search == [b"3"], "Message-ID was absent, duplicated, or routed to the wrong UID")
    status, data = client.uid("FETCH", str(UID), "(UID FLAGS INTERNALDATE BODY.PEEK[])")
    raw = raw_fetch(checked(status, data, "UID FETCH BODY.PEEK[]"))
    status, data = client.uid("FETCH", str(UID), "(UID FLAGS)")
    flags_rows = checked(status, data, "post-fetch UID FLAGS")
    require(unread_uid3_flags(flags_rows), "post-fetch flags did not prove UID3 remained unread")
    return raw, inspect_raw(raw, account_id=account_id, uid_validity=selected_uidv)


def verify_collision_account(client) -> dict:
    status, count = client.select(MAILBOX, readonly=True)
    checked(status, count, "collision EXAMINE")
    require(count == [b"3"], "collision account INBOX does not contain exactly three messages")
    status, data = client.uid("SEARCH", None, "HEADER", "Message-ID", MESSAGE_ID)
    require(checked(status, data, "collision UID SEARCH") == [b"3"], "collision Message-ID is absent or duplicated")
    status, data = client.uid("FETCH", str(UID), "(UID FLAGS INTERNALDATE BODY.PEEK[])")
    raw = raw_fetch(checked(status, data, "collision UID FETCH BODY.PEEK[]"))
    status, data = client.uid("FETCH", str(UID), "(UID FLAGS)")
    require(unread_uid3_flags(checked(status, data, "collision UID FLAGS")), "collision UID3 is not unread")
    return inspect_collision_raw(raw)


def main() -> None:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--root", required=True)
    parser.add_argument("--host", required=True)
    parser.add_argument("--port", required=True, type=int)
    parser.add_argument("--ca", required=True)
    parser.add_argument("--account-id", required=True)
    parser.add_argument("--email", required=True)
    parser.add_argument("--collision-email", required=True)
    parser.add_argument("--fixture-raw", required=True)
    parser.add_argument("--binding-artifact", required=True)
    args = parser.parse_args()
    root = owned_root(args.root)
    require(args.host == "127.0.0.1" and 0 < args.port < 65536, "preflight endpoint must be loopback")
    require(args.account_id == ACCOUNT_ID and args.email == EMAIL and args.collision_email == COLLISION_EMAIL, "preflight account route changed")
    ca = Path(args.ca).absolute()
    require(ca.resolve(strict=True) == ca, "preflight CA path is not canonical")
    private_owned_file(ca, root, "preflight CA")
    raw_path = Path(args.fixture_raw).absolute()
    artifact_path = Path(args.binding_artifact).absolute()
    require(raw_path.parent == root and artifact_path.parent == root, "binding outputs must be direct run-root files")
    password = sys.stdin.buffer.readline().rstrip(b"\r\n")
    require(password == b"synthetic-only-password" and sys.stdin.buffer.read() == b"", "fictional password input is invalid")
    context = ssl.create_default_context(cafile=str(ca))
    context.minimum_version = ssl.TLSVersion.TLSv1_2
    with imaplib.IMAP4_SSL(args.host, args.port, ssl_context=context, timeout=10) as client:
        checked(*client.login(args.email, password.decode("ascii")), "LOGIN")
        raw, artifact = build_binding(client, account_id=args.account_id)
        logout_status, _ = client.logout()
        require(logout_status in ("BYE", "OK"), "IMAP LOGOUT failed")
    with imaplib.IMAP4_SSL(args.host, args.port, ssl_context=context, timeout=10) as client:
        checked(*client.login(args.collision_email, password.decode("ascii")), "collision LOGIN")
        artifact["collisionAccount"] = verify_collision_account(client)
        logout_status, _ = client.logout()
        require(logout_status in ("BYE", "OK"), "collision IMAP LOGOUT failed")
    exclusive_write(raw_path, raw)
    artifact_bytes = (json.dumps(artifact, sort_keys=True, separators=(",", ":")) + "\n").encode()
    exclusive_write(artifact_path, artifact_bytes)
    print(json.dumps({"status": "prepared", "bindingArtifactSha256": sha256(artifact_bytes), "fixtureRawSha256": sha256(raw), "uid": UID, "uidValidity": artifact["uidValidity"], "attachmentSection": artifact["attachment"]["section"], "attachmentId": artifact["attachment"]["id"]}, separators=(",", ":")))


if __name__ == "__main__":
    main()
