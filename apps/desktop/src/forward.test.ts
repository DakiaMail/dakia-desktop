import { describe, expect, it } from "vitest";
import type { MailSummary } from "./types";
import { forwardBody, forwardSubject } from "./forward";

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
    const body = forwardBody(
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
    expect(body).toContain("---------- Original message ----------");
    expect(body).toContain("From: Mara <mara@example.com>");
    expect(body).toContain("Subject: Release plan");
    expect(body).toContain("Full original body");
    expect(body).not.toContain("Cached preview");
  });
});
