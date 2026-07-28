import { describe, expect, it } from "vitest";
import type { MailSummary } from "./types";
import {
  messageRecipients,
  parseAddressList,
  replyRecipients,
} from "./recipients";

const message = {
  from_name: "Sender",
  from_address: "sender@example.com",
  to_addresses: 'Me <me@example.com>, "Doe, Jane" <jane@example.com>',
  cc_addresses: "Other <other@example.com>, JANE@example.com",
  bcc_addresses: "Hidden <hidden@example.com>",
  reply_to_addresses: "Replies <reply@example.com>",
} as MailSummary;

describe("recipient parsing and replies", () => {
  it("parses quoted display names, groups, and truthful empty fields", () => {
    expect(
      parseAddressList(
        'Friends: "Doe, Jane" <jane@example.com>, John <john@example.com>;',
      ),
    ).toEqual([
      { name: "Doe, Jane", address: "jane@example.com" },
      { name: "John", address: "john@example.com" },
    ]);
    expect(parseAddressList(undefined)).toEqual([]);
    expect(parseAddressList("support@example.com (Ticket queue)")).toEqual([
      { name: "Ticket queue", address: "support@example.com" },
    ]);
    expect(parseAddressList("admin@[IPv6:2001:db8::1]")).toEqual([
      { address: "admin@[IPv6:2001:db8::1]" },
    ]);
    expect(messageRecipients(message).bcc).toEqual([
      { name: "Hidden", address: "hidden@example.com" },
    ]);
  });

  it("uses Reply-To for Reply", () => {
    expect(replyRecipients(message, "me@example.com")).toEqual({
      to: "Replies <reply@example.com>",
      cc: "",
    });
  });

  it("preserves every valid Reply-To mailbox", () => {
    const multiple = {
      ...message,
      reply_to_addresses:
        "Replies <reply@example.com>, Support <support@example.com>",
    };
    expect(replyRecipients(multiple, "me@example.com")).toEqual({
      to: "Replies <reply@example.com>, Support <support@example.com>",
      cc: "",
    });
    expect(replyRecipients(multiple, "me@example.com", true)).toEqual({
      to: 'Replies <reply@example.com>, Support <support@example.com>, "Doe, Jane" <jane@example.com>',
      cc: "Other <other@example.com>",
    });
  });

  it("builds Reply All without self, Bcc, or duplicate recipients", () => {
    expect(replyRecipients(message, "ME@example.com", true)).toEqual({
      to: 'Replies <reply@example.com>, "Doe, Jane" <jane@example.com>',
      cc: "Other <other@example.com>",
    });
  });
});
