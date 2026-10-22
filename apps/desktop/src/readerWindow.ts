import { emitTo, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { getAllWebviewWindows } from "@tauri-apps/api/webviewWindow";
import { showNativeMessage } from "./nativeFeedback";
import type { ConversationTarget } from "./types";

export type { ConversationTarget } from "./types";

export type ReaderWindowSeed = {
  target: ConversationTarget;
  focusedMessageId?: string;
};

export type ReaderWindowMutation = {
  accountId: string;
  threadId?: string;
  messageIds: string[];
  mutation:
    | "archive"
    | "spam"
    | "not_spam"
    | "trash"
    | "delete"
    | "read"
    | "star"
    | "unsubscribe";
};
export type ReaderWindowFailure = { accountId: string };
type ReaderWindowReady = { label: string; seedToken: string };
type ReaderWindowTargetEvent = {
  label: string;
  seed: ReaderWindowSeed;
  seedToken?: string;
};
const readerWindowReadyTimeoutMs = 5_000;

const readerSeedStoragePrefix = "dakia.reader-seed.";
const activeReaderSeedKey = "dakia.reader-active-seed";
const readerWindowOpenOperations = new Map<string, Promise<void>>();
const readerWindowOpeningLabels = new Set<string>();

const isTauri = () => "__TAURI_INTERNALS__" in window;

/**
 * A label is intentionally derived only from a stable, non-PII digest. Tauri
 * labels are restrictive, while account/thread identifiers are not.
 */
export async function readerWindowLabel(target: ConversationTarget) {
  const identity = `${target.accountId}\u0000${target.threadId ?? target.localMessageId ?? target.rfcMessageId ?? ""}`;
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(identity),
  );
  const hash = [...new Uint8Array(digest)]
    .slice(0, 16)
    .map((byte) => byte.toString(16).padStart(2, "0"))
    .join("");
  return `reader-${hash}`;
}

function storeReaderSeed(seed: ReaderWindowSeed) {
  try {
    const token = crypto.randomUUID();
    window.localStorage.setItem(
      `${readerSeedStoragePrefix}${token}`,
      JSON.stringify(seed),
    );
    return token;
  } catch {
    return undefined;
  }
}

export function readReaderSeed(): ReaderWindowSeed | undefined {
  const token = new URLSearchParams(window.location.search).get("seedKey");
  try {
    const key = token ? `${readerSeedStoragePrefix}${token}` : undefined;
    const localValue = key ? window.localStorage.getItem(key) : null;
    const value =
      localValue ?? window.sessionStorage.getItem(activeReaderSeedKey);
    if (key) window.localStorage.removeItem(key);
    if (!value) return undefined;
    const stored = JSON.parse(value) as unknown;
    const sessionSeed =
      !localValue && isTauri()
        ? readNativeRememberedReaderSeed(stored)
        : normalizeReaderSeed(stored);
    if (!sessionSeed) return undefined;
    const seed = sessionSeed;
    if (!isConversationTarget(seed.target)) return undefined;
    const validSeed = {
      target: seed.target,
      focusedMessageId:
        typeof seed.focusedMessageId === "string"
          ? seed.focusedMessageId
          : undefined,
    };
    if (localValue) rememberReaderSeed(validSeed);
    return validSeed;
  } catch {
    return undefined;
  }
}

export function hasNativeReaderSeedHandoff() {
  if (
    !isTauri() ||
    !new URLSearchParams(window.location.search).get("seedDbKey")
  ) {
    return false;
  }
  try {
    const remembered = window.sessionStorage.getItem(activeReaderSeedKey);
    return !readNativeRememberedReaderSeed(
      remembered ? JSON.parse(remembered) : undefined,
    );
  } catch {
    return true;
  }
}

function isConversationTarget(value: unknown): value is ConversationTarget {
  if (!value || typeof value !== "object") return false;
  const target = value as Partial<ConversationTarget>;
  return Boolean(
    typeof target.accountId === "string" &&
    target.accountId &&
    (typeof target.threadId === "string" ||
      typeof target.localMessageId === "string" ||
      typeof target.rfcMessageId === "string"),
  );
}

/** Retain a validated target for a normal Reader reload when storage permits. */
export function rememberReaderSeed(seed: ReaderWindowSeed) {
  try {
    const stored = isTauri() ? { label: getCurrentWindow().label, seed } : seed;
    window.sessionStorage.setItem(activeReaderSeedKey, JSON.stringify(stored));
  } catch {
    // Session storage is reload convenience, not a condition for reading mail.
  }
}

function readNativeRememberedReaderSeed(value: unknown) {
  if (!value || typeof value !== "object" || Array.isArray(value))
    return undefined;
  const stored = value as { label?: unknown; seed?: unknown };
  if (
    typeof stored.label !== "string" ||
    stored.label !== getCurrentWindow().label
  ) {
    return undefined;
  }
  return normalizeReaderSeed(stored.seed);
}

export async function openReaderWindow(seed: ReaderWindowSeed): Promise<void> {
  const label = await readerWindowLabel(seed.target);
  if (!isTauri()) {
    const token = storeReaderSeed(seed);
    if (!token) throw new Error("Could not create reader window seed");
    const opened = window.open(
      `/?${new URLSearchParams({ view: "reader", seedKey: token }).toString()}`,
      label,
      "popup=yes,width=980,height=760,resizable=yes,scrollbars=no",
    );
    if (!opened) {
      window.localStorage.removeItem(`${readerSeedStoragePrefix}${token}`);
      throw new Error("Could not open reader window");
    }
    return;
  }

  return queueReaderWindowOpen(label, seed);
}

function queueReaderWindowOpen(label: string, seed: ReaderWindowSeed) {
  const previous = readerWindowOpenOperations.get(label) ?? Promise.resolve();
  const queued = previous
    .catch(() => undefined)
    .then(() => openNativeReaderWindow(label, seed));
  readerWindowOpenOperations.set(label, queued);
  void queued.then(
    () => {
      if (readerWindowOpenOperations.get(label) === queued) {
        readerWindowOpenOperations.delete(label);
        readerWindowOpeningLabels.delete(label);
      }
    },
    () => {
      if (readerWindowOpenOperations.get(label) === queued) {
        readerWindowOpenOperations.delete(label);
        readerWindowOpeningLabels.delete(label);
      }
    },
  );
  return queued;
}

async function openNativeReaderWindow(label: string, seed: ReaderWindowSeed) {
  const existing = (await getAllWebviewWindows()).find(
    (candidate) => candidate.label === label,
  );
  if (existing || readerWindowOpeningLabels.has(label)) {
    if (existing) {
      await existing.show();
      await existing.setFocus();
    }
    await emitTo(label, "reader-target", { label, seed });
    return;
  }

  let ready: { wait: Promise<void>; dispose: UnlistenFn } | undefined;
  try {
    const token = crypto.randomUUID();
    ready = await waitForReaderWindowReady(label, token);
    const createWindow = invoke("open_reader_window", {
      label,
      seedKey: null,
      seedDbKey: token,
    });
    await Promise.all([createWindow, ready.wait]);
    await emitTo(label, "reader-target", { label, seedToken: token, seed });
    readerWindowOpeningLabels.add(label);
  } catch (error) {
    await reportReaderSeedFailure();
    throw new Error(`Could not open reader window: ${String(error)}`);
  } finally {
    ready?.dispose();
  }
}

async function waitForReaderWindowReady(label: string, seedToken: string) {
  let resolveReady: () => void = () => undefined;
  let rejectReady: (reason: Error) => void = () => undefined;
  const wait = new Promise<void>((resolve, reject) => {
    resolveReady = resolve;
    rejectReady = reject;
  });
  let closed = false;
  let unlisten: UnlistenFn = () => undefined;
  const timeout = window.setTimeout(() => {
    rejectReady(new Error("Reader window did not become ready."));
  }, readerWindowReadyTimeoutMs);
  const registration = listen<ReaderWindowReady>("reader-ready", (event) => {
    if (
      event.payload?.label === label &&
      event.payload.seedToken === seedToken
    ) {
      resolveReady();
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
    wait: wait.finally(() => window.clearTimeout(timeout)),
    dispose: () => {
      closed = true;
      unlisten();
    },
  };
}

async function reportReaderSeedFailure() {
  try {
    await showNativeMessage(
      "Could not open this message",
      "Try again.",
      "error",
    );
  } catch {
    // The failed storage handoff is already contained: no Reader was opened.
  }
}

function normalizeReaderSeed(value: unknown): ReaderWindowSeed | undefined {
  if (!value || typeof value !== "object") return undefined;
  const seed = value as Partial<ReaderWindowSeed>;
  if (!isConversationTarget(seed.target)) return undefined;
  return {
    target: seed.target,
    focusedMessageId:
      typeof seed.focusedMessageId === "string"
        ? seed.focusedMessageId
        : undefined,
  };
}

export function onReaderTarget(
  handler: (seed: ReaderWindowSeed) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  const startupToken = new URLSearchParams(window.location.search).get(
    "seedDbKey",
  );
  let targetGeneration = 0;
  return listen<ReaderWindowTargetEvent>("reader-target", (event) => {
    const seed = normalizeReaderSeed(event.payload?.seed);
    if (!seed) return;
    const generation = ++targetGeneration;
    void readerWindowLabel(seed.target).then((expectedLabel) => {
      if (generation !== targetGeneration) return;
      if (
        expectedLabel !== getCurrentWindow().label ||
        event.payload?.label !== expectedLabel
      ) {
        return;
      }
      if (
        startupToken &&
        event.payload.seedToken !== undefined &&
        event.payload.seedToken !== startupToken
      ) {
        return;
      }
      if (
        startupToken &&
        hasNativeReaderSeedHandoff() &&
        event.payload.seedToken !== startupToken
      ) {
        return;
      }
      rememberReaderSeed(seed);
      handler(seed);
    });
  });
}

/** Tell the opener that the Reader target subscription is ready for retargets. */
export async function notifyReaderWindowReady() {
  if (!isTauri()) return;
  const params = new URLSearchParams(window.location.search);
  const seedKey = params.get("seedKey");
  const seedDbKey = params.get("seedDbKey");
  const seedToken = seedDbKey ?? seedKey;
  if (!seedToken || (seedKey && seedDbKey)) return;
  await emitTo("main", "reader-ready", {
    label: getCurrentWindow().label,
    seedToken,
  });
}

export async function notifyReaderWindowMutated(
  mutation: ReaderWindowMutation,
) {
  if (!isTauri()) return;
  try {
    await emitTo("main", "reader-window-mutated", mutation);
  } catch (error) {
    // The mailbox mutation has already succeeded; a closing main window must
    // not turn it into a reader-window failure.
    console.warn("Could not refresh the main mailbox window", error);
  }
}

export function onReaderWindowMutated(
  handler: (mutation: ReaderWindowMutation) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<ReaderWindowMutation>("reader-window-mutated", (event) =>
    handler(event.payload),
  );
}

export async function notifyReaderWindowFailed(failure: ReaderWindowFailure) {
  if (!isTauri()) return;
  await emitTo("main", "reader-window-failed", failure);
}

export function onReaderWindowFailed(
  handler: (failure: ReaderWindowFailure) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<ReaderWindowFailure>("reader-window-failed", (event) =>
    handler(event.payload),
  );
}

export async function closeReaderWindow() {
  if (isTauri()) await getCurrentWindow().close();
  else window.close();
}
