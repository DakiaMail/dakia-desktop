import { emitTo, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { showNativeMessage } from "./nativeFeedback";
import type {
  AiMessageTarget,
  ComposeAttachment,
  MailSummary,
  SendRecoverySeed,
} from "./types";

export type ComposeSeed = {
  /** Stable idempotency key for one user-created compose attempt. */
  sendAttemptId?: string;
  accountId?: string;
  to?: string;
  cc?: string;
  bcc?: string;
  subject?: string;
  body?: string;
  bodyHtml?: string | null;
  inReplyTo?: string;
  references?: string;
  contextMessageIds?: string[];
  /** Immutable receipts for every message allowed into an AI request. */
  contextMessageTargets?: AiMessageTarget[];
  forwardMessageId?: string;
  forwardMessageUidValidity?: number | null;
  attachments?: ComposeAttachment[];
  /**
   * The exact message accepted from the recovery bridge. The editor may
   * normalize its display value, but an untouched retry must send and persist
   * these original protocol values.
   */
  recoveryMessage?: {
    to: string[];
    cc: string[];
    bcc: string[];
    subject: string;
    bodyText: string;
    bodyHtml: string | null;
    inReplyTo: string | null;
    references: string | null;
    attachments: ComposeAttachment[];
  };
  sendState?: "sending" | "delivery_uncertain" | "sent_copy_failed";
};

const isTauri = () => "__TAURI_INTERNALS__" in window;
const composeSeedStoragePrefix = "dakia.compose-seed.";
const activeComposeSeedKey = "dakia.compose-active-seed";
const composeSeedDatabase = "dakia-compose-seeds";
const composeSeedStore = "seeds";
const composeSeedOperations = new Map<string, Promise<void>>();
const composeWindowReadyTimeoutMs = 5_000;

type ComposeWindowReady = { label: string; seedToken: string };
type ComposeWindowTarget = {
  label: string;
  seedToken: string;
  seed: ComposeSeed;
};
type NativeComposeRememberedSeed = {
  label: string;
  seedDbKey?: string;
  recoveryKey?: string;
  seed: ComposeSeed;
};
const composeWindowLabel =
  /^compose-[a-f0-9]{8}-[a-f0-9]{4}-4[a-f0-9]{3}-[89ab][a-f0-9]{3}-[a-f0-9]{12}$/;
const opaqueRecoveryKey =
  /^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/;
const sendAttemptId = opaqueRecoveryKey;
const maxAttachmentBytes = 25 * 1024 * 1024;
const maxAttachmentTotalBytes = 50 * 1024 * 1024;
const maxAttachments = 50;

function parseComposeSeed(value: string | null): ComposeSeed | undefined {
  if (!value) return undefined;
  try {
    const seed: unknown = JSON.parse(value);
    return isComposeSeed(seed) ? seed : undefined;
  } catch {
    return undefined;
  }
}

function nativeComposeSeedToken() {
  return new URLSearchParams(window.location.search).get("seedDbKey");
}

function nativeComposeRecoveryToken() {
  return new URLSearchParams(window.location.search).get("recoveryKey");
}

function isValidNativeComposeRecoveryRequest(
  params = new URLSearchParams(window.location.search),
) {
  const recoveryKey = params.get("recoveryKey");
  return Boolean(
    isTauri() &&
    recoveryKey &&
    opaqueRecoveryKey.test(recoveryKey) &&
    !params.has("seedKey") &&
    !params.has("seedDbKey") &&
    params.get("opener") === "main",
  );
}

function isComposeSeed(value: unknown): value is ComposeSeed {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const seed = value as Record<string, unknown>;
  const strings = [
    "sendAttemptId",
    "accountId",
    "to",
    "cc",
    "bcc",
    "subject",
    "body",
    "bodyHtml",
    "inReplyTo",
    "references",
    "forwardMessageId",
  ];
  return (
    strings
      .filter((key) => key !== "bodyHtml")
      .every(
        (key) => seed[key] === undefined || typeof seed[key] === "string",
      ) &&
    (seed.bodyHtml === undefined ||
      seed.bodyHtml === null ||
      typeof seed.bodyHtml === "string") &&
    (seed.contextMessageIds === undefined ||
      (Array.isArray(seed.contextMessageIds) &&
        seed.contextMessageIds.every((value) => typeof value === "string"))) &&
    (seed.contextMessageTargets === undefined ||
      Array.isArray(seed.contextMessageTargets)) &&
    (seed.forwardMessageUidValidity === undefined ||
      seed.forwardMessageUidValidity === null ||
      typeof seed.forwardMessageUidValidity === "number") &&
    (seed.attachments === undefined || Array.isArray(seed.attachments)) &&
    (seed.recoveryMessage === undefined ||
      isRecoveryComposeMessage(seed.recoveryMessage)) &&
    (seed.sendState === undefined ||
      seed.sendState === "sending" ||
      seed.sendState === "delivery_uncertain" ||
      seed.sendState === "sent_copy_failed")
  );
}

function isRecoveryComposeMessage(
  value: unknown,
): value is NonNullable<ComposeSeed["recoveryMessage"]> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  const message = value as Record<string, unknown>;
  const recipients = [message.to, message.cc, message.bcc];
  return (
    recipients.every(
      (entries) =>
        Array.isArray(entries) &&
        entries.every((entry) => typeof entry === "string"),
    ) &&
    typeof message.subject === "string" &&
    typeof message.bodyText === "string" &&
    (message.bodyHtml === null || typeof message.bodyHtml === "string") &&
    (message.inReplyTo === null || typeof message.inReplyTo === "string") &&
    (message.references === null || typeof message.references === "string") &&
    Array.isArray(message.attachments) &&
    message.attachments.every(isComposeAttachment)
  );
}

function isComposeAttachment(value: unknown): value is ComposeAttachment {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return false;
  }
  const attachment = value as Record<string, unknown>;
  return (
    typeof attachment.filename === "string" &&
    typeof attachment.mime_type === "string" &&
    typeof attachment.content_base64 === "string" &&
    Number.isSafeInteger(attachment.size_bytes) &&
    (attachment.size_bytes as number) >= 0
  );
}

function withSendAttemptId(seed: ComposeSeed, fallback?: string) {
  if (seed.sendAttemptId) return seed;
  return { ...seed, sendAttemptId: fallback ?? crypto.randomUUID() };
}

function storeComposeSeed(seed: ComposeSeed) {
  try {
    const token = crypto.randomUUID();
    localStorage.setItem(
      `${composeSeedStoragePrefix}${token}`,
      JSON.stringify(seed),
    );
    return token;
  } catch {
    return undefined;
  }
}

function readStoredComposeSeed(token: string) {
  try {
    const key = `${composeSeedStoragePrefix}${token}`;
    const value = localStorage.getItem(key);
    return parseComposeSeed(value);
  } catch {
    return undefined;
  }
}

export function readComposeSeed(): ComposeSeed {
  const params = new URLSearchParams(window.location.search);
  const token = params.get("seedKey");
  if (token) {
    const stored = readStoredComposeSeed(token);
    if (stored) return withSendAttemptId(stored, token);
    return { sendAttemptId: token };
  }
  const recoveryToken = params.get("recoveryKey");
  if (recoveryToken) {
    if (isTauri() && !isValidNativeComposeRecoveryRequest(params)) {
      return {};
    }
    return readRememberedComposeSeed() ?? {};
  }
  const databaseToken = params.get("seedDbKey");
  if (databaseToken) {
    const remembered = readRememberedComposeSeed();
    return remembered
      ? withSendAttemptId(remembered, databaseToken)
      : { sendAttemptId: databaseToken };
  }
  return parseComposeSeed(params.get("seed")) ?? {};
}

function readRememberedComposeSeed() {
  try {
    const value = window.sessionStorage.getItem(activeComposeSeedKey);
    if (!isTauri()) return parseComposeSeed(value);
    const seedToken = nativeComposeSeedToken();
    const recoveryToken = nativeComposeRecoveryToken();
    if (recoveryToken && !isValidNativeComposeRecoveryRequest()) {
      return undefined;
    }
    const token = recoveryToken ?? seedToken;
    if (!value || !token) return undefined;
    const remembered = JSON.parse(value) as unknown;
    if (
      !remembered ||
      typeof remembered !== "object" ||
      Array.isArray(remembered)
    ) {
      return undefined;
    }
    const envelope = remembered as Partial<NativeComposeRememberedSeed>;
    const boundToken = recoveryToken
      ? envelope.recoveryKey
      : envelope.seedDbKey;
    return envelope.label === getCurrentWindow().label &&
      boundToken === token &&
      isComposeSeed(envelope.seed)
      ? envelope.seed
      : undefined;
  } catch {
    return undefined;
  }
}

/** Preserve a delivered native seed for reloads in the child WebView only. */
export function rememberComposeSeed(seed: ComposeSeed) {
  try {
    const seedToken = nativeComposeSeedToken();
    const recoveryToken = nativeComposeRecoveryToken();
    const token = recoveryToken ?? seedToken;
    const value =
      isTauri() && token
        ? {
            label: getCurrentWindow().label,
            ...(recoveryToken ? { recoveryKey: token } : { seedDbKey: token }),
            seed,
          }
        : seed;
    window.sessionStorage.setItem(activeComposeSeedKey, JSON.stringify(value));
    return true;
  } catch {
    // Session storage helps reloads but must not prevent a compose handoff.
    return false;
  }
}

export function hasNativeComposeSeedHandoff() {
  if (
    !isTauri() ||
    new URLSearchParams(window.location.search).has("recoveryKey") ||
    !new URLSearchParams(window.location.search).get("seedDbKey")
  ) {
    return false;
  }
  return !readRememberedComposeSeed();
}

export function hasNativeComposeRecovery() {
  if (
    !isTauri() ||
    !new URLSearchParams(window.location.search).has("recoveryKey")
  ) {
    return false;
  }
  // An invalid recovery URL must enter the recovery failure flow. Returning
  // false here would render a normal Compose window from untrusted session
  // storage instead.
  if (!isValidNativeComposeRecoveryRequest()) return true;
  return !readRememberedComposeSeed();
}

export function readNativeComposeRecoveryKey() {
  const params = new URLSearchParams(window.location.search);
  const recoveryKey = params.get("recoveryKey");
  if (!isValidNativeComposeRecoveryRequest(params) || !recoveryKey) {
    throw new Error("Saved message recovery is unavailable.");
  }
  return recoveryKey;
}

export async function readDatabaseComposeSeed() {
  const params = new URLSearchParams(window.location.search);
  const token =
    params.get("recoveryKey") ??
    params.get("seedDbKey") ??
    params.get("seedKey");
  if (!token || !globalThis.indexedDB) return undefined;
  const database = await openComposeSeedDatabase();
  return new Promise<ComposeSeed | undefined>((resolve, reject) => {
    const transaction = database.transaction(composeSeedStore, "readonly");
    const store = transaction.objectStore(composeSeedStore);
    const request = store.get(token);
    request.onsuccess = () => {
      const seed = request.result as unknown;
      if (!isComposeSeed(seed)) {
        resolve(undefined);
        return;
      }
      if (
        params.has("recoveryKey") &&
        (!seed.sendAttemptId || !sendAttemptId.test(seed.sendAttemptId))
      ) {
        resolve(undefined);
        return;
      }
      resolve(withSendAttemptId(seed, token));
    };
    request.onerror = () => {
      database.close();
      reject(request.error ?? new Error("Could not read the compose draft."));
    };
    transaction.oncomplete = () => database.close();
    rejectTransactionOnFailure(database, transaction, reject);
  });
}

function currentComposeSeedToken() {
  const params = new URLSearchParams(window.location.search);
  return {
    local: params.get("seedKey"),
    database: params.get("recoveryKey") ?? params.get("seedDbKey"),
  };
}

export function composeSeedFromSendRecovery(value: unknown): ComposeSeed {
  if (!isSendRecoverySeed(value)) {
    throw new Error("Saved message recovery returned invalid data.");
  }
  const attachments: ComposeAttachment[] = [];
  let totalBytes = 0;
  for (const attachment of value.message.attachments) {
    const sizeBytes = decodedBase64Size(attachment.contentBase64);
    if (attachment.sizeBytes !== sizeBytes) {
      throw new Error(
        "Saved message recovery returned inconsistent attachment data.",
      );
    }
    if (sizeBytes > maxAttachmentBytes) {
      throw new Error(
        "Saved message recovery returned an oversized attachment.",
      );
    }
    totalBytes += sizeBytes;
    if (totalBytes > maxAttachmentTotalBytes) {
      throw new Error("Saved message recovery returned oversized attachments.");
    }
    attachments.push({
      filename: attachment.filename,
      mime_type: attachment.mimeType,
      content_base64: attachment.contentBase64,
      size_bytes: sizeBytes,
    });
  }
  return {
    sendAttemptId: value.attemptId,
    accountId: value.accountId,
    to: value.message.to.join(", "),
    cc: value.message.cc.join(", "),
    bcc: value.message.bcc.join(", "),
    subject: value.message.subject,
    body: value.message.bodyText,
    bodyHtml: value.message.bodyHtml,
    inReplyTo: value.message.inReplyTo ?? undefined,
    references: value.message.references ?? undefined,
    attachments,
    recoveryMessage: {
      to: [...value.message.to],
      cc: [...value.message.cc],
      bcc: [...value.message.bcc],
      subject: value.message.subject,
      bodyText: value.message.bodyText,
      bodyHtml: value.message.bodyHtml,
      inReplyTo: value.message.inReplyTo,
      references: value.message.references,
      attachments: attachments.map((attachment) => ({ ...attachment })),
    },
    sendState:
      value.status === "pending"
        ? "sending"
        : value.status === "delivery_uncertain" ||
            value.status === "sent_copy_failed"
          ? value.status
          : undefined,
  };
}

function isSendRecoverySeed(value: unknown): value is SendRecoverySeed {
  if (!value || typeof value !== "object" || Array.isArray(value)) return false;
  const recovery = value as Partial<SendRecoverySeed>;
  const message = recovery.message;
  if (
    typeof recovery.accountId !== "string" ||
    !sendAttemptId.test(recovery.accountId) ||
    typeof recovery.attemptId !== "string" ||
    !sendAttemptId.test(recovery.attemptId) ||
    ![
      "not_submitted",
      "pending",
      "delivery_uncertain",
      "sent_copy_failed",
    ].includes(recovery.status ?? "") ||
    !message ||
    typeof message !== "object" ||
    message.accountId !== recovery.accountId
  ) {
    return false;
  }
  const strings = [message.subject, message.bodyText];
  const optionalStrings = [
    message.bodyHtml,
    message.inReplyTo,
    message.references,
  ];
  const recipients = [message.to, message.cc, message.bcc];
  return (
    strings.every((entry) => typeof entry === "string") &&
    optionalStrings.every(
      (entry) => entry === null || typeof entry === "string",
    ) &&
    recipients.every(
      (entries) =>
        Array.isArray(entries) &&
        entries.every((entry) => typeof entry === "string"),
    ) &&
    Array.isArray(message.attachments) &&
    message.attachments.length <= maxAttachments &&
    message.attachments.every(
      (attachment) =>
        Boolean(attachment) &&
        typeof attachment === "object" &&
        typeof attachment.filename === "string" &&
        typeof attachment.mimeType === "string" &&
        typeof attachment.contentBase64 === "string" &&
        Number.isSafeInteger(attachment.sizeBytes) &&
        attachment.sizeBytes >= 0,
    )
  );
}

function decodedBase64Size(value: string) {
  if (
    value.length % 4 !== 0 ||
    value.length > Math.ceil(maxAttachmentBytes / 3) * 4 ||
    !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(
      value,
    )
  ) {
    throw new Error("Saved message recovery returned invalid attachment data.");
  }
  const padding = value.endsWith("==") ? 2 : value.endsWith("=") ? 1 : 0;
  return (value.length / 4) * 3 - padding;
}

/** Persist the active compose seed scoped by the current window URL. */
export async function persistComposeSeed(seed: ComposeSeed) {
  const token = currentComposeSeedToken();
  const key = token.database ?? token.local;
  if (!key) throw new Error("Compose draft storage is unavailable.");
  if (
    new URLSearchParams(window.location.search).has("recoveryKey") &&
    (!seed.sendAttemptId || !sendAttemptId.test(seed.sendAttemptId))
  ) {
    throw new Error("Saved message recovery is missing its send attempt.");
  }

  // Active drafts use IndexedDB so permitted attachments do not exceed the
  // small localStorage quota. The original local seed remains a startup
  // fallback until a confirmed send discards both records.
  if (globalThis.indexedDB) {
    await queueComposeSeedOperation(key, () =>
      storeDatabaseComposeSeed(withSendAttemptId(seed, key), key).then(
        () => undefined,
      ),
    );
    return;
  }
  if (token.local) {
    try {
      localStorage.setItem(
        `${composeSeedStoragePrefix}${token.local}`,
        JSON.stringify(withSendAttemptId(seed, token.local)),
      );
    } catch {
      throw new Error("Compose draft storage is unavailable.");
    }
    return;
  }
  throw new Error("Compose draft storage is unavailable.");
}

/** Remove a delivered draft only after the native send command confirms it. */
export async function discardComposeSeed() {
  const token = currentComposeSeedToken();
  const key = token.database ?? token.local;
  if (key && globalThis.indexedDB) {
    await queueComposeSeedOperation(key, () => deleteDatabaseComposeSeed(key));
  }
  if (token.local) {
    try {
      localStorage.removeItem(`${composeSeedStoragePrefix}${token.local}`);
    } catch {
      if (!globalThis.indexedDB) {
        throw new Error("Compose draft storage is unavailable.");
      }
    }
  }
}

function queueComposeSeedOperation(
  token: string,
  operation: () => Promise<void>,
) {
  const previous = composeSeedOperations.get(token) ?? Promise.resolve();
  const queued = previous.catch(() => undefined).then(operation);
  composeSeedOperations.set(token, queued);
  void queued.then(
    () => {
      if (composeSeedOperations.get(token) === queued) {
        composeSeedOperations.delete(token);
      }
    },
    () => {
      if (composeSeedOperations.get(token) === queued) {
        composeSeedOperations.delete(token);
      }
    },
  );
  return queued;
}

export function openComposeWindow(seed: ComposeSeed) {
  const seededAttempt = withSendAttemptId(seed);
  const params = new URLSearchParams({ view: "compose" });

  // Each native WebView may have its own storage partition. The opaque token
  // is only a rendezvous key: content crosses directly to the exact child
  // after it has registered its listener.
  if (isTauri()) {
    const token = crypto.randomUUID();
    params.set("seedDbKey", token);
    void openNativeComposeWindow(
      token,
      seededAttempt,
      `/?${params.toString()}`,
    ).catch(() => reportComposeSeedFailure());
    return;
  }

  const storedSeedToken = storeComposeSeed(seededAttempt);
  if (storedSeedToken) params.set("seedKey", storedSeedToken);
  else params.set("seed", JSON.stringify(seededAttempt));
  createComposeWindow(`/?${params.toString()}`);
}

async function openNativeComposeWindow(
  token: string,
  seed: ComposeSeed,
  url: string,
) {
  const ready = await waitForComposeWindowReady(token);
  try {
    let childLabel: string | undefined;
    const create = createComposeWindow(url).then((label) => {
      childLabel = label;
      ready.expectLabel(label);
    });
    await Promise.all([create, ready.wait]);
    if (!childLabel) throw new Error("Compose window did not return a label.");
    await emitTo(childLabel, "compose-target", {
      label: childLabel,
      seedToken: token,
      seed,
    });
  } finally {
    ready.dispose();
  }
}

async function waitForComposeWindowReady(seedToken: string) {
  let expectedLabel: string | undefined;
  const reportedLabels = new Set<string>();
  let resolveReady: () => void = () => undefined;
  let rejectReady: (reason: Error) => void = () => undefined;
  const wait = new Promise<void>((resolve, reject) => {
    resolveReady = resolve;
    rejectReady = reject;
  });
  let closed = false;
  let unlisten: UnlistenFn = () => undefined;
  const resolveMatchingReady = () => {
    if (expectedLabel && reportedLabels.has(expectedLabel)) resolveReady();
  };
  const timeout = window.setTimeout(() => {
    rejectReady(new Error("Compose window did not become ready."));
  }, composeWindowReadyTimeoutMs);
  const registration = listen<ComposeWindowReady>("compose-ready", (event) => {
    const label = event.payload?.label;
    if (
      event.payload?.seedToken === seedToken &&
      typeof label === "string" &&
      composeWindowLabel.test(label)
    ) {
      reportedLabels.add(label);
      resolveMatchingReady();
    }
  }).then((registered) => {
    if (closed) registered();
    else unlisten = registered;
  });
  try {
    await Promise.race([registration, wait]);
  } catch (error) {
    closed = true;
    window.clearTimeout(timeout);
    unlisten();
    throw error;
  }
  return {
    wait,
    expectLabel: async (label: string) => {
      expectedLabel = label;
      resolveMatchingReady();
    },
    dispose: () => {
      closed = true;
      window.clearTimeout(timeout);
      unlisten();
    },
  };
}

async function reportComposeSeedFailure() {
  try {
    await showNativeMessage(
      "Could not open a new message",
      "Try again.",
      "error",
    );
  } catch {
    // The failed storage handoff is already contained: no window was opened.
  }
}

async function createComposeWindow(url: string): Promise<string> {
  if (!isTauri()) {
    window.open(
      url,
      "_blank",
      "popup=yes,width=880,height=700,resizable=yes,scrollbars=no",
    );
    return "";
  }

  const params = new URL(url, window.location.origin).searchParams;
  return invoke<string>("open_compose_window", {
    seedKey: params.get("seedKey"),
    seedDbKey: params.get("seedDbKey"),
  });
}

async function storeDatabaseComposeSeed(
  seed: ComposeSeed,
  existingToken?: string,
) {
  const database = await openComposeSeedDatabase();
  const token = existingToken ?? crypto.randomUUID();
  return new Promise<string>((resolve, reject) => {
    const transaction = database.transaction(composeSeedStore, "readwrite");
    transaction.objectStore(composeSeedStore).put(seed, token);
    transaction.oncomplete = () => {
      database.close();
      resolve(token);
    };
    rejectTransactionOnFailure(database, transaction, reject);
  });
}

async function deleteDatabaseComposeSeed(token: string) {
  const database = await openComposeSeedDatabase();
  return new Promise<void>((resolve, reject) => {
    const transaction = database.transaction(composeSeedStore, "readwrite");
    transaction.objectStore(composeSeedStore).delete(token);
    transaction.oncomplete = () => {
      database.close();
      resolve();
    };
    rejectTransactionOnFailure(database, transaction, reject);
  });
}

function rejectTransactionOnFailure(
  database: IDBDatabase,
  transaction: IDBTransaction,
  reject: (reason?: unknown) => void,
) {
  const rejectTransaction = () => {
    database.close();
    reject(
      transaction.error ??
        new Error("Compose draft storage transaction aborted."),
    );
  };
  transaction.onerror = rejectTransaction;
  transaction.onabort = rejectTransaction;
}

function openComposeSeedDatabase() {
  return new Promise<IDBDatabase>((resolve, reject) => {
    const request = indexedDB.open(composeSeedDatabase, 1);
    request.onupgradeneeded = () => {
      if (!request.result.objectStoreNames.contains(composeSeedStore)) {
        request.result.createObjectStore(composeSeedStore);
      }
    };
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

export function onComposeTarget(
  handler: (seed: ComposeSeed) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  const token = new URLSearchParams(window.location.search).get("seedDbKey");
  if (!token) return Promise.resolve(() => undefined);
  return listen<ComposeWindowTarget>("compose-target", (event) => {
    if (
      event.payload?.label !== getCurrentWindow().label ||
      !composeWindowLabel.test(event.payload.label) ||
      event.payload?.seedToken !== token ||
      !isComposeSeed(event.payload.seed)
    ) {
      return;
    }
    const seed = withSendAttemptId(event.payload.seed, token);
    handler(seed);
  });
}

/** Acknowledges only the authenticated opener label placed in this URL by Rust. */
export async function notifyComposeWindowReady() {
  if (!isTauri()) return;
  const params = new URLSearchParams(window.location.search);
  const token = params.get("seedDbKey");
  const opener = params.get("opener");
  if (
    !token ||
    !opener ||
    !(opener === "main" || /^reader-[a-f0-9]{32}$/.test(opener))
  ) {
    return;
  }
  const label = getCurrentWindow().label;
  if (!composeWindowLabel.test(label)) return;
  await emitTo(opener, "compose-ready", {
    label,
    seedToken: token,
  });
}

export function onComposeSent(handler: () => void): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen("compose-sent", handler);
}

export type OutboxEvent =
  | { phase: "sending"; message: MailSummary }
  | { phase: "finished"; id: string };

export function onOutboxChanged(
  handler: (event: OutboxEvent) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<OutboxEvent>("outbox-changed", (event) =>
    handler(event.payload),
  );
}

export async function notifyOutbox(event: OutboxEvent) {
  if (!isTauri()) return;
  try {
    await emitTo("main", "outbox-changed", event);
  } catch (error) {
    // A missing or closing main window must never prevent SMTP delivery.
    console.warn("Could not update the Outbox", error);
  }
}

export async function closeComposeWindow(sent = false) {
  if (!isTauri()) {
    window.close();
    return;
  }
  if (sent) await emitTo("main", "compose-sent");
  await getCurrentWindow().close();
}
