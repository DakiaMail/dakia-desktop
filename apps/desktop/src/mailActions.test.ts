import { describe, expect, it } from "vitest";
import {
  conversationActionMessages,
  nextMessageAfterAction,
  removeConcreteMessage,
  restoreMessages,
  restoreThreads,
} from "./mailActions";
import type { MailSummary, MailThread } from "./types";

const message = (id: string): MailSummary => ({
  id,
  account_id: "account",
  mailbox: "INBOX",
  uid: Number(id.replace(/\D/g, "")) || 1,
  thread_id: id,
  subject: `Subject ${id}`,
  from_address: "sender@example.com",
  to_addresses: "me@example.com",
  received_at: "2026-07-19T10:00:00Z",
  snippet: "Preview",
  body_text: "Body",
  is_read: false,
  is_flagged: false,
  has_attachments: false,
});

describe("mail action state", () => {
  const messages = [message("1"), message("2"), message("3")];

  it("opens the next remaining message", () => {
    expect(
      nextMessageAfterAction(messages, messages[1], new Set(["2"])),
    ).toEqual(messages[2]);
  });

  it("falls back to the previous message at the end", () => {
    expect(
      nextMessageAfterAction(messages, messages[2], new Set(["3"])),
    ).toEqual(messages[1]);
  });

  it("skips every message in a bulk action", () => {
    expect(
      nextMessageAfterAction(messages, messages[0], new Set(["1", "2"])),
    ).toEqual(messages[2]);
  });

  it("restores only failed messages in their original positions", () => {
    expect(
      restoreMessages([messages[2]], messages, new Set(["1"])).map(
        (item) => item.id,
      ),
    ).toEqual(["1", "3"]);
  });

  it("archives only Inbox members of a hydrated conversation", () => {
    const inbox = message("1");
    const sent = { ...message("2"), mailbox: "Sent" };
    const thread: MailThread = {
      id: "account:thread",
      messages: [inbox, sent],
      latest: sent,
      unread: true,
      hasAttachments: false,
      participants: [],
    };
    expect(
      conversationActionMessages(thread, "INBOX", "archive").map(
        (item) => item.mailbox,
      ),
    ).toEqual(["INBOX"]);
  });

  it("preserves hidden duplicate locators as action targets", () => {
    const visibleArchive = {
      ...message("visible"),
      mailbox: "Archive",
      message_id: "<same@example.test>",
    };
    const hiddenInbox = {
      ...message("hidden"),
      mailbox: "INBOX",
      message_id: "<same@example.test>",
    };
    const thread: MailThread = {
      id: "account:thread",
      messages: [visibleArchive],
      sourceMessages: [hiddenInbox, visibleArchive],
      latest: visibleArchive,
      unread: false,
      hasAttachments: false,
      participants: [],
    };
    expect(
      conversationActionMessages(thread, "INBOX", "archive").map(
        (item) => item.id,
      ),
    ).toEqual(["hidden"]);
  });

  it("treats discovered Sent folders as Sent without acting on them", () => {
    const inbox = message("1");
    const sent = { ...message("2"), mailbox: "Sent::Sent Messages" };
    const thread: MailThread = {
      id: "account:thread",
      messages: [inbox, sent],
      latest: sent,
      unread: true,
      hasAttachments: false,
      participants: [],
    };
    expect(
      conversationActionMessages(thread, "INBOX", "spam").map(
        (item) => item.mailbox,
      ),
    ).toEqual(["INBOX"]);
  });

  it("restores only Spam members to Inbox", () => {
    const spam = { ...message("1"), mailbox: "Spam::Junk" };
    const sent = { ...message("2"), mailbox: "Sent" };
    const thread: MailThread = {
      id: "account:thread",
      messages: [spam, sent],
      latest: spam,
      unread: true,
      hasAttachments: false,
      participants: [],
    };
    expect(
      conversationActionMessages(thread, "Spam", "not_spam").map(
        (item) => item.id,
      ),
    ).toEqual(["1"]);
  });

  it("moves ordinary conversation members to Trash but leaves drafts alone", () => {
    const inbox = message("1");
    const draft = { ...message("2"), mailbox: "Drafts" };
    const thread: MailThread = {
      id: "account:thread",
      messages: [inbox, draft],
      latest: inbox,
      unread: true,
      hasAttachments: false,
      participants: [],
    };
    expect(
      conversationActionMessages(thread, "INBOX", "trash").map(
        (item) => item.id,
      ),
    ).toEqual(["1"]);
  });

  it("removes only the selected mailbox and UID copy from a conversation", () => {
    const inboxCopy = {
      ...message("inbox-copy"),
      mailbox: "INBOX",
      uid: 8,
      thread_id: "thread",
      message_id: "<duplicate@example.test>",
    };
    const archiveCopy = {
      ...message("archive-copy"),
      mailbox: "Archive",
      uid: 12,
      thread_id: "thread",
      message_id: "<duplicate@example.test>",
    };
    const sibling = {
      ...message("sibling"),
      uid: 13,
      thread_id: "thread",
      received_at: "2026-07-19T11:00:00Z",
    };
    const thread = {
      id: "account:thread",
      messages: [inboxCopy, sibling],
      sourceMessages: [inboxCopy, archiveCopy, sibling],
      latest: sibling,
      unread: true,
      hasAttachments: false,
      participants: [],
    } satisfies MailThread;

    const remaining = removeConcreteMessage(thread, inboxCopy)!;

    expect(remaining.sourceMessages).toEqual([archiveCopy, sibling]);
    expect(remaining.messages).toEqual([archiveCopy, sibling]);
    expect(remaining.latest).toEqual(sibling);
  });

  it("restores a failed conversation once regardless of message count", () => {
    const first: MailThread = {
      id: "first",
      messages: [message("1"), message("2")],
      latest: message("2"),
      unread: true,
      hasAttachments: false,
      participants: [],
    };
    const second: MailThread = {
      ...first,
      id: "second",
      messages: [message("3")],
      latest: message("3"),
    };
    expect(
      restoreThreads([second], [first, second], new Set(["first"])),
    ).toEqual([first, second]);
  });

  it("preserves list order when concurrent failures restore from different snapshots", () => {
    const threads = ["first", "second", "third"].map((id, index) => ({
      id,
      messages: [message(String(index + 1))],
      latest: message(String(index + 1)),
      unread: true,
      hasAttachments: false,
      participants: [],
    })) satisfies MailThread[];

    const afterFirstFailure = restoreThreads(
      [threads[2]],
      threads,
      new Set(["first"]),
    );
    expect(
      restoreThreads(afterFirstFailure, threads.slice(1), new Set(["second"])),
    ).toEqual(threads);
  });
});
