import { describe, expect, it } from "vitest";
import type { MailSummary } from "./types";
import { formatForwardHistory, forwardSubject } from "./forward";

const message: MailSummary = {
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
  snippet: "Preview",
  body_text: "Cached preview",
  is_read: true,
  is_flagged: false,
  has_attachments: false,
};

describe("email forwarding", () => {
  it("adds the forward prefix only once", () => {
    expect(forwardSubject("Release plan", "Fwd:")).toBe("Fwd: Release plan");
    expect(forwardSubject("fWd: Release plan", "Fwd:")).toBe(
      "fWd: Release plan",
    );
  });

  it("quotes the provider-loaded plain-text body with message metadata", () => {
    const history = formatForwardHistory(
      message,
      { body_text: "Full original body", attachments: [] },
      {
        originalMessage: "Original message",
        from: "From",
        date: "Date",
        subject: "Subject",
        to: "To",
      },
    );
    expect(history.body).toContain("---------- Original message ----------");
    expect(history.body).toContain("From: Mara <mara@example.com>");
    expect(history.body).toContain("Subject: Release plan");
    expect(history.body).toContain("Full original body");
    expect(history.body).not.toContain("Cached preview");
    expect(history.bodyHtml).toContain(
      "---------- Original message ----------",
    );
  });

  it("preserves the provider-loaded rich body in the HTML alternative", () => {
    const history = formatForwardHistory(
      message,
      {
        body_text: "GitHub Actions Usage Manage budgets",
        body_html:
          '<table style="width: 600px"><tbody><tr><td style="background-color: #ffffff"><img alt="GitHub" width="32" src="https://example.com/github.png"><a href="https://example.com/settings">Manage budgets</a></td></tr></tbody></table>',
        attachments: [],
      },
      {
        originalMessage: "Original message",
        from: "From",
        date: "Date",
        subject: "Subject",
        to: "To",
      },
    );

    expect(history.bodyHtml).toContain('data-dakia-quoted-email="true"');
    expect(history.bodyHtml).toContain('<table style="width: 600px">');
    expect(history.bodyHtml).toContain("Manage budgets");
  });
});
