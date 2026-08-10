import { describe, expect, it } from "vitest";
import { buildNewMailNotification } from "./notifications";
import type { MailSummary } from "./types";

const copy = {
  newMail: "New email",
  oneGeneric: "You have a new message",
  many: (count: number) => `${count} new emails`,
  manyBody: (count: number) => `${count} messages arrived`,
};

const message = (id: string): MailSummary => ({
  id,
  account_id: "account",
  mailbox: "INBOX",
  uid: Number(id),
  thread_id: id,
  subject: `Subject ${id}`,
  from_name: "Mara",
  from_address: "mara@example.com",
  to_addresses: "me@example.com",
  received_at: "2026-07-19T10:00:00Z",
  snippet: "Preview",
  body_text: "Body",
  is_read: false,
  is_flagged: false,
  has_attachments: false,
});

describe("new-mail notification copy", () => {
  it("shows the sender and subject for one message", () => {
    expect(
      buildNewMailNotification(
        [{ ...message("1"), message_id: "<message-1@example.com>" }],
        true,
        copy,
      ),
    ).toMatchObject({
      title: "Mara",
      body: "Subject 1",
      extra: {
        accountId: "account",
        messageId: "1",
        rfcMessageId: "<message-1@example.com>",
        threadId: "1",
        count: 1,
      },
    });
  });

  it("hides message metadata when previews are disabled", () => {
    expect(buildNewMailNotification([message("1")], false, copy)).toMatchObject(
      { title: "New email", body: "You have a new message" },
    );
  });

  it("batches several messages into one summary", () => {
    expect(
      buildNewMailNotification([message("1"), message("2")], true, copy),
    ).toEqual({
      title: "2 new emails",
      body: "2 messages arrived",
      extra: { count: 2 },
    });
  });
});
