import { describe, expect, it } from "vitest";
import { parseInertHtml } from "./inertHtml";

describe("inert HTML parsing", () => {
  it("preserves root language and direction attributes", () => {
    const document = parseInertHtml(
      '<html lang="ar" dir="rtl"><body><p>مرحبا</p></body></html>',
    );

    expect(document.documentElement.getAttribute("lang")).toBe("ar");
    expect(document.documentElement.getAttribute("dir")).toBe("rtl");
    expect(document.body.textContent).toContain("مرحبا");
  });

  it("keeps nested SVG titles and styles in their original structure", () => {
    const document = parseInertHtml(
      [
        "<html><head><style>.mail { color: red }</style></head><body>",
        '<svg aria-label="Chart"><title>Chart label</title><style>.bar { fill: blue }</style><rect class="bar"></rect></svg>',
        "</body></html>",
      ].join(""),
    );

    expect(document.head.querySelector("style")?.textContent).toContain(
      ".mail",
    );
    expect(document.body.querySelector("svg title")?.textContent).toBe(
      "Chart label",
    );
    expect(document.body.querySelector("svg style")?.textContent).toContain(
      ".bar",
    );
  });

  it("does not mistake tag-shaped script or comment text for body attributes", () => {
    for (const html of [
      [
        '<script>const sample = "</scriptx><body dir=rtl hidden>";</script>',
        "<!-- <body dir=rtl hidden> -->",
        "<p>Hello</p>",
      ].join(""),
      "<noscript><body dir=rtl hidden></noscript><p>Hello</p>",
      "<plaintext><body dir=rtl hidden><p>Hello</p>",
      "<svg><![CDATA[x > <body dir=rtl hidden>]]></svg><p>Hello</p>",
    ]) {
      const document = parseInertHtml(html);

      expect(document.body.hasAttribute("dir")).toBe(false);
      expect(document.body.hasAttribute("hidden")).toBe(false);
    }
  });

  it("merges attributes from concatenated document wrappers", () => {
    const document = parseInertHtml(
      [
        '<html lang="en"><body class="first"><p>First</p></body>',
        '<html dir="rtl" lang="et"><body data-second="true" class="later"><p>Second</p>',
      ].join(""),
    );

    expect(document.documentElement.getAttribute("lang")).toBe("en");
    expect(document.documentElement.getAttribute("dir")).toBe("rtl");
    expect(document.body.getAttribute("class")).toBe("first");
    expect(document.body.getAttribute("data-second")).toBe("true");
    expect(document.body.textContent).toContain("First");
    expect(document.body.textContent).toContain("Second");
  });

  it("recovers a malformed comment before a real body wrapper", () => {
    const document = parseInertHtml(
      "<!-- comment --!><body dir=rtl><p>Visible</p>",
    );

    expect(document.body.getAttribute("dir")).toBe("rtl");
    expect(document.body.textContent).toContain("Visible");
  });
});
