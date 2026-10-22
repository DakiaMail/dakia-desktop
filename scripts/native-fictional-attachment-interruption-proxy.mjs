#!/usr/bin/env node
// A private, fictional TLS IMAP overlay for native attachment-download fault
// acceptance. It is deliberately not a general IMAP proxy: a binding fixes
// one account/mailbox/UID/message and only one read-neutral body fetch may be
// interrupted after a control command is acknowledged.
import { createHash, randomBytes, randomUUID } from "node:crypto";
import {
  appendFileSync,
  chmodSync,
  existsSync,
  linkSync,
  lstatSync,
  readFileSync,
  realpathSync,
  renameSync,
  unlinkSync,
  writeFileSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { createServer, connect as tlsConnect } from "node:tls";

const ROOT_SENTINEL = "dakia-native-mail-safety-fixture-v1\n";
const COMMAND_FILE = "attachment-interruption-command.json";
const STATE_FILE = "attachment-interruption-state.json";
const METADATA_FILE = "attachment-interruption-overlay.json";
const STALL_LITERAL_BYTES = 44;
const STALL_FORWARD_BYTES = 12;
const DEFAULT_STALL_DEADLINE_MS = 30_000;

function must(condition, message) {
  if (!condition) throw new Error(message);
}

function sha256Text(value) {
  return /^[a-f0-9]{64}$/.test(value);
}

export function ownedRoot(rootPath) {
  const supplied = resolve(rootPath);
  const root = realpathSync(supplied);
  const details = lstatSync(supplied);
  must(
    supplied === root && details.isDirectory() && !details.isSymbolicLink(),
    "attachment interruption root must be a canonical directory",
  );
  if (typeof process.getuid === "function")
    must(
      details.uid === process.getuid(),
      "attachment interruption root is not owned by this user",
    );
  must(
    (details.mode & 0o777) === 0o700,
    "attachment interruption root mode must be 0700",
  );
  secureRegularFile(
    join(root, ".dakia-native-mail-safety-root"),
    root,
    "ownership sentinel",
  );
  must(
    readFileSync(join(root, ".dakia-native-mail-safety-root"), "utf8") ===
      ROOT_SENTINEL,
    "attachment interruption root lacks its exact ownership sentinel",
  );
  return root;
}

function secureRegularFile(path, root, description) {
  const absolute = resolve(path);
  must(
    absolute.startsWith(`${root}/`),
    `${description} must be inside the owned root`,
  );
  const canonical = realpathSync(absolute);
  must(
    canonical === absolute && canonical.startsWith(`${root}/`),
    `${description} must not traverse a symbolic link`,
  );
  const details = lstatSync(canonical);
  must(
    details.isFile() && !details.isSymbolicLink(),
    `${description} must be a regular file`,
  );
  if (typeof process.getuid === "function")
    must(
      details.uid === process.getuid(),
      `${description} is not owned by this user`,
    );
  must((details.mode & 0o777) === 0o600, `${description} mode must be 0600`);
  return canonical;
}

function secureRead(path, root, description) {
  return readFileSync(secureRegularFile(path, root, description));
}

export function validateBinding(value) {
  must(value && value.schemaVersion === 1, "binding schemaVersion must be 1");
  for (const field of [
    "accountId",
    "email",
    "mailbox",
    "messageId",
    "rawSha256",
  ]) {
    must(
      typeof value[field] === "string" && value[field].length > 0,
      `binding ${field} is required`,
    );
  }
  must(value.mailbox === "INBOX", "binding mailbox must be INBOX");
  must(/^[^\s@]+@[^\s@]+$/.test(value.email), "binding email is invalid");
  must(/^<[^<>\r\n]+>$/.test(value.messageId), "binding messageId is invalid");
  must(sha256Text(value.rawSha256), "binding rawSha256 is invalid");
  for (const field of ["uid", "uidValidity"]) {
    must(
      Number.isSafeInteger(value[field]) && value[field] > 0,
      `binding ${field} must be positive`,
    );
  }
  must(
    value.attachment && typeof value.attachment === "object",
    "binding attachment is required",
  );
  must(
    typeof value.attachment.id === "string" && value.attachment.id.length > 0,
    "binding attachment id is required",
  );
  must(
    /^\d+(?:\.\d+)*$/.test(value.attachment.section),
    "binding attachment section is invalid",
  );
  must(
    Number.isSafeInteger(value.attachment.rawOffset) &&
      value.attachment.rawOffset >= 0,
    "binding attachment rawOffset is invalid",
  );
  must(
    Number.isSafeInteger(value.attachment.wirePrefixLength) &&
      value.attachment.wirePrefixLength > 0,
    "binding attachment wirePrefixLength is invalid",
  );
  must(
    sha256Text(value.attachment.wirePrefixSha256),
    "binding attachment wirePrefixSha256 is invalid",
  );
  must(
    sha256Text(value.attachment.decodedSha256),
    "binding attachment decodedSha256 is invalid",
  );
  return Object.freeze({
    schemaVersion: 1,
    accountId: value.accountId,
    email: value.email,
    mailbox: value.mailbox,
    uid: value.uid,
    uidValidity: value.uidValidity,
    messageId: value.messageId,
    rawSha256: value.rawSha256,
    attachment: Object.freeze({
      id: value.attachment.id,
      section: value.attachment.section,
      rawOffset: value.attachment.rawOffset,
      wirePrefixLength: value.attachment.wirePrefixLength,
      wirePrefixSha256: value.attachment.wirePrefixSha256,
      decodedSha256: value.attachment.decodedSha256,
    }),
  });
}

export function loadBindingArtifact({
  root,
  artifactPath,
  artifactSha256,
  fixtureRawPath,
}) {
  must(sha256Text(artifactSha256), "binding artifact SHA-256 is invalid");
  const artifactBytes = secureRead(artifactPath, root, "binding artifact");
  must(
    createHash("sha256").update(artifactBytes).digest("hex") === artifactSha256,
    "binding artifact SHA-256 did not match its pin",
  );
  const binding = validateBinding(JSON.parse(artifactBytes));
  const rawFixture = secureRead(fixtureRawPath, root, "pinned raw fixture");
  must(
    createHash("sha256").update(rawFixture).digest("hex") === binding.rawSha256,
    "pinned raw fixture SHA-256 did not match the binding artifact",
  );
  return binding;
}

// Only a PEEK body fetch for the bound UID is eligible. Header and MIME
// fetches are deliberately never faulted: a partial header is not an
// attachment-download interruption.
export function classifyFetchCommand(line, binding) {
  const match = /^(\S+)\s+UID FETCH\s+(\d+)\s+\(([^\r\n]*)\)\r?\n$/i.exec(line);
  if (!match) return { kind: "other" };
  const [, tag, uidText, fields] = match;
  const uid = Number(uidText);
  if (uid !== binding.uid) return { kind: "other", tag, uid };
  const normalized = fields.replace(/\s+/g, " ").trim();
  const single = `BODY.PEEK[${binding.attachment.section}]`;
  if (normalized === single)
    return { kind: "single", tag, uid, section: binding.attachment.section };
  if (normalized === "FLAGS INTERNALDATE BODY.PEEK[]")
    return { kind: "all", tag, uid };
  return { kind: "other", tag, uid };
}

export function isMutatingImapCommand(line) {
  // These are deliberately checked before forwarding while an interruption is
  // armed. The overlay is test infrastructure, never a way to mask a mutation.
  return /^\S+\s+(?:UID\s+)?(?:STORE|APPEND|COPY|MOVE|EXPUNGE|DELETE|CREATE|RENAME|SUBSCRIBE|UNSUBSCRIBE|CLOSE)\b/i.test(
    line,
  );
}

export function responseLiteralHeader(chunk) {
  const end = chunk.indexOf("\r\n");
  if (end < 0) return null;
  const line = chunk.subarray(0, end + 2).toString("latin1");
  const match = /\{(\d+)\}\r\n$/.exec(line);
  if (!match) return { kind: "not-literal", bytes: end + 2, line };
  return { kind: "literal", bytes: end + 2, length: Number(match[1]), line };
}

export class LiteralTruncator {
  constructor({
    attachmentOffset,
    attachmentPrefixLength,
    attachmentPrefixSha256,
    validateHeader = () => true,
  }) {
    must(
      Number.isSafeInteger(attachmentOffset) && attachmentOffset >= 0,
      "attachmentOffset is invalid",
    );
    must(
      Number.isSafeInteger(attachmentPrefixLength) &&
        attachmentPrefixLength > 0,
      "attachmentPrefixLength is invalid",
    );
    must(
      sha256Text(attachmentPrefixSha256),
      "attachmentPrefixSha256 is invalid",
    );
    this.attachmentOffset = attachmentOffset;
    this.attachmentPrefixLength = attachmentPrefixLength;
    this.attachmentPrefixSha256 = attachmentPrefixSha256;
    this.validateHeader = validateHeader;
    this.pending = Buffer.alloc(0);
    this.header = null;
    this.delivered = 0;
    this.announced = null;
    this.done = false;
  }

  consume(chunk) {
    if (this.done) return { forward: Buffer.alloc(0), close: false };
    this.pending = Buffer.concat([this.pending, chunk]);
    if (!this.header) {
      const header = responseLiteralHeader(this.pending);
      if (!header) return { forward: Buffer.alloc(0), close: false };
      if (header.kind !== "literal")
        throw new Error("targeted IMAP response did not announce a literal");
      must(
        this.validateHeader(header.line),
        "targeted IMAP response did not match the bound UID and body section",
      );
      must(
        header.length > this.attachmentOffset + this.attachmentPrefixLength,
        "targeted literal is too small for a nonzero attachment interruption prefix",
      );
      this.header = header;
      this.announced = header.length;
    }
    const delivered = this.attachmentOffset + this.attachmentPrefixLength;
    const desired = this.header.bytes + delivered;
    if (this.pending.length < desired)
      return { forward: Buffer.alloc(0), close: false };
    const attachmentPrefix = this.pending.subarray(
      this.header.bytes + this.attachmentOffset,
      this.header.bytes + delivered,
    );
    must(
      createHash("sha256").update(attachmentPrefix).digest("hex") ===
        this.attachmentPrefixSha256,
      "targeted literal did not contain the bound attachment prefix",
    );
    const forward = this.pending.subarray(0, desired);
    this.delivered = delivered;
    this.pending = Buffer.alloc(0);
    this.done = true;
    return {
      forward,
      close: true,
      announced: this.announced,
      delivered: this.delivered,
    };
  }
}

class LiteralStaller {
  constructor({
    attachmentPrefixLength,
    attachmentPrefixSha256,
    validateHeader,
  }) {
    must(
      Number.isSafeInteger(attachmentPrefixLength) &&
        attachmentPrefixLength > 0 &&
        attachmentPrefixLength <= STALL_FORWARD_BYTES,
      "stall mode attachment prefix must fit in the 12-byte forwarded prefix",
    );
    this.attachmentPrefixLength = attachmentPrefixLength;
    this.attachmentPrefixSha256 = attachmentPrefixSha256;
    this.validateHeader = validateHeader;
    this.pending = Buffer.alloc(0);
    this.header = null;
    this.done = false;
    this.withheldLiteralBytes = 0;
  }

  consume(chunk) {
    if (this.done) {
      return {
        forward: Buffer.alloc(0),
        stalled: true,
        announced: STALL_LITERAL_BYTES,
        delivered: STALL_FORWARD_BYTES,
        withheldLiteralBytes: this.withheldLiteralBytes,
      };
    }
    this.pending = Buffer.concat([this.pending, chunk]);
    if (!this.header) {
      const header = responseLiteralHeader(this.pending);
      if (!header) return { forward: Buffer.alloc(0), stalled: false };
      if (header.kind !== "literal")
        throw new Error("targeted IMAP response did not announce a literal");
      must(
        this.validateHeader(header.line),
        "targeted IMAP response did not match the bound UID and body section",
      );
      must(
        header.length === STALL_LITERAL_BYTES,
        "stall mode requires an announced 44-byte attachment literal",
      );
      this.header = header;
    }
    const desired = this.header.bytes + STALL_FORWARD_BYTES;
    if (this.pending.length < desired)
      return { forward: Buffer.alloc(0), stalled: false };
    const attachmentPrefix = this.pending.subarray(
      this.header.bytes,
      this.header.bytes + this.attachmentPrefixLength,
    );
    must(
      createHash("sha256").update(attachmentPrefix).digest("hex") ===
        this.attachmentPrefixSha256,
      "targeted literal did not contain the bound attachment prefix",
    );
    // This is the declared portion withheld from the client, rather than an
    // observation of TLS packet boundaries. Receipts stay deterministic when
    // the upstream literal is fragmented differently.
    this.withheldLiteralBytes = STALL_LITERAL_BYTES - STALL_FORWARD_BYTES;
    const forward = this.pending.subarray(0, desired);
    this.pending = Buffer.alloc(0);
    this.done = true;
    return {
      forward,
      stalled: true,
      announced: STALL_LITERAL_BYTES,
      delivered: STALL_FORWARD_BYTES,
      withheldLiteralBytes: this.withheldLiteralBytes,
    };
  }
}

function atomicallyWrite(path, value) {
  const temporary = `${path}.${randomUUID()}.tmp`;
  writeFileSync(temporary, `${JSON.stringify(value, null, 2)}\n`, {
    flag: "wx",
    mode: 0o600,
  });
  renameSync(temporary, path);
  chmodSync(path, 0o600);
}

export function validateControlCommand(command, nonce) {
  must(
    command && typeof command === "object",
    "attachment interruption command must be an object",
  );
  must(
    command.nonce === nonce,
    "attachment interruption command nonce is invalid",
  );
  must(
    /^[a-f0-9-]{36}$/.test(command.commandId),
    "attachment interruption command id is invalid",
  );
  must(
    ["arm-single", "arm-all", "arm-stall-single", "status"].includes(
      command.action,
    ),
    "attachment interruption command action is invalid",
  );
  return command;
}

function consumeCommand(root, nonce) {
  const path = join(root, COMMAND_FILE);
  if (!existsSync(path)) return null;
  try {
    return validateControlCommand(
      JSON.parse(readFileSync(path, "utf8")),
      nonce,
    );
  } finally {
    // An invalid control request is a one-shot rejection too. Leaving it in
    // place would turn a malformed file into a polling loop and obscure later
    // valid controls.
    unlinkSync(path);
  }
}

export function createState({ binding, nonce }) {
  return {
    schemaVersion: 1,
    mode: "fictional-attachment-interruption-inner-tls",
    bound: {
      accountId: binding.accountId,
      email: binding.email,
      mailbox: binding.mailbox,
      uid: binding.uid,
      uidValidity: binding.uidValidity,
      messageId: binding.messageId,
      rawSha256: binding.rawSha256,
      attachmentId: binding.attachment.id,
      attachmentSection: binding.attachment.section,
      attachmentRawOffset: binding.attachment.rawOffset,
      attachmentWirePrefixLength: binding.attachment.wirePrefixLength,
      attachmentWirePrefixSha256: binding.attachment.wirePrefixSha256,
    },
    controlNonce: nonce,
    armed: null,
    inFlight: null,
    readOnlyWindow: false,
    attempts: [],
    stall: { pending: null, closed: [] },
    lastCommandId: null,
    lastCommandError: null,
  };
}

function loginAttempt(line) {
  const match = /^(\S+)\s+LOGIN\s+(?:"((?:[^"\\]|\\.)*)"|([^\s]+))\s+/i.exec(
    line,
  );
  if (!match) return null;
  return {
    tag: match[1],
    username: (match[2] ?? match[3]).replace(/\\([\\"])/g, "$1"),
  };
}

function selectAttempt(line) {
  const match = /^(\S+)\s+SELECT\s+"INBOX"\r?\n$/i.exec(line);
  return match ? { tag: match[1] } : null;
}

function escapeRegex(value) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function targetResponseMatches(headerLine, target, binding) {
  if (
    !new RegExp(
      `^\\*\\s+\\d+\\s+FETCH\\s+\\(.*\\bUID\\s+${binding.uid}\\b`,
      "i",
    ).test(headerLine)
  )
    return false;
  if ((headerLine.match(/\bBODY\[/gi) ?? []).length !== 1) return false;
  const body =
    target.kind === "single"
      ? `BODY\\[${escapeRegex(binding.attachment.section)}\\](?:<0>)?`
      : "BODY\\[\\](?:<0>)?";
  return new RegExp(`${body}\\s+\\{\\d+\\}\\r\\n$`, "i").test(headerLine);
}

export async function createAttachmentInterruptionOverlay({
  rootPath,
  backendPort,
  certificate,
  key,
  ca,
  accountId,
  bindingArtifactPath,
  bindingArtifactSha256,
  fixtureRawPath,
  stallDeadlineMs = DEFAULT_STALL_DEADLINE_MS,
}) {
  const root = ownedRoot(rootPath);
  must(
    Number.isSafeInteger(backendPort) && backendPort > 0 && backendPort < 65536,
    "backend port is invalid",
  );
  must(
    Number.isSafeInteger(stallDeadlineMs) &&
      stallDeadlineMs >= 25 &&
      stallDeadlineMs <= DEFAULT_STALL_DEADLINE_MS,
    "stall deadline must be between 25 milliseconds and 30 seconds",
  );
  const binding = loadBindingArtifact({
    root,
    artifactPath: bindingArtifactPath,
    artifactSha256: bindingArtifactSha256,
    fixtureRawPath,
  });
  must(
    accountId === binding.accountId,
    "route accountId must match the bound attachment account",
  );
  const certificateBytes = secureRead(certificate, root, "overlay certificate");
  const keyBytes = secureRead(key, root, "overlay key");
  const caBytes = secureRead(ca, root, "upstream CA certificate");
  const nonce = randomBytes(24).toString("hex");
  const transcript = join(root, "attachment-interruption-transcript.jsonl");
  writeFileSync(transcript, "", { flag: "wx", mode: 0o600 });
  const log = (entry) =>
    appendFileSync(
      transcript,
      `${JSON.stringify({ at: new Date().toISOString(), ...entry })}\n`,
    );
  const state = createState({ binding, nonce });
  const writeState = () => atomicallyWrite(join(root, STATE_FILE), state);
  const activate = () => {
    const command = consumeCommand(root, nonce);
    if (!command) return;
    state.lastCommandId = command.commandId;
    state.lastCommandError = null;
    if (command.action === "status") {
      writeState();
      return;
    }
    if (state.armed || state.inFlight) {
      state.lastCommandError =
        "an attachment interruption is already armed or in flight";
      writeState();
      return;
    }
    state.readOnlyWindow = true;
    state.armed = {
      kind: command.action === "arm-all" ? "all" : "single",
      stall: command.action === "arm-stall-single",
      commandId: command.commandId,
      armedAt: new Date().toISOString(),
    };
    log({
      direction: "control",
      action: state.armed.kind,
      commandId: command.commandId,
      stall: state.armed.stall,
      readOnlyWindowOpened: true,
      binding: state.bound,
    });
    writeState();
  };
  writeState();
  const poll = setInterval(() => {
    try {
      activate();
    } catch (error) {
      state.lastCommandError = String(error.message || error);
      writeState();
    }
  }, 25);
  const sockets = new Set();
  const stallFinalizers = new Set();
  let overlayClosing = false;
  const server = createServer(
    { cert: certificateBytes, key: keyBytes, minVersion: "TLSv1.2" },
    (client) => {
      const backend = tlsConnect({
        host: "127.0.0.1",
        port: backendPort,
        servername: "localhost",
        ca: caBytes,
        rejectUnauthorized: true,
        minVersion: "TLSv1.2",
      });
      sockets.add(client);
      sockets.add(backend);
      let request = "";
      let target = null;
      let truncator = null;
      let staller = null;
      let attempt = null;
      let backendControl = "";
      let stallTimer = null;
      let stallFinalized = false;
      const protocolCounts = {
        clientCommands: 0,
        backendChunks: 0,
        clientEofs: 0,
        backendEofs: 0,
      };
      const session = {
        pendingLogin: null,
        authenticated: false,
        boundAccount: false,
        pendingSelect: null,
        selected: false,
        uidValidity: null,
      };
      let closed = false;
      let completingPrefix = false;
      const close = () => {
        if (closed) return;
        closed = true;
        if (stallTimer) clearTimeout(stallTimer);
        client.destroy();
        backend.destroy();
      };
      const finalizeStall = (outcome, error = null) => {
        if (!staller || stallFinalized || !state.stall.pending) return;
        stallFinalized = true;
        if (stallTimer) clearTimeout(stallTimer);
        const closedAt = new Date().toISOString();
        const record = {
          ...state.stall.pending,
          outcome,
          error,
          closedAt,
          protocolCounts: { ...protocolCounts },
        };
        state.stall.pending = null;
        state.stall.closed.push(record);
        state.attempts.push({
          commandId: attempt.commandId,
          kind: target.kind,
          tag: target.tag,
          uid: target.uid,
          announcedLiteralBytes: record.announcedLiteralBytes,
          deliveredLiteralBytes: record.deliveredLiteralBytes,
          outcome,
          closedAt,
        });
        state.armed = null;
        state.inFlight = null;
        state.lastCommandError = error;
        writeState();
        log({
          direction: "server",
          action: "stalled-literal-closed",
          commandId: attempt.commandId,
          outcome,
          error,
          announcedLiteralBytes: record.announcedLiteralBytes,
          deliveredLiteralBytes: record.deliveredLiteralBytes,
          protocolCounts: record.protocolCounts,
        });
      };
      stallFinalizers.add(finalizeStall);
      const failAttempt = (message, action) => {
        state.lastCommandError = message;
        state.armed = null;
        state.inFlight = null;
        writeState();
        log({
          direction: "server",
          action,
          commandId: attempt?.commandId ?? null,
          error: message,
        });
        close();
      };
      client.on("data", (chunk) => {
        request += chunk.toString("latin1");
        while (request.includes("\n")) {
          const end = request.indexOf("\n") + 1;
          const line = request.slice(0, end);
          request = request.slice(end);
          protocolCounts.clientCommands += 1;
          activate();
          const observed = classifyFetchCommand(line, binding);
          if (state.readOnlyWindow && isMutatingImapCommand(line)) {
            state.lastCommandError =
              "mutating IMAP command rejected after the attachment interruption read-only window opened";
            state.armed = null;
            writeState();
            log({
              direction: "client",
              action: "rejected-mutation",
              commandVerb: line.trim().split(/\s+/)[1] ?? "unknown",
            });
            close();
            return;
          }
          const login = loginAttempt(line);
          if (login) session.pendingLogin = login;
          const selection = selectAttempt(line);
          if (selection) session.pendingSelect = selection;
          if (target) {
            state.lastCommandError =
              "multiple IMAP commands arrived while an attachment literal was pending";
            state.armed = null;
            state.inFlight = null;
            writeState();
            log({
              direction: "client",
              action: "rejected-ambiguous-command",
              commandVerb: line.trim().split(/\s+/)[1] ?? "unknown",
            });
            close();
            return;
          }
          // A different successfully authenticated fictional account can use
          // the same UID and body section. It must pass through before either
          // account A's in-flight guard or explicitly armed fault is examined.
          if (
            observed.kind !== "other" &&
            session.authenticated &&
            !session.boundAccount
          ) {
            backend.write(Buffer.from(line, "latin1"));
            continue;
          }
          if (observed.kind !== "other" && state.inFlight) {
            state.lastCommandError =
              "matching attachment fetch rejected because another attempt is in flight";
            writeState();
            log({
              direction: "client",
              action: "rejected-in-flight",
              kind: observed.kind,
              tag: observed.tag,
              uid: observed.uid,
            });
            close();
            return;
          }
          if (!state.armed || observed.kind !== state.armed.kind) {
            backend.write(Buffer.from(line, "latin1"));
            continue;
          }
          if (
            !session.authenticated ||
            !session.selected ||
            session.uidValidity !== binding.uidValidity
          ) {
            state.lastCommandError =
              "bound attachment fetch rejected before the fictional account and mailbox epoch were verified";
            state.armed = null;
            writeState();
            log({
              direction: "client",
              action: "rejected-unverified-session",
              kind: observed.kind,
              tag: observed.tag,
              uid: observed.uid,
            });
            close();
            return;
          }
          // This reservation is deliberately before the upstream write. A second
          // connection can observe only `inFlight`, never a still-armed command.
          attempt = {
            ...state.armed,
            kind: observed.kind,
            tag: observed.tag,
            uid: observed.uid,
            reservedAt: new Date().toISOString(),
          };
          state.armed = null;
          state.inFlight = attempt;
          writeState();
          target = observed;
          if (attempt.stall) {
            staller = new LiteralStaller({
              attachmentPrefixLength: binding.attachment.wirePrefixLength,
              attachmentPrefixSha256: binding.attachment.wirePrefixSha256,
              validateHeader: (header) =>
                targetResponseMatches(header, observed, binding),
            });
          } else {
            truncator = new LiteralTruncator({
              attachmentOffset:
                observed.kind === "all" ? binding.attachment.rawOffset : 0,
              attachmentPrefixLength: binding.attachment.wirePrefixLength,
              attachmentPrefixSha256: binding.attachment.wirePrefixSha256,
              validateHeader: (header) =>
                targetResponseMatches(header, observed, binding),
            });
          }
          log({
            direction: "client",
            action: "targeted-fetch",
            kind: observed.kind,
            tag: observed.tag,
            uid: observed.uid,
            commandId: attempt.commandId,
            stall: attempt.stall,
          });
          backend.write(Buffer.from(line, "latin1"));
        }
      });
      backend.on("data", (chunk) => {
        protocolCounts.backendChunks += 1;
        if (!target) {
          backendControl = (backendControl + chunk.toString("latin1")).slice(
            -16_384,
          );
          if (
            session.pendingLogin &&
            new RegExp(
              `(?:^|\\r?\\n)${escapeRegex(session.pendingLogin.tag)} OK\\b`,
              "i",
            ).test(backendControl)
          ) {
            session.authenticated = true;
            session.boundAccount =
              session.pendingLogin.username === binding.email;
            session.pendingLogin = null;
          }
          if (session.pendingSelect) {
            const validity = new RegExp(
              `\\[UIDVALIDITY\\s+(${binding.uidValidity})\\]`,
              "i",
            ).exec(backendControl);
            const selected = new RegExp(
              `(?:^|\\r?\\n)${escapeRegex(session.pendingSelect.tag)} OK\\b`,
              "i",
            ).test(backendControl);
            if (validity && selected) {
              session.uidValidity = Number(validity[1]);
              session.selected = true;
              session.pendingSelect = null;
            }
          }
        }
        if (!target) {
          client.write(chunk);
          return;
        }
        try {
          if (staller) {
            const result = staller.consume(chunk);
            if (result.forward.length) client.write(result.forward);
            if (result.stalled) {
              if (!state.stall.pending) {
                const stalledAt = new Date().toISOString();
                state.stall.pending = {
                  commandId: attempt.commandId,
                  kind: target.kind,
                  tag: target.tag,
                  uid: target.uid,
                  announcedLiteralBytes: result.announced,
                  deliveredLiteralBytes: result.delivered,
                  withheldLiteralBytes: result.withheldLiteralBytes,
                  stalledAt,
                  protocolCounts: { ...protocolCounts },
                };
                writeState();
                log({
                  direction: "server",
                  action: "stalled-literal-pending",
                  commandId: attempt.commandId,
                  announcedLiteralBytes: result.announced,
                  deliveredLiteralBytes: result.delivered,
                  protocolCounts: { ...protocolCounts },
                });
                stallTimer = setTimeout(() => {
                  finalizeStall(
                    "fixture-deadline",
                    "fixture stall deadline elapsed before client EOF",
                  );
                  close();
                }, stallDeadlineMs);
              } else {
                state.stall.pending.withheldLiteralBytes =
                  result.withheldLiteralBytes;
                state.stall.pending.protocolCounts = { ...protocolCounts };
                writeState();
              }
              return;
            }
            return;
          }
          const result = truncator.consume(chunk);
          if (result.close) {
            state.attempts.push({
              commandId: attempt.commandId,
              kind: target.kind,
              tag: target.tag,
              uid: target.uid,
              announcedLiteralBytes: result.announced,
              deliveredLiteralBytes: result.delivered,
              terminatedAt: new Date().toISOString(),
            });
            state.armed = null;
            state.inFlight = null;
            writeState();
            log({
              direction: "server",
              action: "interrupted-literal",
              ...state.attempts.at(-1),
            });
            // `destroy()` immediately after `write()` can lose the deliberately
            // delivered attachment prefix. A TLS close-notify keeps that verified
            // prefix observable while the upstream leg is closed without receiving
            // another command.
            completingPrefix = true;
            backend.destroy();
            client.end(result.forward);
          } else if (result.forward.length) {
            client.write(result.forward);
          }
        } catch (error) {
          failAttempt(String(error.message || error), "rejected-target");
        }
      });
      client.on("error", () => {
        finalizeStall(
          "client-error",
          "client TLS connection errored while stalled",
        );
        close();
      });
      backend.on("error", () => {
        finalizeStall(
          "server-error",
          "upstream TLS connection errored while stalled",
        );
        close();
      });
      client.on("close", () => {
        sockets.delete(client);
        protocolCounts.clientEofs += 1;
        if (staller && !stallFinalized)
          finalizeStall(
            overlayClosing ? "fixture-closed" : "client-eof",
            overlayClosing
              ? "fixture closed while attachment literal was stalled"
              : null,
          );
        if (!completingPrefix) close();
        stallFinalizers.delete(finalizeStall);
      });
      backend.on("close", () => {
        sockets.delete(backend);
        protocolCounts.backendEofs += 1;
        if (staller && !stallFinalized)
          finalizeStall(
            overlayClosing ? "fixture-closed" : "server-error",
            overlayClosing
              ? "fixture closed while attachment literal was stalled"
              : "upstream TLS connection closed while attachment literal was stalled",
          );
        if (!completingPrefix) close();
        stallFinalizers.delete(finalizeStall);
      });
    },
  );
  await new Promise((resolvePromise, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolvePromise);
  });
  const metadata = {
    schemaVersion: 1,
    mode: state.mode,
    pid: process.pid,
    port: server.address().port,
    backendPort,
    routeAccountId: accountId,
    transcript,
    state: join(root, STATE_FILE),
    controlNonce: nonce,
    bound: state.bound,
  };
  writeFileSync(
    join(root, METADATA_FILE),
    `${JSON.stringify(metadata, null, 2)}\n`,
    { flag: "wx", mode: 0o600 },
  );
  return {
    port: metadata.port,
    metadata,
    state,
    close: async () => {
      overlayClosing = true;
      clearInterval(poll);
      for (const finalizeStall of stallFinalizers)
        finalizeStall(
          "fixture-closed",
          "fixture closed while attachment literal was stalled",
        );
      for (const socket of sockets) socket.destroy();
      await new Promise((resolvePromise, reject) =>
        server.close((error) => (error ? reject(error) : resolvePromise())),
      );
    },
  };
}

export function requestControl(rootPath, action) {
  const root = ownedRoot(rootPath);
  const metadata = JSON.parse(
    secureRead(join(root, METADATA_FILE), root, "overlay metadata"),
  );
  must(
    /^[a-f0-9]{48}$/.test(metadata.controlNonce),
    "attachment interruption metadata nonce is invalid",
  );
  must(
    ["arm-single", "arm-all", "arm-stall-single", "status"].includes(action),
    "unknown attachment interruption action",
  );
  const commandId = randomUUID();
  const temporary = join(root, `${COMMAND_FILE}.${commandId}.tmp`);
  const finalPath = join(root, COMMAND_FILE);
  try {
    writeFileSync(
      temporary,
      JSON.stringify({ action, commandId, nonce: metadata.controlNonce }),
      { flag: "wx", mode: 0o600 },
    );
    linkSync(temporary, finalPath);
  } finally {
    if (existsSync(temporary)) unlinkSync(temporary);
  }
  return commandId;
}

function parsedArgs(argv) {
  const args = new Map();
  for (let index = 0; index < argv.length; index += 2)
    args.set(argv[index], argv[index + 1]);
  return args;
}

async function main() {
  const [mode, ...rest] = process.argv.slice(2);
  const args = parsedArgs(rest);
  if (mode === "control") {
    const id = requestControl(args.get("--root"), args.get("--action"));
    process.stdout.write(
      `${JSON.stringify({ status: "requested", commandId: id })}\n`,
    );
    return;
  }
  if (mode !== "serve") throw new Error("use serve or control");
  for (const key of [
    "--root",
    "--backend-port",
    "--certificate",
    "--key",
    "--ca",
    "--account-id",
    "--binding-artifact",
    "--binding-artifact-sha256",
    "--fixture-raw",
  ])
    must(args.get(key), `missing ${key}`);
  const overlay = await createAttachmentInterruptionOverlay({
    rootPath: args.get("--root"),
    backendPort: Number(args.get("--backend-port")),
    certificate: args.get("--certificate"),
    key: args.get("--key"),
    ca: args.get("--ca"),
    accountId: args.get("--account-id"),
    bindingArtifactPath: args.get("--binding-artifact"),
    bindingArtifactSha256: args.get("--binding-artifact-sha256"),
    fixtureRawPath: args.get("--fixture-raw"),
  });
  const stop = () => overlay.close().then(() => process.exit(0));
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);
}

if (
  process.argv[1] &&
  import.meta.url === new URL(`file://${resolve(process.argv[1])}`).href
) {
  main().catch((error) => {
    console.error(error.stack || error);
    process.exit(1);
  });
}
