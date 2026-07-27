import type { MailSummary, MailThread } from "./types";

export type MailAction = "archive" | "spam" | "not_spam" | "trash";
export type MailActionPhase = "exiting" | "restoring";

export type PendingMailAction = {
  action: MailAction;
  phase: MailActionPhase;
  delay: number;
};

export type PendingMailActions = Record<string, PendingMailAction>;

export function mailboxFamily(mailbox: string) {
  return mailbox.split("::", 1)[0];
}

export function conversationActionMessages(
  thread: MailThread,
  view: string,
  action: MailAction,
) {
  if (action === "archive") {
    // Archiving removes only the Inbox label/copy. Sent history and already
    // archived members are reader context, not action targets.
    return thread.messages.filter(
      (message) => mailboxFamily(message.mailbox) === "INBOX",
    );
  }
  if (action === "not_spam") {
    return thread.messages.filter(
      (message) => mailboxFamily(message.mailbox) === "Spam",
    );
  }
  if (action === "trash") {
    return thread.messages.filter(
      (message) =>
        !["Drafts", "Trash"].includes(mailboxFamily(message.mailbox)),
    );
  }
  if (view === "INBOX") {
    return thread.messages.filter(
      (message) => mailboxFamily(message.mailbox) === "INBOX",
    );
  }
  if (view && !["unread", "starred"].includes(view)) {
    return thread.messages.filter(
      (message) =>
        mailboxFamily(message.mailbox) === view &&
        !["Sent", "Drafts"].includes(mailboxFamily(message.mailbox)),
    );
  }
  return thread.messages.filter(
    (message) =>
      !["Sent", "Drafts", "Spam", "Trash"].includes(
        mailboxFamily(message.mailbox),
      ),
  );
}

export function nextMessageAfterAction(
  messages: MailSummary[],
  active: MailSummary | undefined,
  removedIds: Set<string>,
) {
  if (!active || !removedIds.has(active.id)) return active;
  const activeIndex = messages.findIndex((message) => message.id === active.id);
  if (activeIndex < 0) return undefined;

  for (let index = activeIndex + 1; index < messages.length; index += 1) {
    if (!removedIds.has(messages[index].id)) return messages[index];
  }
  for (let index = activeIndex - 1; index >= 0; index -= 1) {
    if (!removedIds.has(messages[index].id)) return messages[index];
  }
  return undefined;
}

export function restoreMessages(
  current: MailSummary[],
  original: MailSummary[],
  restoredIds: Set<string>,
) {
  const byId = new Map(current.map((message) => [message.id, message]));
  for (const message of original) {
    if (restoredIds.has(message.id)) byId.set(message.id, message);
  }

  const originalOrder = new Map(
    original.map((message, index) => [message.id, index]),
  );
  return [...byId.values()].sort((left, right) => {
    const leftIndex = originalOrder.get(left.id);
    const rightIndex = originalOrder.get(right.id);
    if (leftIndex === undefined && rightIndex === undefined) return 0;
    if (leftIndex === undefined) return 1;
    if (rightIndex === undefined) return -1;
    return leftIndex - rightIndex;
  });
}

export function restoreThreads(
  current: MailThread[],
  original: MailThread[],
  restoredIds: Set<string>,
) {
  const byId = new Map(current.map((thread) => [thread.id, thread]));
  for (const thread of original) {
    if (restoredIds.has(thread.id)) byId.set(thread.id, thread);
  }
  const originalOrder = new Map(
    original.map((thread, index) => [thread.id, index]),
  );
  return [...byId.values()].sort(
    (left, right) =>
      (originalOrder.get(left.id) ?? Number.MAX_SAFE_INTEGER) -
      (originalOrder.get(right.id) ?? Number.MAX_SAFE_INTEGER),
  );
}
