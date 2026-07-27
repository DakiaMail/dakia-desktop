#!/usr/bin/env python3

import hashlib
import json
import sqlite3
import sys
from pathlib import Path

ACCOUNT_ID = "11111111-1111-4111-8111-111111111111"
ACCOUNT_EMAIL = "updater-fixture@example.test"
ACCOUNT = {
    "id": ACCOUNT_ID,
    "email": ACCOUNT_EMAIL,
    "account_name": "Updater acceptance fixture",
    "display_name": "Updater Fixture",
    "provider_id": "migadu",
    "auth": {"type": "password", "username": ACCOUNT_EMAIL},
    "imap_host": "127.0.0.1",
    "imap_port": 993,
    "imap_security": "tls",
    "smtp_host": "127.0.0.1",
    "smtp_port": 465,
    "smtp_security": "tls",
    "archive_mailbox": "Archive",
    "spam_mailbox": "Junk",
    "enabled": False,
    "created_at": "2026-07-25T00:00:00Z",
}

MESSAGE_COLUMNS = (
    "id, account_id, mailbox, uid, message_id, in_reply_to, reference_ids, "
    "thread_id, threading_scanned, subject, from_name, from_address, "
    "to_addresses, received_at, snippet, body_text, body_html, content_state, "
    "unsubscribe_kind, unsubscribe_url, unsubscribe_scanned, is_read, "
    "is_flagged, has_attachments, category, classification_confidence, "
    "classification_source, classification_signals"
)


def connect(database: Path) -> sqlite3.Connection:
    if not database.is_file():
        raise SystemExit(f"Updater acceptance database is missing: {database}")
    connection = sqlite3.connect(database)
    connection.execute("CREATE VIRTUAL TABLE temp.fts5_probe USING fts5(value)")
    connection.execute("DROP TABLE temp.fts5_probe")
    return connection


def seed(database: Path) -> None:
    connection = connect(database)
    with connection:
        connection.execute(
            """
            INSERT INTO accounts(id, email, data, created_at)
            VALUES (?, ?, ?, ?)
            """,
            (
                ACCOUNT_ID,
                ACCOUNT_EMAIL,
                json.dumps(ACCOUNT, separators=(",", ":")),
                "2026-07-25T00:00:00Z",
            ),
        )
        statement = f"""
            INSERT INTO messages({MESSAGE_COLUMNS})
            VALUES ({",".join("?" for _ in range(28))})
        """
        connection.executemany(
            statement,
            [
                (
                    "fixture-message-1",
                    ACCOUNT_ID,
                    "INBOX",
                    1,
                    "<fixture-1@example.test>",
                    None,
                    "[]",
                    "fixture-thread-1",
                    1,
                    "Updater acceptance message one",
                    "Release Bot",
                    "release-bot@example.test",
                    f'["{ACCOUNT_EMAIL}"]',
                    "2026-07-25T10:00:00Z",
                    "This message must survive the update.",
                    "This message must survive the update.",
                    "<p>This message must survive the update.</p>",
                    "complete",
                    None,
                    None,
                    1,
                    0,
                    1,
                    0,
                    "updates",
                    1.0,
                    "user",
                    "acceptance-fixture",
                ),
                (
                    "fixture-message-2",
                    ACCOUNT_ID,
                    "Archive",
                    2,
                    "<fixture-2@example.test>",
                    None,
                    "[]",
                    "fixture-thread-2",
                    1,
                    "Updater acceptance message two",
                    "Release Bot",
                    "release-bot@example.test",
                    f'["{ACCOUNT_EMAIL}"]',
                    "2026-07-25T11:00:00Z",
                    "Archived local data must also survive.",
                    "Archived local data must also survive.",
                    "<p>Archived local data must also survive.</p>",
                    "complete",
                    None,
                    None,
                    1,
                    1,
                    0,
                    0,
                    "updates",
                    1.0,
                    "user",
                    "acceptance-fixture",
                ),
            ],
        )
    connection.close()
    print(f"Seeded updater acceptance profile: {database.parent}")


def serializable(value):
    if isinstance(value, bytes):
        return {"bytes_hex": value.hex()}
    return value


def snapshot(database: Path) -> None:
    connection = connect(database)
    tables = {
        "accounts": "SELECT id, email, data, created_at FROM accounts ORDER BY id",
        "messages": f"SELECT {MESSAGE_COLUMNS} FROM messages ORDER BY id",
        "attachments": (
            "SELECT id, message_id, filename, mime_type, size_bytes, is_inline, "
            "is_potentially_unsafe, data FROM attachments ORDER BY id"
        ),
        "starred_message_bodies": (
            "SELECT message_id, body_text, body_html, cached_at "
            "FROM starred_message_bodies ORDER BY message_id"
        ),
        "starred_attachment_metadata": (
            "SELECT id, message_id, filename, mime_type, size_bytes, is_inline, "
            "is_potentially_unsafe FROM starred_attachment_metadata ORDER BY id"
        ),
    }
    payload = {}
    for table, query in tables.items():
        cursor = connection.execute(query)
        columns = [column[0] for column in cursor.description]
        payload[table] = [
            {
                column: serializable(value)
                for column, value in zip(columns, row, strict=True)
            }
            for row in cursor.fetchall()
        ]
    connection.close()
    encoded = json.dumps(
        payload, sort_keys=True, separators=(",", ":"), ensure_ascii=True
    ).encode()
    print(
        f"accounts={len(payload['accounts'])} "
        f"messages={len(payload['messages'])} "
        f"sha256={hashlib.sha256(encoded).hexdigest()}"
    )


def main() -> None:
    if len(sys.argv) != 3 or sys.argv[1] not in {"seed", "snapshot"}:
        raise SystemExit(
            "Usage: updater-acceptance-profile.py <seed|snapshot> /path/to/data-dir"
        )
    database = Path(sys.argv[2]) / "dakia.db"
    if sys.argv[1] == "seed":
        seed(database)
    else:
        snapshot(database)


if __name__ == "__main__":
    main()
