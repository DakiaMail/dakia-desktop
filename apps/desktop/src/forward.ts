import type { MailSummary, MessageContent } from "./types";

export function forwardSubject(subject: string, prefix: string) {
  const trimmed = subject.trim();
  return trimmed.toLowerCase().startsWith(prefix.toLowerCase())
    ? trimmed
    : `${prefix} ${trimmed}`.trim();
}

export function forwardBody(
  message: MailSummary,
  content: MessageContent,
  labels: {
    originalMessage: string;
    from: string;
    date: string;
    subject: string;
    to: string;
  },
) {
  return [
    "",
    "",
    `---------- ${labels.originalMessage} ----------`,
    `${labels.from}: ${formatSender(message)}`,
    `${labels.date}: ${new Date(message.received_at).toLocaleString()}`,
    `${labels.subject}: ${message.subject}`,
    `${labels.to}: ${message.to_addresses}`,
    "",
    content.body_text,
  ].join("\n");
}

function formatSender(message: MailSummary) {
  return message.from_name
    ? `${message.from_name} <${message.from_address}>`
    : message.from_address;
}
