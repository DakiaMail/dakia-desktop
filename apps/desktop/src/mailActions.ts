import type { MailSummary, MailThread } from "./types";
import { concreteThreadMessages, groupMessages } from "./threads";

// Conversation actions deliberately exclude permanent deletion. That operation
// is only available for one expanded Reader message, never a list, bulk, or
// native-menu target. Shift+Delete is handled by Reader against that concrete
// expanded message and therefore does not belong in this conversation union.
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
  const messages = concreteThreadMessages(thread);
  if (action === "archive") {
    // Archiving removes only the Inbox label/copy. Sent history and already
    // archived members are reader context, not action targets.
    return messages.filter(
      (message) => mailboxFamily(message.mailbox) === "INBOX",
    );
  }
  if (action === "not_spam") {
    return messages.filter(
      (message) => mailboxFamily(message.mailbox) === "Spam",
    );
  }
  if (action === "trash") {
    return messages.filter(
      (message) =>
        !["Drafts", "Trash"].includes(mailboxFamily(message.mailbox)),
    );
  }
  if (view === "INBOX") {
    return messages.filter(
      (message) => mailboxFamily(message.mailbox) === "INBOX",
    );
  }
  if (view && !["unread", "starred"].includes(view)) {
    return messages.filter(
      (message) =>
        mailboxFamily(message.mailbox) === view &&
        !["Sent", "Drafts"].includes(mailboxFamily(message.mailbox)),
    );
  }
  return messages.filter(
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

/**
 * Mailbox and UID identify an IMAP message copy. Message-ID is deliberately
 * excluded: a conversation can contain copies of the same RFC message in
 * several mailboxes, and a permanent delete must affect only the selected
 * copy.
 */
export function sameMessageLocator(left: MailSummary, right: MailSummary) {
  return (
    left.account_id === right.account_id &&
    left.mailbox === right.mailbox &&
    left.uid === right.uid
  );
}

/**
 * Removes one concrete IMAP copy while rebuilding the derived conversation
 * fields (deduplicated display rows, latest message, unread state, and
 * participants) from its remaining copies.
 */
export function removeConcreteMessage(
  thread: MailThread,
  target: MailSummary,
): MailThread | undefined {
  const sourceMessages = concreteThreadMessages(thread);
  const remaining = sourceMessages.filter(
    (message) => !sameMessageLocator(message, target),
  );
  if (remaining.length === sourceMessages.length) return thread;
  if (!remaining.length) return undefined;

  return { ...thread, ...groupMessages(remaining)[0] };
}

export function removeConcreteMessageFromThreads(
  threads: MailThread[],
  target: MailSummary,
) {
  return threads.flatMap((thread) => {
    const remaining = removeConcreteMessage(thread, target);
    return remaining ? [remaining] : [];
  });
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
  const next = [...current];
  const originalIds = original.map((thread) => thread.id);
  for (const thread of original) {
    if (!restoredIds.has(thread.id)) continue;
    const existing = next.findIndex((item) => item.id === thread.id);
    if (existing >= 0) {
      next[existing] = thread;
      continue;
    }

    const originalIndex = originalIds.indexOf(thread.id);
    const successor = originalIds
      .slice(originalIndex + 1)
      .map((id) => next.findIndex((item) => item.id === id))
      .find((index) => index >= 0);
    if (successor !== undefined) {
      next.splice(successor, 0, thread);
      continue;
    }
    const predecessor = originalIds
      .slice(0, originalIndex)
      .reverse()
      .map((id) => next.findIndex((item) => item.id === id))
      .find((index) => index >= 0);
    next.splice(
      predecessor === undefined ? next.length : predecessor + 1,
      0,
      thread,
    );
  }
  return next;
}
