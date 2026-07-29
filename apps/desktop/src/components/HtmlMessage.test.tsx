import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import appleMailReplySection from "../test/fixtures/apple-mail-reply-section.html?raw";
import outlookWordReplySection from "../test/fixtures/outlook-word-reply-section.html?raw";
import { buildEmailDocument, HtmlMessage } from "./HtmlMessage";

function renderedEmailRoot(message: HTMLElement) {
  const surface = message.shadowRoot?.firstElementChild as HTMLElement | null;
  return surface?.shadowRoot ?? null;
}

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
        '<svg><a href="#safe"><animate attributeName="href" values="#safe;javascript:alert(1)"></animate><set attributeName="href" to="https://evil.example"></set><text>Open</text></a></svg>',
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
      document.querySelectorAll(
        "script, iframe, object, form, input, base, animate, set",
      ),
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
    expect(document.querySelector("[attributeName]")).toBeNull();
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

  it("renders sanitized email in a layout-contained shadow tree", () => {
    render(
      <HtmlMessage
        html="<script>bad()</script><p>Hello</p>"
        title="Secure message"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );

    const message = screen.getByRole("document", { name: "Secure message" });
    expect(message.tagName).toBe("DIV");
    expect(message.shadowRoot).not.toBeNull();
    expect(renderedEmailRoot(message)).not.toBeNull();
    expect(renderedEmailRoot(message)?.querySelector("script")).toBeNull();
    expect(message.style.contain).toBe("layout paint");
    expect(message.style.height).toBe("");
    expect(document.querySelector("iframe")).toBeNull();
  });

  it("collapses recognized quoted history without treating generic quotes as history", () => {
    const gmail = buildEmailDocument(
      '<p>Fresh</p><div class="gmail_quote"><script>bad()</script><p>Old</p></div>',
      false,
      { show: "Näita ajalugu", hide: "Peida ajalugu" },
    );
    const gmailDocument = new DOMParser().parseFromString(
      gmail.source,
      "text/html",
    );
    const details = gmailDocument.querySelector("details.dakia-quoted-history");
    expect(details).not.toBeNull();
    expect(details?.hasAttribute("open")).toBe(false);
    expect(details?.textContent).toContain("Näita ajalugu");
    expect(details?.textContent).toContain("Peida ajalugu");
    expect(details?.querySelector("script")).toBeNull();

    const generic = buildEmailDocument(
      "<p>Fresh</p><blockquote>A deliberate quotation</blockquote>",
      false,
    );
    const genericDocument = new DOMParser().parseFromString(
      generic.source,
      "text/html",
    );
    expect(
      genericDocument.querySelector("details.dakia-quoted-history"),
    ).toBeNull();
  });

  it("does not show history for empty or citation-only provider containers", () => {
    for (const html of [
      '<p>Fresh</p><div class="gmail_quote"> \n </div>',
      '<p>Fresh</p><div class="gmail_quote">On 19 Feb 2026 at 21:00 +0200, Romario Verbran &lt;romario@example.com&gt;, wrote:</div>',
      '<p>Fresh</p><blockquote type="cite">On 19 Feb 2026 at 21:00 +0200, Romario Verbran &lt;romario@example.com&gt;, wrote:</blockquote>',
    ]) {
      const email = buildEmailDocument(html, false);
      const document = new DOMParser().parseFromString(
        email.source,
        "text/html",
      );
      expect(document.querySelector("details.dakia-quoted-history")).toBeNull();
    }
  });

  it("moves an adjacent wrote citation into the collapsed history", () => {
    const email = buildEmailDocument(
      [
        "<p>Current reply</p>",
        "<div>On 19 Feb 2026 at 21:00 +0200, Romario Verbran &lt;romario.verbran@gmail.com&gt;, wrote:</div>",
        '<blockquote type="cite"><p>Earlier message</p></blockquote>',
      ].join(""),
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");
    const details = document.querySelector("details.dakia-quoted-history");

    expect(document.body.firstElementChild?.textContent).toBe("Current reply");
    expect(details?.textContent).toContain(
      "On 19 Feb 2026 at 21:00 +0200, Romario Verbran",
    );
    expect(details?.textContent).toContain("Earlier message");
    expect(
      document.body.textContent?.replace(details?.textContent ?? "", ""),
    ).not.toContain("wrote:");
  });

  it("collapses Apple Mail reply sections with their attribution", () => {
    const email = buildEmailDocument(
      [
        "<p>Current reply</p>",
        '<div name="messageReplySection">',
        "On 19 Feb 2026 at 21:00 +0200, Romario Verbran &lt;romario.verbran@gmail.com&gt;, wrote:<br>",
        '<blockquote type="cite"><div>Earlier message</div></blockquote>',
        "</div>",
      ].join(""),
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");
    const details = document.querySelector("details.dakia-quoted-history");

    expect(details?.textContent).toContain(
      "On 19 Feb 2026 at 21:00 +0200, Romario Verbran",
    );
    expect(details?.textContent).toContain("Earlier message");
    expect(
      document.body.querySelector(":scope > [name='messageReplySection']"),
    ).toBeNull();
  });

  it("does not show history for an empty Apple Mail reply section", () => {
    const email = buildEmailDocument(
      [
        "<p>Current reply</p>",
        '<div name="messageReplySection">',
        "On 19 Feb 2026 at 21:00 +0200, Romario Verbran &lt;romario@example.com&gt;, wrote:<br>",
        '<blockquote type="cite"><div name="messageBodySection"></div></blockquote>',
        "</div>",
      ].join(""),
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");

    expect(document.querySelector("details.dakia-quoted-history")).toBeNull();
  });

  it("captures Outlook history from its separator through the remaining siblings", () => {
    const email = buildEmailDocument(
      '<p>Fresh</p><div id="divRplyFwdMsg">From: Old Sender</div><blockquote>Old body</blockquote><p>Authored footer</p>',
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");
    const details = document.querySelector("details.dakia-quoted-history");
    expect(details?.textContent).toContain("From: Old Sender");
    expect(details?.textContent).toContain("Old body");
    expect(details?.textContent).not.toContain("Authored footer");
    expect(document.body.textContent).toContain("Authored footer");
    expect(document.body.firstElementChild?.textContent).toBe("Fresh");
  });

  it("collapses Word-generated Outlook history from its reply header through the quoted chain", () => {
    const email = buildEmailDocument(outlookWordReplySection, false);
    const document = new DOMParser().parseFromString(email.source, "text/html");
    const disclosures = document.querySelectorAll("details.dakia-quoted-history");
    expect(disclosures).toHaveLength(1);
    const details = disclosures[0];
    expect(details.hasAttribute("open")).toBe(false);

    const visible =
      document.body.textContent?.replace(details.textContent ?? "", "") ?? "";
    expect(visible).toContain("uus vastus, mis peab nähtavaks jääma");
    expect(visible).toContain("Juhan Tamm");
    expect(visible).not.toContain("From:");
    expect(visible).not.toContain("External email");
    expect(visible).not.toContain("wrote:");

    expect(details.textContent).toContain("From: Marten Mets");
    expect(details.textContent).toContain("Sent: Friday, July 24, 2026");
    expect(details.textContent).toContain("External email.");
    expect(details.textContent).toContain("varasema päeva sõnum, mis kuulub ajalukku");
    expect(details.textContent).toContain("wrote:");
    expect(details.textContent).toContain("-----Original Message-----");
    expect(details.textContent).toContain("Kõige vanem sõnum");
    expect(
      details.querySelector("[name='messageBodySection']"),
    ).not.toBeNull();
    expect(
      details.querySelector("[name='messageSignatureSection']"),
    ).not.toBeNull();
    expect(
      details.querySelector("[name='messageReplySection']"),
    ).not.toBeNull();
  });

  it("expands and collapses Word-generated Outlook history repeatedly", () => {
    render(
      <HtmlMessage
        html={outlookWordReplySection}
        title="Outlook history sizing"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );

    const message = screen.getByRole("document", {
      name: "Outlook history sizing",
    });
    const root = renderedEmailRoot(message);
    expect(root?.textContent).toContain("uus vastus, mis peab nähtavaks jääma");
    const details = root?.querySelector<HTMLDetailsElement>(
      "details.dakia-quoted-history",
    );
    expect(details).not.toBeNull();
    if (!details) return;
    expect(details.open).toBe(false);

    for (let cycle = 0; cycle < 2; cycle += 1) {
      fireEvent.click(details.querySelector("summary")!);
      expect(details.open).toBe(true);
      expect(details.textContent).toContain("Kõige vanem sõnum");

      fireEvent.click(details.querySelector("summary")!);
      expect(details.open).toBe(false);
    }
  });

  it("does not collapse a Word reply header that has no quoted content after it", () => {
    const email = buildEmailDocument(
      [
        "<p>Reply</p>",
        '<div><div style="border:none;border-top:solid #E1E1E1 1.0pt;padding:3.0pt 0cm 0cm 0cm">',
        "<p><b>From:</b> Old Sender &lt;old@example.test&gt;<br>",
        "<b>Sent:</b> Friday, July 24, 2026 10:24 PM<br>",
        "<b>To:</b> New Sender<br>",
        "<b>Subject:</b> RE: Empty quote</p>",
        "</div></div>",
        "<p>&nbsp;</p>",
      ].join(""),
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");
    expect(document.querySelector("details.dakia-quoted-history")).toBeNull();
    expect(document.body.textContent).toContain("From: Old Sender");
  });

  it("does not treat a bare border-top divider as a reply header", () => {
    const email = buildEmailDocument(
      [
        "<p>Above the divider</p>",
        '<div style="border:none;border-top:solid #E1E1E1 1.0pt;padding:3.0pt 0cm 0cm 0cm">',
        "<p>Quarterly report highlights</p>",
        "</div>",
        "<p>Below the divider</p>",
      ].join(""),
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");
    expect(document.querySelector("details.dakia-quoted-history")).toBeNull();
    expect(document.body.textContent).toContain("Below the divider");
  });

  it("collapses stacked Word reply headers into a single disclosure", () => {
    const email = buildEmailDocument(
      [
        "<p>Fresh</p>",
        '<div><div style="border:none;border-top:solid #E1E1E1 1.0pt;padding:3.0pt 0cm 0cm 0cm">',
        "<p><b>From:</b> One<br><b>Sent:</b> Monday<br><b>To:</b> Two<br><b>Subject:</b> RE: A</p>",
        "</div></div>",
        "<p>First quoted body</p>",
        '<div><div style="border:none;border-top:solid #E1E1E1 1.0pt;padding:3.0pt 0cm 0cm 0cm">',
        "<p><b>From:</b> Two<br><b>Sent:</b> Tuesday<br><b>To:</b> One<br><b>Subject:</b> RE: A</p>",
        "</div></div>",
        "<p>Second quoted body</p>",
      ].join(""),
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");
    const disclosures = document.querySelectorAll("details.dakia-quoted-history");
    expect(disclosures).toHaveLength(1);
    const details = disclosures[0];
    const visible =
      document.body.textContent?.replace(details.textContent ?? "", "") ?? "";
    expect(visible).toContain("Fresh");
    expect(visible).not.toContain("quoted body");
    expect(details.textContent).toContain("First quoted body");
    expect(details.textContent).toContain("Second quoted body");
  });

  it("keeps the trusted history disclosure visible against hostile message CSS", () => {
    const email = buildEmailDocument(
      '<style>summary { display:none!important; visibility:hidden!important }</style><p>Fresh</p><div class="gmail_quote">Old</div>',
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");
    const summary = document.querySelector<HTMLElement>("details > summary");
    expect(summary?.style.getPropertyValue("display")).toBe("list-item");
    expect(summary?.style.getPropertyPriority("display")).toBe("important");
    expect(summary?.style.getPropertyValue("visibility")).toBe("visible");
    expect(summary?.style.getPropertyPriority("visibility")).toBe("important");
    expect(summary?.style.getPropertyValue("all")).toBe("revert");
    expect(summary?.style.getPropertyPriority("all")).toBe("important");
    expect(summary?.style.getPropertyValue("position")).toBe("static");
    expect(summary?.style.getPropertyPriority("position")).toBe("important");
    expect(summary?.style.getPropertyValue("transform")).toBe("none");
    expect(summary?.style.getPropertyPriority("transform")).toBe("important");
  });

  it("uses normal document flow through repeated history toggles", () => {
    render(
      <HtmlMessage
        html={appleMailReplySection}
        title="Apple Mail history sizing"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );

    const message = screen.getByRole("document", {
      name: "Apple Mail history sizing",
    });
    const details = renderedEmailRoot(
      message,
    )?.querySelector<HTMLDetailsElement>("details.dakia-quoted-history");
    expect(details).not.toBeNull();
    if (!details) return;
    expect(details.open).toBe(false);
    expect(message.style.height).toBe("");

    for (let cycle = 0; cycle < 2; cycle += 1) {
      fireEvent.click(details.querySelector("summary")!);
      expect(details.open).toBe(true);
      expect(message.style.height).toBe("");
      expect(details.textContent).toContain(
        "The earlier message belongs inside history.",
      );

      fireEvent.click(details.querySelector("summary")!);
      expect(details.open).toBe(false);
      expect(message.style.height).toBe("");
    }
  });

  it("keeps external links usable only through Dakia's system-browser handler", () => {
    const email = buildEmailDocument(
      '<a href="https://example.test/details">Open details</a>',
      false,
    );
    const document = new DOMParser().parseFromString(email.source, "text/html");
    const anchor = document.querySelector("a");

    expect(anchor?.getAttribute("href")).toBeNull();
    expect(anchor?.getAttribute("role")).toBe("link");
    expect(anchor?.getAttribute("tabindex")).toBe("0");
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
    expect(svgAnchor?.getAttribute("href")).toBeNull();
    expect((svgAnchor as HTMLElement | null)?.dataset.dakiaExternalHref).toBe(
      "https://example.test/vector",
    );
  });
});
