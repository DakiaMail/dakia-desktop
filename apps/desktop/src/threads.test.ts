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

  it("is invariant while retaining concrete duplicate locators without synthetic row state", () => {
    const messages = permutationFixture(17);
    const expected = groupMessages(messages);

    for (let permutation = 0; permutation < 48; permutation += 1) {
      expect(groupMessages(shuffle(messages, 1000 + permutation))).toEqual(
        expected,
      );
    }

    const sharedThread = expected.find(
      (thread) => thread.id === "account-1:shared-thread",
    )!;
    expect(expected.map((thread) => thread.latest.id)).toEqual([
      "other-thread-17",
      "account-2-copy-17",
      "shared-tie-b-17",
    ]);
    expect(sharedThread.messages.map((item) => item.id)).toEqual([
      "shared-early-17",
      "shared-copy-new-17",
      "shared-tie-a-17",
      "shared-tie-b-17",
    ]);
    expect(sharedThread.messages[1]).toMatchObject({
      id: "shared-copy-new-17",
      is_read: true,
      has_attachments: false,
    });
    expect(sharedThread.sourceMessages?.map((item) => item.id)).toContain(
      "shared-copy-old-17",
    );
    expect(sharedThread.unread).toBe(true);
    expect(sharedThread.hasAttachments).toBe(true);
    expect(sharedThread.participants).toEqual(["Amy Example", "Zoe Example"]);
    expect(
      expected.find((thread) => thread.id === "account-2:shared-thread"),
    ).toMatchObject({
      messages: [{ id: "account-2-copy-17" }],
    });
  });

  it("keeps grouping, deduplication, ordering, and aggregates invariant across fixed seeds", () => {
    for (const seed of [1, 19, 73, 211]) {
      const messages = permutationFixture(seed);
      const expected = groupMessages(messages);

      for (let permutation = 0; permutation < 24; permutation += 1) {
        expect(
          groupMessages(shuffle(messages, seed * 100 + permutation)),
        ).toEqual(expected);
      }
    }
  });
});

function permutationFixture(seed: number): MailSummary[] {
  const suffix = String(seed);
  return [
    message({
      id: `shared-copy-old-${suffix}`,
      uid: 10,
      thread_id: "shared-thread",
      message_id: `<COPY-${suffix}@example.com>`,
      from_name: "Zoe Example",
      received_at: "2026-07-19T10:00:00Z",
      is_read: false,
      has_attachments: true,
    }),
    message({
      id: `shared-copy-new-${suffix}`,
      uid: 11,
      thread_id: "shared-thread",
      message_id: `<copy-${suffix}@example.com>`,
      from_name: "Zoe Example",
      received_at: "2026-07-19T12:00:00Z",
      is_read: true,
      has_attachments: false,
    }),
    message({
      id: `shared-early-${suffix}`,
      uid: 12,
      thread_id: "shared-thread",
      message_id: `<early-${suffix}@example.com>`,
      from_name: "Amy Example",
      received_at: "2026-07-19T09:00:00Z",
    }),
    message({
      id: `shared-tie-b-${suffix}`,
      uid: 13,
      thread_id: "shared-thread",
      message_id: `<tie-b-${suffix}@example.com>`,
      from_name: "Amy Example",
      received_at: "2026-07-19T13:00:00Z",
    }),
    message({
      id: `shared-tie-a-${suffix}`,
      uid: 14,
      thread_id: "shared-thread",
      message_id: `<tie-a-${suffix}@example.com>`,
      from_name: "Amy Example",
      received_at: "2026-07-19T13:00:00Z",
    }),
    message({
      id: `account-2-copy-${suffix}`,
      account_id: "account-2",
      uid: 15,
      thread_id: "shared-thread",
      message_id: `<COPY-${suffix}@example.com>`,
      received_at: "2026-07-19T14:00:00Z",
    }),
    message({
      id: `other-thread-${suffix}`,
      uid: 16,
      thread_id: "other-thread",
      message_id: `<other-${suffix}@example.com>`,
      received_at: "2026-07-19T15:00:00Z",
    }),
  ];
}

function shuffle<T>(values: readonly T[], seed: number): T[] {
  const result = [...values];
  let state = seed >>> 0;
  const next = () => {
    state ^= state << 13;
    state ^= state >>> 17;
    state ^= state << 5;
    return state >>> 0;
  };

  for (let index = result.length - 1; index > 0; index -= 1) {
    const other = next() % (index + 1);
    [result[index], result[other]] = [result[other], result[index]];
  }
  return result;
}
