import type { MailSummary } from "./types";

type ReplyCitation = {
  readonly date: string;
  readonly sender: string;
};

type ReplyHistoryInput = {
  readonly message: Pick<
    MailSummary,
    "from_address" | "from_name" | "received_at"
  >;
  readonly bodyText: string;
  readonly bodyHtml?: string | null;
  readonly formatCitation: (citation: ReplyCitation) => string;
};

export function formatReplyHistory({
  message,
  bodyText,
  bodyHtml,
  formatCitation,
}: ReplyHistoryInput) {
  const citation = formatCitation({
    date: new Date(message.received_at).toLocaleString(),
    sender: formatSender(message),
  });
  const normalizedBody = bodyText.replace(/\r\n?/g, "\n");
  const quotedBody = normalizedBody.split("\n").map((line) => `> ${line}`);

  return {
    body: [citation, ...quotedBody].join("\n"),
    bodyHtml: [
      "<p><br></p>",
      `<div class="moz-cite-prefix">${escapeHtml(citation)}</div>`,
      `<blockquote type="cite">${
        bodyHtml
          ? `<div data-dakia-quoted-email="true">${bodyHtml}</div>`
          : escapeHtml(normalizedBody).replaceAll("\n", "<br>")
      }</blockquote>`,
    ].join(""),
  };
}

function formatSender(
  message: Pick<MailSummary, "from_address" | "from_name">,
) {
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
