import { describe, expect, it } from "vitest";
import { splitQuotedText } from "./quotedHistory";

describe("splitQuotedText", () => {
  it("collapses a strong wrote-and-quote block", () => {
    expect(
      splitQuotedText("New answer\n\nOn Monday, Pat wrote:\n> Old\n> Text"),
    ).toEqual({
      visible: "New answer",
      history: "On Monday, Pat wrote:\n> Old\n> Text",
    });
  });

  it("collapses original-message separators and coherent quote runs", () => {
    expect(splitQuotedText("Reply\n--- Original Message ---\nOld")).toEqual({
      visible: "Reply",
      history: "--- Original Message ---\nOld",
    });
    expect(splitQuotedText("Reply\n\n> Old\n> Text")).toEqual({
      visible: "Reply",
      history: "> Old\n> Text",
    });
    expect(splitQuotedText("Reply\r\nBegin forwarded message:\r\nOld")).toEqual(
      {
        visible: "Reply",
        history: "Begin forwarded message:\nOld",
      },
    );
  });

  it("leaves signatures and uncertain quoting visible", () => {
    const signature = "Thanks\n-- \nPat";
    const prose = "Use > to compare values.\nStill authored.";
    const shortQuote = "Reply\n> One cited line";
    expect(splitQuotedText(signature)).toEqual({ visible: signature });
    expect(splitQuotedText(prose)).toEqual({ visible: prose });
    expect(splitQuotedText(shortQuote)).toEqual({ visible: shortQuote });
  });

  it("collapses an Outlook desktop reply header block", () => {
    const text = [
      "Uus vastus",
      "",
      "Juhan Tamm",
      "",
      "From: Marten Mets <marten.mets@example.com>",
      "Sent: Friday, July 24, 2026 10:24 PM",
      "To: Juhan Tamm <juhan.tamm@example.com>",
      "Subject: RE: Kahjuteade VO1000042",
      "",
      "Varasem sisu",
      "On 24 Jul 2026 at 15:59 +0300, juhan.tamm@example.com, wrote:",
      "Veel vanem sisu",
    ].join("\n");
    expect(splitQuotedText(text)).toEqual({
      visible: "Uus vastus\n\nJuhan Tamm",
      history: [
        "From: Marten Mets <marten.mets@example.com>",
        "Sent: Friday, July 24, 2026 10:24 PM",
        "To: Juhan Tamm <juhan.tamm@example.com>",
        "Subject: RE: Kahjuteade VO1000042",
        "",
        "Varasem sisu",
        "On 24 Jul 2026 at 15:59 +0300, juhan.tamm@example.com, wrote:",
        "Veel vanem sisu",
      ].join("\n"),
    });
  });

  it("does not split on a From: line without adjacent mail header labels", () => {
    const prose = [
      "Reply",
      "From: the letters of the archive we quote",
      "a plain body line",
      "another plain body line",
    ].join("\n");
    const blankBreak = [
      "Reply",
      "From: a standalone mention",
      "",
      "To: a label separated by a blank line is not a header",
    ].join("\n");
    expect(splitQuotedText(prose)).toEqual({ visible: prose });
    expect(splitQuotedText(blankBreak)).toEqual({ visible: blankBreak });
  });
});
