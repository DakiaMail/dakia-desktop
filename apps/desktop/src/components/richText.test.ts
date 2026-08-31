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

  it("preserves only the Thunderbird citation markers used by reply history", () => {
    expect(
      sanitizeRichText(
        '<div class="moz-cite-prefix" type="cite" data-test="remove">Citation</div><blockquote type="cite" class="remove">Original</blockquote>',
      ),
    ).toBe(
      '<div class="moz-cite-prefix">Citation</div><blockquote type="cite">Original</blockquote>',
    );
    expect(
      sanitizeRichText(
        '<div class="moz-cite-prefix extra">Citation</div><blockquote type="other" class="moz-cite-prefix">Original</blockquote>',
      ),
    ).toBe("<div>Citation</div><blockquote>Original</blockquote>");
  });

  it("preserves safe rich email layout only inside generated quoted history", () => {
    const quoted = [
      '<div data-dakia-quoted-email="true">',
      '<table width="600" cellpadding="0" style="width: 600px; background-color: rgb(255, 255, 255); position: fixed">',
      '<tbody><tr><td align="center" style="padding: 24px; color: rgb(36, 48, 44)">',
      '<img alt="GitHub" width="32" src="https://example.com/github.png" onerror="alert(1)">',
      '<a href="https://example.com/settings" style="display: inline-block; background-color: rgb(22, 136, 63); padding: 12px; color: white; position: fixed" onclick="alert(1)">Manage budgets</a>',
      "</td></tr></tbody></table><script>alert(1)</script></div>",
    ].join("");

    const sanitized = sanitizeRichText(quoted, {
      preserveQuotedEmail: true,
    });
    expect(sanitized).toContain('<table width="600" cellpadding="0"');
    expect(sanitized).toContain("background-color: rgb(255, 255, 255)");
    expect(sanitized).toContain(
      '<img alt="GitHub" width="32" src="https://example.com/github.png">',
    );
    expect(sanitized).toContain(
      'style="display: inline-block; background-color: rgb(22, 136, 63);',
    );
    expect(sanitized).toContain(
      'href="https://example.com/settings" rel="noreferrer noopener">Manage budgets</a>',
    );
    expect(sanitized).not.toContain("position");
    expect(sanitized).not.toContain("onerror");
    expect(sanitized).not.toContain("script");

    expect(sanitizeRichText(quoted)).toBe(
      '<div><a href="https://example.com/settings" rel="noreferrer noopener">Manage budgets</a></div>',
    );
  });

  it("creates a readable text alternative for structured content", () => {
    expect(
      plainTextFromRichText(
        "<p>Hello <strong>there</strong></p><ul><li>One</li><li>Two</li></ul><blockquote>Thanks</blockquote>",
      ),
    ).toBe("Hello there\n• One\n• Two\n> Thanks");
    expect(
      plainTextFromRichText(
        '<div data-dakia-quoted-email="true"><table><tbody><tr><td>Plan</td><td>0.5 GB</td></tr></tbody></table></div>',
      ),
    ).toBe("Plan\n0.5 GB");
  });

  it("prefixes non-empty lines within nested blockquotes by quote depth", () => {
    expect(
      plainTextFromRichText(
        "<p>Current reply</p><blockquote>First line<br><blockquote>Nested line<br>Nested second line</blockquote>Final line<br></blockquote>",
      ),
    ).toBe(
      [
        "Current reply",
        "> First line",
        "> > Nested line",
        "> > Nested second line",
        "> Final line",
      ].join("\n"),
    );
  });

  it("converts plain-text seeds without treating them as markup", () => {
    expect(richTextFromPlainText("<Hello>\nWorld")).toBe(
      "<p>&lt;Hello&gt;<br>World</p>",
    );
    expect(isRichTextEmpty("<p><br></p>")).toBe(true);
  });
});
