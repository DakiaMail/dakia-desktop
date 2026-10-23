import { readFile } from "node:fs/promises";
import { pathToFileURL } from "node:url";

const ALLOWED = new Set([
  "CAPABILITY",
  "LOGIN",
  "LIST",
  "SELECT",
  "FETCH",
  "UID FETCH",
  "LOGOUT",
]);
function fail(message) {
  throw new Error(message);
}
function partition(events, kind) {
  const batches = events.filter((event) => event.kind === kind);
  const seen = new Set();
  for (const event of batches) {
    if (
      !Array.isArray(event.uids) ||
      !event.uids.length ||
      event.uids.length > 25
    )
      fail(`invalid ${kind} batch`);
    for (const uid of event.uids) {
      if (!Number.isInteger(uid) || uid < 1 || uid > 30000 || seen.has(uid))
        fail(`duplicate or invalid ${kind} UID`);
      seen.add(uid);
    }
  }
  if (seen.size !== 30000) fail(`incomplete ${kind} UID coverage`);
  return batches.length;
}
function sessionEvents(session) {
  if (
    !session ||
    !["TLSv1.2", "TLSv1.3"].includes(session.tlsProtocol) ||
    !Array.isArray(session.events)
  )
    fail("missing negotiated TLS session evidence");
  const events = session.events;
  for (const event of events) {
    if (
      !event ||
      ![
        "command",
        "login",
        "select",
        "snapshot",
        "metadata",
        "snippet",
        "section",
      ].includes(event.kind)
    )
      fail("rejected or unsupported IMAP event");
    if (event.kind === "command" && !ALLOWED.has(event.command))
      fail(`unsupported or mutating IMAP command: ${event.command}`);
  }
  const commands = events
    .filter((event) => event.kind === "command")
    .map((event) => event.command);
  if (
    commands.filter((command) => command === "LOGIN").length !== 1 ||
    events.filter((event) => event.kind === "login").length !== 1 ||
    commands.filter((command) => command === "LOGOUT").length !== 1 ||
    commands.at(-1) !== "LOGOUT"
  )
    fail("missing exact authentication/logout evidence");
  if (commands.indexOf("LOGIN") > commands.indexOf("SELECT"))
    fail("selection preceded authentication");
  if (
    commands.filter((command) => command === "SELECT").length !==
    events.filter((event) => event.kind === "select").length
  )
    fail("selection ingress differs from results");
  return { events, commands };
}

export function verifyLargeMailboxAudit(audit) {
  if (
    !audit ||
    audit.schemaVersion !== 2 ||
    audit.fixture !== "fictional-30000" ||
    !Array.isArray(audit.sessions) ||
    audit.sessions.length !== 2
  )
    fail("expected exactly two fictional TLS sessions");
  const parsed = audit.sessions.map(sessionEvents);
  const sync = parsed.find((session) =>
    session.events.some((event) => event.kind === "snapshot"),
  );
  const reader = parsed.find((session) => session !== sync);
  if (!sync || !reader) fail("missing distinct sync and Reader sessions");
  const pages = sync.events.filter((event) => event.kind === "snapshot");
  if (pages.length !== 60) fail("expected exactly sixty snapshot pages");
  for (let index = 0; index < 60; index += 1) {
    const end = 30000 - index * 500;
    if (pages[index].start !== end - 499 || pages[index].end !== end)
      fail("snapshot pages are not exact descending 500-UID windows");
  }
  const metadataBatches = partition(sync.events, "metadata");
  const snippetBatches = partition(sync.events, "snippet");
  if (sync.events.some((event) => event.kind === "section"))
    fail("unexpected Reader section in sync session");
  if (
    sync.commands.filter((command) => command === "FETCH").length !== 60 ||
    sync.commands.filter((command) => command === "UID FETCH").length !==
      metadataBatches + snippetBatches
  )
    fail("sync ingress and results differ");
  const readerMetadata = reader.events.filter(
    (event) => event.kind === "metadata",
  );
  const sections = reader.events.filter((event) => event.kind === "section");
  if (
    readerMetadata.length !== 1 ||
    JSON.stringify(readerMetadata[0].uids) !== "[2]" ||
    sections.length !== 2 ||
    sections.some((event) => event.uid !== 2) ||
    sections
      .map((event) => event.section)
      .sort()
      .join(",") !== "1,1.MIME" ||
    reader.events.some((event) => ["snapshot", "snippet"].includes(event.kind))
  )
    fail("Reader UID 2 metadata/section/MIME proof differs");
  if (
    reader.commands.filter((command) => command === "UID FETCH").length !== 3 ||
    reader.commands.includes("FETCH")
  )
    fail("unexpected Reader ingress");
  for (const { commands } of parsed) {
    if (
      commands.filter((command) => command === "CAPABILITY").length !==
        (commands === sync.commands ? 2 : 1) ||
      commands.filter((command) => command === "LIST").length !==
        (commands === sync.commands ? 1 : 0)
    )
      fail("unexpected capability/LIST command count");
  }
  if (
    sync.commands.filter((command) => command === "SELECT").length !== 602 ||
    reader.commands.filter((command) => command === "SELECT").length !== 1
  )
    fail("unexpected SELECT command count");
  const expectedSync = [
    "command:CAPABILITY",
    "command:LOGIN",
    "login",
    "command:CAPABILITY",
    "command:LIST",
    "command:SELECT",
    "select",
  ];
  for (let page = 0; page < 60; page += 1)
    expectedSync.push("command:FETCH", "snapshot");
  for (let batch = 0; batch < 600; batch += 1)
    expectedSync.push(
      "command:SELECT",
      "select",
      "command:UID FETCH",
      "metadata",
      "command:UID FETCH",
      "metadata",
      "command:UID FETCH",
      "snippet",
      "command:UID FETCH",
      "snippet",
    );
  expectedSync.push("command:SELECT", "select", "command:LOGOUT");
  const expectedReader = [
    "command:CAPABILITY",
    "command:LOGIN",
    "login",
    "command:SELECT",
    "select",
    "command:UID FETCH",
    "metadata",
    "command:UID FETCH",
    "section",
    "command:UID FETCH",
    "section",
    "command:LOGOUT",
  ];
  const signature = (events) =>
    events.map((event) =>
      event.kind === "command" ? `command:${event.command}` : event.kind,
    );
  if (
    JSON.stringify(signature(sync.events)) !== JSON.stringify(expectedSync) ||
    JSON.stringify(signature(reader.events)) !==
      JSON.stringify(expectedReader) ||
    sections[0].section !== "1.MIME" ||
    sections[1].section !== "1"
  )
    fail(
      "IMAP command/result sequence differs from the source-derived contract",
    );
  return {
    sessions: 2,
    snapshotPages: 60,
    metadataBatches,
    snippetBatches,
    readerUid: 2,
    readerSections: sections.map((event) => event.section),
    tlsProtocols: audit.sessions.map((session) => session.tlsProtocol),
  };
}
if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  if (process.argv.length !== 3)
    fail("usage: verify-large-mailbox-imap-audit.mjs AUDIT.json");
  process.stdout.write(
    `${JSON.stringify(verifyLargeMailboxAudit(JSON.parse(await readFile(process.argv[2], "utf8"))))}\n`,
  );
}
