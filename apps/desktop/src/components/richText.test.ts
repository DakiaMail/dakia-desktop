import { describe, expect, it } from "vitest";
import {
  isRichTextEmpty,
  plainTextFromRichText,
  richTextFromPlainText,
  sanitizeRichText,
} from "./richText";

describe("rich text conversion", () => {
  it("keeps supported formatting and removes active content and inline images", () => {
    expect(
      sanitizeRichText(
        '<p>Hello <strong>there</strong><script>alert(1)</script><img src="data:image/png;base64,x"><a href="javascript:alert(1)">bad</a></p>',
      ),
    ).toBe("<p>Hello <strong>there</strong>bad</p>");
    expect(sanitizeRichText('<a href="example.com">Read more</a>')).toBe(
      '<a href="https://example.com/" rel="noreferrer noopener">Read more</a>',
    );
    expect(
      sanitizeRichText(
        '<b>Bold</b><span style="font-style: italic; color: red">Italic</span>',
      ),
    ).toBe('<b>Bold</b><span style="font-style: italic">Italic</span>');
  });

  it("creates a readable text alternative for structured content", () => {
    expect(
      plainTextFromRichText(
        "<p>Hello <strong>there</strong></p><ul><li>One</li><li>Two</li></ul><blockquote>Thanks</blockquote>",
      ),
    ).toBe("Hello there\n• One\n• Two\nThanks");
  });

  it("converts plain-text seeds without treating them as markup", () => {
    expect(richTextFromPlainText("<Hello>\nWorld")).toBe(
      "<p>&lt;Hello&gt;<br>World</p>",
    );
    expect(isRichTextEmpty("<p><br></p>")).toBe(true);
  });
});
