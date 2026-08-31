import { describe, expect, it } from "vitest";
import { formatReplyHistory } from "./replyHistory";
import type { MailSummary } from "./types";

const message = {
  id: "message-1",
  account_id: "account-1",
  mailbox: "INBOX",
  uid: 1,
  thread_id: "thread-1",
  subject: "Release plan",
  from_name: "Mara",
  from_address: "mara@example.com",
  to_addresses: "me@example.com",
  received_at: "2026-07-19T10:00:00Z",
  snippet: "Untrusted summary preview",
  body_text: "Cached summary body",
  is_read: true,
  is_flagged: false,
  has_attachments: false,
} satisfies MailSummary;

describe("reply history", () => {
  it("formats provider plain text as Thunderbird-compatible plain and HTML quotes", () => {
    const quote = formatReplyHistory({
      message,
      bodyText: "First line\r\n> Earlier history\r\n<script>&",
      formatCitation: ({ date, sender }) => `On ${date}, ${sender} wrote:`,
    });
    const date = new Date(message.received_at).toLocaleString();

    expect(quote.body).toBe(
      [
        `On ${date}, Mara <mara@example.com> wrote:`,
        "> First line",
        "> > Earlier history",
        "> <script>&",
      ].join("\n"),
    );
    expect(quote.bodyHtml).toBe(
      [
        "<p><br></p>",
        `<div class="moz-cite-prefix">On ${date}, Mara &lt;mara@example.com&gt; wrote:</div>`,
        '<blockquote type="cite">First line<br>&gt; Earlier history<br>&lt;script&gt;&amp;</blockquote>',
      ].join(""),
    );
  });

  it("uses the sender address when the reply message has no display name", () => {
    const { from_name: _fromName, ...messageWithoutName } = message;
    const quote = formatReplyHistory({
      message: messageWithoutName,
      bodyText: "Original body",
      formatCitation: ({ sender }) => `Citation: ${sender}`,
    });

    expect(quote.body).toContain("Citation: mara@example.com");
    expect(quote.bodyHtml).toContain("Citation: mara@example.com");
  });

  it("quotes the original rich HTML without replacing it with flattened text", () => {
    const quote = formatReplyHistory({
      message,
      bodyText: "GitHub Actions Usage Manage budgets",
      bodyHtml:
        '<table style="width: 600px"><tbody><tr><td><img alt="GitHub" src="https://example.com/github.png"><a href="https://example.com/settings">Manage budgets</a></td></tr></tbody></table>',
      formatCitation: ({ sender }) => `${sender} wrote:`,
    });

    expect(quote.body).toContain("> GitHub Actions Usage Manage budgets");
    expect(quote.bodyHtml).toContain(
      '<blockquote type="cite"><div data-dakia-quoted-email="true"><table',
    );
    expect(quote.bodyHtml).toContain("Manage budgets</a>");
    expect(quote.bodyHtml).not.toContain("> GitHub Actions Usage");
  });
});
