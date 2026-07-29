import { emitTo, listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { WebviewWindow } from "@tauri-apps/api/webviewWindow";
import type { ComposeAttachment, MailSummary } from "./types";

export type ComposeSeed = {
  accountId?: string;
  to?: string;
  cc?: string;
  subject?: string;
  body?: string;
  bodyHtml?: string;
  inReplyTo?: string;
  references?: string;
  contextMessageIds?: string[];
  forwardMessageId?: string;
  attachments?: ComposeAttachment[];
};

const isTauri = () => "__TAURI_INTERNALS__" in window;
const composeSeedStoragePrefix = "dakia.compose-seed.";

function parseComposeSeed(value: string | null): ComposeSeed | undefined {
  if (!value) return undefined;
  try {
    return JSON.parse(value) as ComposeSeed;
  } catch {
    return undefined;
  }
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

function consumeStoredComposeSeed(token: string) {
  try {
    const key = `${composeSeedStoragePrefix}${token}`;
    const value = localStorage.getItem(key);
    localStorage.removeItem(key);
    return parseComposeSeed(value);
  } catch {
    return undefined;
  }
}

export function readComposeSeed(): ComposeSeed {
  const params = new URLSearchParams(window.location.search);
  const token = params.get("seedKey");
  if (token) {
    const stored = consumeStoredComposeSeed(token);
    if (stored) return stored;
  }
  return parseComposeSeed(params.get("seed")) ?? {};
}

export function openComposeWindow(seed: ComposeSeed) {
  const params = new URLSearchParams({ view: "compose" });
  const storedSeedToken = storeComposeSeed(seed);
  if (storedSeedToken) params.set("seedKey", storedSeedToken);
  else params.set("seed", JSON.stringify(seed));
  const url = `/?${params.toString()}`;

  if (!isTauri()) {
    window.open(
      url,
      "_blank",
      "popup=yes,width=880,height=700,resizable=yes,scrollbars=no",
    );
    return;
  }

  const composeWindow = new WebviewWindow(`compose-${crypto.randomUUID()}`, {
    url,
    title: "New message",
    width: 880,
    height: 700,
    minWidth: 620,
    minHeight: 480,
    center: true,
    focus: true,
    resizable: true,
    decorations: true,
    titleBarStyle: "overlay",
    hiddenTitle: true,
    shadow: true,
  });
  composeWindow.once("tauri://error", (event) => {
    console.error("Could not open compose window", event.payload);
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
