import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { gunzipSync } from "node:zlib";
import { createHash } from "node:crypto";
import { verifyLargeMailboxAudit } from "./verify-large-mailbox-imap-audit.mjs";

export function validAudit() {
  const sync = [];
  const push = (command, result) => {
    sync.push({ kind: "command", command });
    if (result) sync.push(result);
  };
  push("CAPABILITY");
  push("LOGIN", { kind: "login" });
  push("CAPABILITY");
  push("LIST");
  push("SELECT", { kind: "select" });
  for (let page = 0; page < 60; page += 1)
    push("FETCH", {
      kind: "snapshot",
      start: 29501 - page * 500,
      end: 30000 - page * 500,
    });
  for (let batch = 0; batch < 600; batch += 1) {
    push("SELECT", { kind: "select" });
    for (const kind of ["metadata", "snippet"])
      for (let half = 0; half < 2; half += 1)
        push("UID FETCH", {
          kind,
          uids: Array.from(
            { length: 25 },
            (_, index) => batch * 50 + half * 25 + index + 1,
          ),
        });
  }
  push("SELECT", { kind: "select" });
  push("LOGOUT");
  const reader = [
    { kind: "command", command: "CAPABILITY" },
    { kind: "command", command: "LOGIN" },
    { kind: "login" },
    { kind: "command", command: "SELECT" },
    { kind: "select" },
    { kind: "command", command: "UID FETCH" },
    { kind: "metadata", uids: [2] },
    { kind: "command", command: "UID FETCH" },
    { kind: "section", uid: 2, section: "1.MIME" },
    { kind: "command", command: "UID FETCH" },
    { kind: "section", uid: 2, section: "1" },
    { kind: "command", command: "LOGOUT" },
  ];
  return {
    schemaVersion: 2,
    fixture: "fictional-30000",
    sessions: [
      { tlsProtocol: "TLSv1.3", events: sync },
      { tlsProtocol: "TLSv1.2", events: reader },
    ],
  };
}

test("requires complete separate sync and Reader TLS ingress/result contracts", () => {
  const result = verifyLargeMailboxAudit(validAudit());
  assert.equal(result.snapshotPages, 60);
  assert.equal(result.metadataBatches, 1200);
  assert.equal(result.snippetBatches, 1200);
});
for (const family of ["metadata", "snippet"])
  for (const variant of ["duplicate", "missing", "out-of-range", "oversized"])
    test(`rejects ${family} ${variant} coverage`, () => {
      const audit = validAudit();
      const event = audit.sessions[0].events.find((row) => row.kind === family);
      if (variant === "duplicate") event.uids[1] = event.uids[0];
      if (variant === "missing") event.uids.pop();
      if (variant === "out-of-range") event.uids[0] = 30001;
      if (variant === "oversized") event.uids.push(30000);
      assert.throws(() => verifyLargeMailboxAudit(audit));
    });
const corruptions = {
  "one session": (a) => a.sessions.pop(),
  "third session": (a) => a.sessions.push(a.sessions[1]),
  "unnegotiated TLS": (a) => (a.sessions[0].tlsProtocol = null),
  "old TLS": (a) => (a.sessions[1].tlsProtocol = "TLSv1.1"),
  "wrong page order": (a) =>
    (a.sessions[0].events.find((e) => e.kind === "snapshot").start = 1),
  "extra snapshot": (a) =>
    a.sessions[0].events.push({ kind: "snapshot", start: 1, end: 500 }),
  "UID mutation": (a) =>
    a.sessions[0].events.push({ kind: "command", command: "UID STORE" }),
  "rejected unknown": (a) =>
    a.sessions[1].events.push({ kind: "rejected", command: "NOOP" }),
  "extra allowed command": (a) =>
    a.sessions[1].events.unshift({ kind: "command", command: "CAPABILITY" }),
  "missing Reader metadata": (a) =>
    (a.sessions[1].events = a.sessions[1].events.filter(
      (e) => e.kind !== "metadata",
    )),
  "wrong Reader UID": (a) =>
    (a.sessions[1].events.find((e) => e.kind === "section").uid = 3),
  "extra Reader section": (a) =>
    a.sessions[1].events.push({ kind: "section", uid: 2, section: "1" }),
  "missing logout": (a) => a.sessions[0].events.pop(),
  "missing LIST": (a) =>
    (a.sessions[0].events = a.sessions[0].events.filter(
      (e) => e.command !== "LIST",
    )),
  "wrong result ordering": (a) => a.sessions[0].events.reverse(),
};
for (const [name, change] of Object.entries(corruptions))
  test(`rejects ${name}`, () => {
    const audit = validAudit();
    change(audit);
    assert.throws(() => verifyLargeMailboxAudit(audit));
  });

test("accepts the exact failed-run transcript after correcting the capability oracle", async () => {
  const root = new URL("./testdata/large-mailbox/", import.meta.url);
  const provenance = JSON.parse(
    await readFile(new URL("provenance.json", root), "utf8"),
  );
  for (const [name, record] of Object.entries(provenance.files)) {
    const bytes = await readFile(new URL(name, root));
    assert.equal(
      createHash("sha256").update(bytes).digest("hex"),
      record.sha256,
      name,
    );
    assert.equal(bytes.length, record.bytes, name);
  }
  const compressed = await readFile(new URL("imap-audit.json.gz", root));
  const raw = gunzipSync(compressed);
  assert.equal(
    createHash("sha256").update(raw).digest("hex"),
    provenance.files["imap-audit.json.gz"].uncompressedSha256,
  );
  assert.equal(
    raw.length,
    provenance.files["imap-audit.json.gz"].uncompressedBytes,
  );
  const audit = JSON.parse(raw);
  assert.deepEqual(
    audit.sessions[0].events.slice(0, 5).map((e) => e.command ?? e.kind),
    ["CAPABILITY", "LOGIN", "login", "CAPABILITY", "LIST"],
  );
  assert.deepEqual(
    audit.sessions[1].events.slice(0, 4).map((e) => e.command ?? e.kind),
    ["CAPABILITY", "LOGIN", "login", "SELECT"],
  );
  const result = verifyLargeMailboxAudit(audit);
  assert.equal(result.metadataBatches, 1200);
  assert.equal(result.snippetBatches, 1200);
  for (const role of [0, 1]) {
    const absent = structuredClone(audit);
    absent.sessions[role].events.shift();
    assert.throws(() => verifyLargeMailboxAudit(absent));
    const extra = structuredClone(audit);
    extra.sessions[role].events.unshift({
      kind: "command",
      command: "CAPABILITY",
    });
    assert.throws(() => verifyLargeMailboxAudit(extra));
  }
  const moved = structuredClone(audit);
  const preauth = moved.sessions[0].events.shift();
  moved.sessions[0].events.splice(2, 0, preauth);
  assert.throws(() => verifyLargeMailboxAudit(moved), /sequence/);
});
