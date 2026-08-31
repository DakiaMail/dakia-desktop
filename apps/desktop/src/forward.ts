import type { MailSummary, MessageContent } from "./types";

export function forwardSubject(subject: string, prefix: string) {
  const trimmed = subject.trim();
  return trimmed.toLowerCase().startsWith(prefix.toLowerCase())
    ? trimmed
    : `${prefix} ${trimmed}`.trim();
}

export function formatForwardHistory(
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
  const headerLines = [
    `---------- ${labels.originalMessage} ----------`,
    `${labels.from}: ${formatSender(message)}`,
    `${labels.date}: ${new Date(message.received_at).toLocaleString()}`,
    `${labels.subject}: ${message.subject}`,
    `${labels.to}: ${message.to_addresses}`,
  ];
  return {
    body: ["", "", ...headerLines, "", content.body_text].join("\n"),
    bodyHtml: [
      "<p><br></p>",
      `<div>${headerLines.map(escapeHtml).join("<br>")}</div>`,
      "<br>",
      content.body_html
        ? `<div data-dakia-quoted-email="true">${content.body_html}</div>`
        : `<div>${escapeHtml(content.body_text).replace(/\r\n?/g, "\n").replaceAll("\n", "<br>")}</div>`,
    ].join(""),
  };
}

function formatSender(message: MailSummary) {
  return message.from_name
    ? `${message.from_name} <${message.from_address}>`
    : message.from_address;
}

function escapeHtml(value: string) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}
