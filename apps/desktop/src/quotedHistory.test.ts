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
});
