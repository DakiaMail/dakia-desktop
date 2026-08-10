import { emitTo, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  getAllWebviewWindows,
  WebviewWindow,
} from "@tauri-apps/api/webviewWindow";
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

const readerSeedStoragePrefix = "dakia.reader-seed.";
const activeReaderSeedKey = "dakia.reader-active-seed";

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
    const value =
      (key ? window.localStorage.getItem(key) : null) ??
      window.sessionStorage.getItem(activeReaderSeedKey);
    if (key) window.localStorage.removeItem(key);
    if (!value) return undefined;
    const seed = JSON.parse(value) as Partial<ReaderWindowSeed>;
    if (!isConversationTarget(seed.target)) return undefined;
    const validSeed = {
      target: seed.target,
      focusedMessageId:
        typeof seed.focusedMessageId === "string"
          ? seed.focusedMessageId
          : undefined,
    };
    window.sessionStorage.setItem(
      activeReaderSeedKey,
      JSON.stringify(validSeed),
    );
    return validSeed;
  } catch {
    return undefined;
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

  const existing = (await getAllWebviewWindows()).find(
    (candidate) => candidate.label === label,
  );
  if (existing) {
    await existing.show();
    await existing.setFocus();
    await emitTo(label, "reader-target", seed);
    return;
  }

  const token = storeReaderSeed(seed);
  if (!token) throw new Error("Could not create reader window seed");
  await new Promise<void>((resolve, reject) => {
    const readerWindow = new WebviewWindow(label, {
      url: `/?${new URLSearchParams({ view: "reader", seedKey: token }).toString()}`,
      title: "Dakia",
      width: 980,
      height: 760,
      minWidth: 600,
      minHeight: 480,
      center: true,
      focus: true,
      resizable: true,
      decorations: true,
      shadow: true,
      titleBarStyle: "overlay",
      hiddenTitle: true,
    });
    readerWindow.once("tauri://created", () => resolve());
    readerWindow.once("tauri://error", (event) => {
      window.localStorage.removeItem(`${readerSeedStoragePrefix}${token}`);
      reject(
        new Error(`Could not open reader window: ${String(event.payload)}`),
      );
    });
  });
}

export function onReaderTarget(
  handler: (seed: ReaderWindowSeed) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return Promise.resolve(() => undefined);
  return listen<ReaderWindowSeed>("reader-target", (event) => {
    if (isConversationTarget(event.payload.target)) {
      window.sessionStorage.setItem(
        activeReaderSeedKey,
        JSON.stringify(event.payload),
      );
      handler(event.payload);
    }
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
