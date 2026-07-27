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
        (left, right) =>
          new Date(left.received_at).getTime() -
          new Date(right.received_at).getTime(),
      );
      const latest = deduplicated[deduplicated.length - 1];
      return {
        id,
        messages: deduplicated,
        latest,
        unread: deduplicated.some((message) => !message.is_read),
        hasAttachments: deduplicated.some((message) => message.has_attachments),
        participants: unique(
          deduplicated.map(
            (message) => message.from_name || message.from_address,
          ),
        ),
      };
    })
    .sort(
      (left, right) =>
        new Date(right.latest.received_at).getTime() -
        new Date(left.latest.received_at).getTime(),
    );
}

function deduplicateCopies(messages: MailSummary[]) {
  const seen = new Set<string>();
  return messages.filter((message) => {
    const key = message.message_id?.toLowerCase() || message.id;
    if (seen.has(key)) return false;
    seen.add(key);
    return true;
  });
}

function unique(values: string[]) {
  return [...new Set(values)];
}
