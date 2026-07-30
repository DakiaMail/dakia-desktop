import { describe, expect, it } from "vitest";
import { groupMessages } from "./threads";
import type { MailSummary } from "./types";

const baseMessage = (overrides: Partial<MailSummary>): MailSummary => ({
  id: "message-1",
  account_id: "account-1",
  mailbox: "INBOX",
  uid: 1,
  message_id: null,
  thread_id: "shared-thread",
  subject: "Subject",
  from_name: null,
  from_address: "sender@example.test",
  to_addresses: "reader@example.test",
  received_at: "2026-07-30T10:00:00Z",
  snippet: "Snippet",
  body_text: "Body",
  is_read: true,
  is_flagged: false,
  has_attachments: false,
  ...overrides,
});

describe("groupMessages fixed-seed invariants", () => {
  it("is permutation-invariant for invalid dates, null Message-IDs, duplicate copies, and accounts", () => {
    for (const seed of [0x1, 0x51f15e, 0xc0ffee, 0xffffffff]) {
      const graph = generatedThreadGraph(seed);
      const canonical = groupMessages(graph);

      for (let permutation = 0; permutation < 32; permutation += 1) {
        expect(groupMessages(shuffle(graph, seed ^ permutation))).toEqual(
          canonical,
        );
      }

      expect(canonical).toHaveLength(5);
      expect(
        canonical.filter((thread) => thread.id.endsWith(":shared-thread")),
      ).toHaveLength(2);

      const primary = canonical.find(
        (thread) => thread.id === "account-1:shared-thread",
      )!;
      expect(primary.messages.map(({ id }) => id)).toEqual([
        `invalid-empty-${seed}`,
        `invalid-text-${seed}`,
        `null-a-${seed}`,
        `null-b-${seed}`,
        `duplicate-new-${seed}`,
        `valid-latest-${seed}`,
      ]);
      expect(primary.messages[4]).toMatchObject({
        is_read: true,
        has_attachments: false,
      });
      expect(primary.sourceMessages).toEqual(
        expect.arrayContaining([
          expect.objectContaining({ id: `duplicate-old-${seed}` }),
          expect.objectContaining({ id: `duplicate-new-${seed}` }),
        ]),
      );
      expect(primary.unread).toBe(true);
      expect(primary.hasAttachments).toBe(true);
      expect(primary.participants).toEqual([
        "Alpha Example",
        "Beta Example",
        "sender@example.test",
      ]);
    }
  });

  it("never collapses distinct null Message-ID messages and orders malformed dates deterministically", () => {
    for (const seed of [3, 17, 101, 4099]) {
      const nullCopies = Array.from({ length: 20 }, (_, index) =>
        baseMessage({
          id: `null-${seed}-${index.toString().padStart(2, "0")}`,
          uid: index + 1,
          message_id: null,
          received_at: invalidDate(index),
        }),
      );

      const expectedIds = nullCopies
        .map(({ id }) => id)
        .sort((left, right) => left.localeCompare(right));

      for (let permutation = 0; permutation < 16; permutation += 1) {
        const [thread] = groupMessages(
          shuffle(nullCopies, seed * 100 + permutation),
        );
        expect(thread.messages.map(({ id }) => id)).toEqual(expectedIds);
        expect(thread.latest.id).toBe(expectedIds.at(-1));
      }
    }
  });
});

function generatedThreadGraph(seed: number): MailSummary[] {
  return [
    baseMessage({
      id: `invalid-empty-${seed}`,
      uid: 1,
      message_id: `<invalid-empty-${seed}@example.test>`,
      received_at: "",
    }),
    baseMessage({
      id: `invalid-text-${seed}`,
      uid: 2,
      message_id: `<invalid-text-${seed}@example.test>`,
      received_at: "not-a-date",
      from_name: "Beta Example",
    }),
    baseMessage({
      id: `null-a-${seed}`,
      uid: 3,
      message_id: null,
      received_at: "2026-07-30T08:00:00Z",
      from_name: "Alpha Example",
    }),
    baseMessage({
      id: `null-b-${seed}`,
      uid: 4,
      message_id: null,
      received_at: "2026-07-30T09:00:00Z",
      from_name: "Alpha Example",
    }),
    baseMessage({
      id: `duplicate-old-${seed}`,
      uid: 5,
      message_id: `<DuPlIcAtE-${seed}@example.test>`,
      received_at: "2026-07-30T07:00:00Z",
      is_read: false,
      has_attachments: true,
    }),
    baseMessage({
      id: `duplicate-new-${seed}`,
      uid: 6,
      mailbox: "Archive",
      message_id: `<duplicate-${seed}@example.test>`,
      received_at: "2026-07-30T10:00:00Z",
    }),
    baseMessage({
      id: `valid-latest-${seed}`,
      uid: 7,
      message_id: `<latest-${seed}@example.test>`,
      received_at: "2026-07-30T11:00:00Z",
    }),
    baseMessage({
      id: `account-2-copy-${seed}`,
      account_id: "account-2",
      uid: 8,
      message_id: `<duplicate-${seed}@example.test>`,
      received_at: "2026-07-30T12:00:00Z",
    }),
    baseMessage({
      id: `fallback-a-${seed}`,
      uid: 9,
      thread_id: "",
      message_id: null,
      received_at: "2026-07-30T13:00:00Z",
    }),
    baseMessage({
      id: `fallback-b-${seed}`,
      uid: 10,
      thread_id: "",
      message_id: null,
      received_at: "2026-07-30T14:00:00Z",
    }),
    baseMessage({
      id: `other-thread-${seed}`,
      uid: 11,
      thread_id: "other-thread",
      message_id: null,
      received_at: "2026-07-30T15:00:00Z",
    }),
  ];
}

function invalidDate(index: number): string {
  return ["", "not-a-date", "2026-99-99", "NaN", "invalid"][index % 5];
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
