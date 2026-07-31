import type { MailSummary, MailThread } from "./types";

export function groupMessages(messages: MailSummary[]): MailThread[] {
  const grouped = new Map<string, MailSummary[]>();
  for (const message of messages) {
    const key = `${message.account_id}:${message.thread_id || message.id}`;
    grouped.set(key, [...(grouped.get(key) ?? []), message]);
  }

  return [...grouped.entries()]
    .map(([id, threadMessages]) => {
      const deduplicated = deduplicateCopies(threadMessages).sort(
        compareMessagesByReceivedAt,
      );
      const latest = deduplicated[deduplicated.length - 1];
      const sourceMessages = [...threadMessages].sort(
        compareMessagesByReceivedAt,
      );
      return {
        id,
        messages: deduplicated,
        sourceMessages,
        latest,
        unread: sourceMessages.some((message) => !message.is_read),
        hasAttachments: sourceMessages.some(
          (message) => message.has_attachments,
        ),
        participants: unique(
          sourceMessages.map(
            (message) => message.from_name || message.from_address,
          ),
        ),
      };
    })
    .sort((left, right) => {
      const latestComparison = compareMessagesByReceivedAt(
        right.latest,
        left.latest,
      );
      return latestComparison || compareStrings(left.id, right.id);
    });
}

function deduplicateCopies(messages: MailSummary[]): MailSummary[] {
  const copiesByMessageId = new Map<string, MailSummary[]>();
  for (const message of messages) {
    const key = message.message_id?.toLowerCase() || message.id;
    copiesByMessageId.set(key, [
      ...(copiesByMessageId.get(key) ?? []),
      message,
    ]);
  }

  return [...copiesByMessageId.values()].map(mergeCopies);
}

/**
 * A duplicate's newest valid received time is its canonical display
 * representation. Concrete copies remain on sourceMessages so actions never
 * lose the mailbox/UID locator or manufacture state on the winning row.
 */
function mergeCopies(copies: MailSummary[]): MailSummary {
  return [...copies].sort(compareMessagesByReceivedAt).at(-1)!;
}

export function concreteThreadMessages(thread: MailThread): MailSummary[] {
  return thread.sourceMessages ?? thread.messages;
}

function compareMessagesByReceivedAt(
  left: MailSummary,
  right: MailSummary,
): number {
  const receivedAtComparison =
    receivedAtMilliseconds(left) - receivedAtMilliseconds(right);
  if (receivedAtComparison) return receivedAtComparison;

  const messageIdComparison = compareStrings(
    left.message_id?.toLowerCase() ?? "",
    right.message_id?.toLowerCase() ?? "",
  );
  return messageIdComparison || compareStrings(left.id, right.id);
}

function receivedAtMilliseconds(message: MailSummary): number {
  const value = new Date(message.received_at).getTime();
  return Number.isFinite(value) ? value : Number.NEGATIVE_INFINITY;
}

function compareStrings(left: string, right: string): number {
  if (left < right) return -1;
  if (left > right) return 1;
  return 0;
}

function unique(values: string[]) {
  return [...new Set(values)].sort(compareStrings);
}
