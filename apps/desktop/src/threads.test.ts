import { describe, expect, it } from "vitest";
import type { MailSummary } from "./types";
import { groupMessages } from "./threads";

const message = (overrides: Partial<MailSummary>): MailSummary => ({
  id: "message-1",
  account_id: "account-1",
  mailbox: "INBOX",
  uid: 1,
  thread_id: "thread-1",
  subject: "Hello",
  from_address: "sender@example.com",
  to_addresses: "me@example.com",
  received_at: "2026-07-19T10:00:00Z",
  snippet: "Preview",
  body_text: "Body",
  is_read: true,
  is_flagged: false,
  has_attachments: false,
  ...overrides,
});

describe("groupMessages", () => {
  it("groups a thread, orders its messages, and puts the latest thread first", () => {
    const threads = groupMessages([
      message({
        id: "reply",
        uid: 2,
        received_at: "2026-07-19T11:00:00Z",
        is_read: false,
      }),
      message({ id: "root", uid: 1, received_at: "2026-07-19T09:00:00Z" }),
      message({
        id: "other",
        uid: 3,
        thread_id: "thread-2",
        received_at: "2026-07-19T10:30:00Z",
      }),
    ]);

    expect(threads.map((thread) => thread.latest.id)).toEqual([
      "reply",
      "other",
    ]);
    expect(threads[0].messages.map((item) => item.id)).toEqual([
      "root",
      "reply",
    ]);
    expect(threads[0].unread).toBe(true);
  });

  it("does not merge equal thread ids from different accounts", () => {
    expect(
      groupMessages([
        message({ id: "one" }),
        message({ id: "two", account_id: "account-2" }),
      ]),
    ).toHaveLength(2);
  });
});
