import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import appleMailReplySection from "../test/fixtures/apple-mail-reply-section.html?raw";
import freshdeskReplySection from "../test/fixtures/freshdesk-reply-section.html?raw";
import outlookWordReplySection from "../test/fixtures/outlook-word-reply-section.html?raw";
import stripeReceiptLayout from "../test/fixtures/stripe-receipt-layout.html?raw";
import swedbankSignatureLayout from "../test/fixtures/swedbank-signature-layout.html?raw";
import { buildEmailDocument, HtmlMessage } from "./HtmlMessage";

function parse(source: string | undefined) {
  if (!source) throw new Error("Expected an isolated history document");
  return new DOMParser().parseFromString(source, "text/html");
}

function renderedEmailRoot(message: HTMLElement, surface = "current") {
  const host =
    surface === "current"
      ? message
      : message.querySelector<HTMLElement>(
          `[data-dakia-email-surface="${surface}"]`,
        );
  const emailSurface = host?.shadowRoot
    ?.firstElementChild as HTMLElement | null;
  return emailSurface?.shadowRoot ?? null;
}

describe("HTML email appearance", () => {
  it("uses a legible reader baseline for ordinary messages", () => {
    const email = buildEmailDocument("<p>Hello from Dakia</p>", false);

    expect(email.source).toContain(
      'font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
    );
    expect(email.source).toContain("padding: 20px");
    expect(email.source).toContain("body.dakia-reader-content > *");
  });

  it("adapts neutral palettes in dark mode without changing brand colors", () => {
    const email = buildEmailDocument(swedbankSignatureLayout, true);
    const stylesheet = buildEmailDocument(
      '<style>.ink { color:#000000; background-color:#ffffff }</style><p class="ink">Plain text</p>',
      true,
    );

    expect(email.dark).toBe(true);
    expect(email.source).toContain("color-scheme: dark");
    expect(email.source).toContain('bgcolor="#191a19"');
    expect(email.source).toContain("color: #d9dedb");
    expect(email.source).toContain("#4f83c4");
    expect(email.source).toContain("#734744");
    expect(email.source).toContain("swedbank-logo.png");
    expect(stylesheet.source).toContain("color:#d9dedb");
    expect(stylesheet.source).toContain("background-color:#191a19");
  });

  it("preserves spacer rows that form part of a designed signature", () => {
    const document = parse(
      buildEmailDocument(swedbankSignatureLayout, true).source,
    );
    const message = document.body.querySelector<HTMLTableElement>(
      ':scope > table[role="presentation"]',
    );

    expect(message).not.toBeNull();
    expect(message?.querySelectorAll(":scope > tbody > tr")).toHaveLength(7);
    expect(message?.querySelectorAll(".email-spacer")).toHaveLength(2);
    expect(message?.querySelector('[height="42"]')).not.toBeNull();
    expect(message?.querySelector('img[alt="Swedbank"]')).not.toBeNull();
    expect(message?.textContent).toContain("K. Näide");
  });

  it("keeps authored dark and image-backed layouts in their original appearance", () => {
    const dark = buildEmailDocument(
      '<div style="background-color:#111111; color:#ffffff">Designed dark mail</div>',
      true,
    );
    const imageBacked = buildEmailDocument(
      '<div style="background-image:url(https://assets.example.test/hero.jpg); color:#000000">Newsletter</div>',
      true,
    );

    expect(dark.dark).toBe(false);
    expect(dark.source).toContain("color-scheme: light");
    expect(imageBacked.dark).toBe(false);
    expect(imageBacked.source).toContain("color:#000000");
  });

  it("does not recolor text over artwork just because unrelated CSS declares white", () => {
    const email = buildEmailDocument(
      [
        "<style>.unused-card { background-color:#ffffff; color:#000000 }</style>",
        '<div style="background-image:url(https://assets.example.test/hero.jpg); color:#000000">Newsletter over artwork</div>',
      ].join(""),
      true,
    );

    expect(email.dark).toBe(false);
    expect(email.source).toContain("color:#000000");
    expect(email.source).toContain("background-color:#ffffff");
  });

  it("preserves a CSS-authored dark body with a white QR card", () => {
    const email = buildEmailDocument(
      [
        "<style>body{background:#111;color:#fff}.qr{background:#fff}</style>",
        '<div class="qr"><img src="https://assets.example.test/transparent-black-qr.png" alt="QR code"></div>',
      ].join(""),
      true,
    );

    expect(email.dark).toBe(false);
    expect(email.source).toContain("body{background:#111;color:#fff}");
    expect(email.source).toContain(".qr{background:#fff}");
    expect(email.source).toContain("transparent-black-qr.png");
  });

  it("keeps image-only white QR cards light without mistaking a decorative header for the body", () => {
    const variants = [
      [
        '<body style="background:#ffffff;color:#000000">',
        "<p>Payment confirmation</p>",
        '<div style="background:#000000"><img src="https://assets.example.test/header-art.png" alt=""></div>',
        '<div style="background:#ffffff"><img src="https://assets.example.test/transparent-black-qr.png" alt="QR code"></div>',
        "</body>",
      ].join(""),
      [
        "<style>body{background:#fff;color:#000}.dark-header{background:#000}.qr{background:#fff}</style>",
        "<p>Payment confirmation</p>",
        '<div class="dark-header"><img src="https://assets.example.test/header-art.png" alt=""></div>',
        '<div class="qr"><img src="https://assets.example.test/transparent-black-qr.png" alt="QR code"></div>',
      ].join(""),
    ];

    for (const html of variants) {
      const email = buildEmailDocument(html, true);
      expect(email.dark).toBe(false);
      expect(email.source).toContain("transparent-black-qr.png");
      expect(email.source).toContain("background:#fff");
    }
    expect(buildEmailDocument(stripeReceiptLayout, true).dark).toBe(true);
  });

  it("skips dark adaptation when a foreground or background palette is ambiguous", () => {
    for (const html of [
      '<div style="background:hsl(0, 0%, 100%); color:#000000">HSL</div>',
      '<div style="background:rgb(255 255 255); color:#000000">Modern RGB</div>',
      '<div style="background:var(--mail-background); color:#000000">Variable</div>',
      '<div style="background:rgba(255,255,255,.5); color:#000000">Transparent</div>',
    ]) {
      const email = buildEmailDocument(html, true);
      expect(email.dark).toBe(false);
      expect(email.source).toContain("color:#000000");
    }
  });

  it("normalizes pathological type and empty spacers in plain reader content", () => {
    const email = buildEmailDocument(
      [
        '<body style="font-size: 96px"><h1 style="font-size:64px">Meaningful heading</h1>',
        '<div style="height:48px">&nbsp;</div>',
        '<p data-testid="bare-empty-paragraph">&nbsp;</p>',
        '<div class="spacer" style="height:48px; border-top:1px solid #ccc">&nbsp;</div>',
        '<style>.brand-rule { background:#f60 }</style><div class="brand-rule" style="height:4px"></div>',
        "</body>",
      ].join(""),
      false,
    );
    const document = parse(email.source);

    expect(email.source).toContain("font-size: 32px");
    expect(email.source).toContain("font-size:64px");
    expect(
      document.querySelector('[data-testid="bare-empty-paragraph"]'),
    ).toBeNull();
    expect(document.querySelectorAll(".spacer")).toHaveLength(1);
    expect(document.querySelector(".brand-rule")).not.toBeNull();
    expect(document.body.classList).toContain("dakia-reader-content");
  });

  it("preserves authored type and every layout cell in designed table mail", () => {
    const email = buildEmailDocument(
      [
        '<body style="font-size: 96px"><h1 style="font-size:64px">Meaningful heading</h1>',
        '<table><tr data-testid="empty-row"><td height="80">&nbsp;</td></tr><tr class="email-spacer"><td height="80">&nbsp;</td></tr><tr><td height="80">Visible cell</td></tr><tr><td height="100"></td><td>Hero copy</td></tr></table>',
        '<p data-testid="bare-empty-paragraph">&nbsp;</p>',
        "</body>",
      ].join(""),
      false,
    );
    const document = parse(email.source);

    expect(email.source).toContain("font-size: 32px");
    expect(email.source).toContain("font-size:64px");
    expect(document.querySelector('[data-testid="empty-row"]')).not.toBeNull();
    expect(document.querySelector(".email-spacer")).not.toBeNull();
    expect(
      document.querySelector('[data-testid="bare-empty-paragraph"]'),
    ).not.toBeNull();
    expect(document.querySelector('td[height="100"]')).not.toBeNull();
    expect(document.body.textContent).toContain("Visible cell");
    expect(document.body.textContent).toContain("Hero copy");
    expect(document.body.classList).not.toContain("dakia-reader-content");
  });

  it("preserves the receipt table contract while adapting neutral surfaces for dark mode", () => {
    const email = buildEmailDocument(stripeReceiptLayout, true);
    const document = parse(email.source);
    const wrapper =
      document.querySelector<HTMLTableElement>("table.st-Wrapper");
    const summaryAmounts = [
      ...document.querySelectorAll<HTMLElement>("td.Table-amount"),
    ].map((cell) => cell.textContent?.replace(/\s+/g, " ").trim());

    expect(email.dark).toBe(true);
    expect(wrapper).not.toBeNull();
    expect(wrapper?.getAttribute("width")).toBe("600");
    expect(wrapper?.getAttribute("style")).toContain("min-width: 600px");
    expect(
      document.querySelector('td.st-Spacer--stacked[colspan="3"][height="24"]'),
    ).not.toBeNull();
    expect(
      document.querySelector('td.Table-divider[colspan="3"][height="6"]'),
    ).not.toBeNull();
    expect(summaryAmounts).toEqual(["US$12.34", "US$12.34"]);

    expect(document.body.getAttribute("bgcolor")).toBe("#191a19");
    expect(wrapper?.getAttribute("bgcolor")).toBe("#191a19");
    expect(
      document
        .querySelector<HTMLElement>("td.Header-left")
        ?.getAttribute("style"),
    ).toContain("background-color: #000000");
    expect(email.source).toContain("color: #d9dedb");
    expect(email.source).toContain(
      "https://stripe-images.s3.amazonaws.com/html_emails/2017-08-21/header/Header-background.png",
    );
  });

  it("uses reader wrapping only for plain content, not designed receipt tables", () => {
    const designed = parse(
      buildEmailDocument(stripeReceiptLayout, true).source,
    );
    const plain = parse(
      buildEmailDocument(`<p>${"unbroken".repeat(100)}</p>`, false).source,
    );
    const designedCompatibilityStyle =
      designed.head.lastElementChild?.textContent;
    const plainCompatibilityStyle = plain.head.lastElementChild?.textContent;

    expect(designedCompatibilityStyle).toContain(
      "html { color-scheme: dark; }",
    );
    expect(designedCompatibilityStyle).toContain("overflow-wrap: normal");
    expect(designedCompatibilityStyle).not.toContain(
      "table { max-width: 100%; }",
    );
    expect(designed.body.classList).not.toContain("dakia-reader-content");
    expect(plain.body.classList).toContain("dakia-reader-content");
    expect(plainCompatibilityStyle).toContain(
      "body.dakia-reader-content { padding: 20px; overflow-wrap: anywhere; }",
    );
  });

  it("keeps the complete receipt mounted through dark-light-dark theme changes", async () => {
    const appRoot = document.documentElement;
    const previousScheme = appRoot.dataset.mantineColorScheme;
    appRoot.dataset.mantineColorScheme = "dark";

    const rendered = render(
      <HtmlMessage
        html={stripeReceiptLayout}
        title="Example receipt"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );
    const message = screen.getByRole("document", { name: "Example receipt" });
    const receipt = () => renderedEmailRoot(message);
    const assertReceiptStructure = () => {
      expect(receipt()?.querySelector("table.st-Wrapper")).not.toBeNull();
      expect(receipt()?.querySelector("td.Header-left img")).not.toBeNull();
      expect(
        receipt()?.querySelector(
          'td.st-Spacer--stacked[colspan="3"][height="24"]',
        ),
      ).not.toBeNull();
      expect(
        [...(receipt()?.querySelectorAll("td.Table-amount") ?? [])].map(
          (cell) => cell.textContent?.replace(/\s+/g, " ").trim(),
        ),
      ).toEqual(["US$12.34", "US$12.34"]);
      expect(receipt()?.textContent?.match(/US\$12\.34/g)).toHaveLength(4);
    };

    try {
      await waitFor(() => expect(message).toHaveClass("html-message--dark"));
      expect(receipt()?.querySelector("body")?.getAttribute("bgcolor")).toBe(
        "#191a19",
      );
      assertReceiptStructure();

      await act(async () => {
        appRoot.dataset.mantineColorScheme = "light";
      });
      await waitFor(() =>
        expect(message).not.toHaveClass("html-message--dark"),
      );
      expect(receipt()?.querySelector("body")?.getAttribute("bgcolor")).toBe(
        "#f6f9fc",
      );
      assertReceiptStructure();

      await act(async () => {
        appRoot.dataset.mantineColorScheme = "dark";
      });
      await waitFor(() => expect(message).toHaveClass("html-message--dark"));
      expect(receipt()?.querySelector("body")?.getAttribute("bgcolor")).toBe(
        "#191a19",
      );
      assertReceiptStructure();
    } finally {
      rendered.unmount();
      if (previousScheme === undefined) {
        delete appRoot.dataset.mantineColorScheme;
      } else {
        appRoot.dataset.mantineColorScheme = previousScheme;
      }
    }
  });

  it("wraps a plain reply separately from quoted table history and a tracking pixel", () => {
    const longReply = "current-unbroken-token".repeat(80);
    render(
      <HtmlMessage
        html={[
          `<p>${longReply}</p>`,
          '<img src="https://images.example.test/tracking.gif" width="1" height="1" alt="">',
          '<div class="gmail_quote"><table><tr><td class="quoted-spacer" colspan="3" height="24">&nbsp;</td></tr><tr><td>Quoted table message</td></tr></table></div>',
        ].join("")}
        title="Reply with table history"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );

    const message = screen.getByRole("document", {
      name: "Reply with table history",
    });
    const current = renderedEmailRoot(message);
    const currentBody = current?.querySelector("body");
    const currentCompatibilityStyle = current?.querySelector(
      "head style:last-child",
    )?.textContent;

    expect(currentBody).toHaveClass("dakia-reader-content");
    expect(currentBody?.textContent).toContain(longReply);
    expect(currentBody?.textContent).not.toContain("Quoted table message");
    expect(currentBody?.querySelector("table")).toBeNull();
    expect(
      currentBody?.querySelector('img[width="1"][height="1"]'),
    ).not.toBeNull();
    expect(currentCompatibilityStyle).toContain("overflow-wrap: anywhere");

    fireEvent.click(screen.getByRole("button", { name: "Show history" }));
    const history = renderedEmailRoot(message, "history");
    expect(history?.querySelector("body")).not.toHaveClass(
      "dakia-reader-content",
    );
    expect(
      history?.querySelector(".quoted-spacer")?.getAttribute("height"),
    ).toBe("24");
    expect(history?.textContent).toContain("Quoted table message");
  });

  it("does not constrain complex wrappers that contain a table or artwork", () => {
    const email = buildEmailDocument(
      '<div><table><tr><td><img src="https://assets.example.test/logo.png" alt="Logo"></td></tr></table></div>',
      false,
    );
    const document = parse(email.source);

    expect(document.body.classList.contains("dakia-reader-content")).toBe(
      false,
    );
    expect(email.source).toContain("<table>");
  });

  it("preserves remote email images, stylesheets, and fonts", () => {
    const email = buildEmailDocument(
      [
        '<link rel="stylesheet" href="https://assets.example.test/mail.css">',
        "<style>",
        '@import url("https://assets.example.test/imported.css");',
        '@font-face { font-family: Mail; src: url("https://assets.example.test/mail.woff2"); }',
        '.hero { color: #000000; background-image: url("https://assets.example.test/hero.jpg"); }',
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
      'src="https://images.example.test/protocol-relative.jpg"',
    );
    expect(email.source).toContain("img-src data: http: https:");
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
        '<svg><a xlink:href="https://example.test/vector"><animate attributeName="href"></animate><set attributeName="href"></set><text>Vector</text></a></svg>',
        "<math><script>alert(1)</script></math>",
        '<a href="javascript:alert(1)" onclick="alert(1)">Open</a>',
      ].join(""),
      false,
    );
    const document = parse(email.source);
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
    expect(document.querySelector("[srcdoc], [attributeName]")).toBeNull();
    expect(document.querySelector("a")?.hasAttribute("href")).toBe(false);
    expect(document.querySelector("img")?.hasAttribute("src")).toBe(false);
    expect(document.querySelector("video")?.hasAttribute("poster")).toBe(false);
    expect(policy?.getAttribute("content")).toContain("script-src 'none'");
    expect(policy?.getAttribute("content")).toContain("form-action 'none'");
  });

  it("renders sanitized current content in a layout-contained nested shadow tree", () => {
    render(
      <HtmlMessage
        html="<script>bad()</script><p>Hello</p>"
        title="Secure message"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );

    const message = screen.getByRole("document", { name: "Secure message" });
    expect(renderedEmailRoot(message)).not.toBeNull();
    expect(renderedEmailRoot(message)?.querySelector("script")).toBeNull();
    expect(message.style.contain).toBe("layout paint");
    expect(document.querySelector("iframe")).toBeNull();
  });

  it("splits current content and provider history into separately sanitized documents", () => {
    for (const [html, current, history] of [
      [
        freshdeskReplySection,
        "Thank you for letting us know.",
        "Oldest customer request.",
      ],
      [
        appleMailReplySection,
        "This is the current reply",
        "The earlier message belongs inside history.",
      ],
      [
        outlookWordReplySection,
        "uus vastus, mis peab nähtavaks jääma",
        "Kõige vanem sõnum",
      ],
    ]) {
      const email = buildEmailDocument(html, false);
      const visible = parse(email.source);
      const quoted = parse(email.historySource);

      expect(visible.body.textContent).toContain(current);
      expect(visible.body.textContent).not.toContain(history);
      expect(quoted.body.textContent).toContain(history);
      expect(visible.querySelector("details.dakia-quoted-history")).toBeNull();
      expect(quoted.querySelector("script")).toBeNull();
    }
  });

  it("keeps generic quotes, citation-only markers, and malformed lookalikes visible", () => {
    for (const html of [
      "<p>Fresh</p><blockquote>A deliberate quotation</blockquote>",
      '<p>Fresh</p><div class="gmail_quote">On 19 Feb 2026 at 21:00 +0200, Pat wrote:</div>',
      '<p>Fresh</p><div class="gmail_quote_extra">Authored content</div>',
      '<p>Fresh</p><div class="yahoo_quoted-note">Authored content</div>',
      '<p>Fresh</p><div id="AOLMsgPartialSummary">Authored content</div>',
      '<p>Fresh</p><div name="messageReplySection"><blockquote type="cite"></blockquote></div>',
    ]) {
      const email = buildEmailDocument(html, false);
      expect(email.historySource).toBeUndefined();
      expect(parse(email.source).body.textContent).toContain("Fresh");
    }
  });

  it("moves adjacent citations and mixed provider quote chains into one history source", () => {
    const email = buildEmailDocument(
      [
        "<p>Current reply</p>",
        "<div>On 19 Feb 2026 at 21:00 +0200, Rowan Example &lt;rowan@example.test&gt;, wrote:</div>",
        '<div class="gmail_quote">First older reply</div>',
        '<div class="freshdesk_quote">Second older reply</div>',
        "<p>Authored footer</p>",
      ].join(""),
      false,
    );
    const visible = parse(email.source);
    const history = parse(email.historySource);

    expect(visible.body.textContent).toContain("Current reply");
    expect(visible.body.textContent).toContain("Authored footer");
    expect(visible.body.textContent).not.toContain("older reply");
    expect(history.body.textContent).toContain("Rowan Example");
    expect(history.body.textContent).toContain("First older reply");
    expect(history.body.textContent).toContain("Second older reply");
  });

  it("skips empty provider markers before the meaningful quoted history", () => {
    const email = buildEmailDocument(
      [
        "<p>Fresh</p>",
        '<div class="gmail_quote gmail_quote_container"><div class="gmail_attr"></div></div>',
        '<div class="yahoo_quoted">Meaningful older reply</div>',
      ].join(""),
      false,
    );

    expect(parse(email.source).body.textContent).toContain("Fresh");
    expect(parse(email.source).body.textContent).not.toContain(
      "Meaningful older reply",
    );
    expect(parse(email.historySource).body.textContent).toContain(
      "Meaningful older reply",
    );
  });

  it("moves Thunderbird citations into history while preserving current content", () => {
    const email = buildEmailDocument(
      [
        "<p>Current reply</p>",
        '<div class="moz-cite-prefix">On 19 Feb 2026 at 21:00 +0200, Rowan Example &lt;rowan@example.test&gt;, wrote:</div>',
        '<blockquote type="cite"><p>Earlier message</p></blockquote>',
      ].join(""),
      false,
    );

    expect(parse(email.source).body.textContent).toContain("Current reply");
    expect(parse(email.source).body.textContent).not.toContain(
      "Earlier message",
    );
    expect(parse(email.historySource).body.textContent).toContain(
      "Rowan Example",
    );
    expect(parse(email.historySource).body.textContent).toContain(
      "Earlier message",
    );
  });

  it("leaves an empty Apple Mail reply section visible and without a disclosure", () => {
    const email = buildEmailDocument(
      [
        "<p>Current reply</p>",
        '<div name="messageReplySection">',
        "On 19 Feb 2026 at 21:00 +0200, Sender &lt;sender@example.test&gt;, wrote:<br>",
        '<blockquote type="cite"><div name="messageBodySection"></div></blockquote>',
        "</div>",
      ].join(""),
      false,
    );

    expect(email.historySource).toBeUndefined();
    expect(parse(email.source).body.textContent).toContain("Current reply");
    expect(parse(email.source).body.textContent).toContain("Sender");
  });

  it("retains the established Gmail, Yahoo, AOL, and Outlook separators", () => {
    for (const [html, expected] of [
      [
        '<p>Fresh</p><div class="gmail_quote">Old Gmail reply</div>',
        "Old Gmail reply",
      ],
      [
        '<p>Fresh</p><div class="yahoo_quoted">Old Yahoo reply</div>',
        "Old Yahoo reply",
      ],
      [
        '<p>Fresh</p><div id="AOLMsgPart_4f8c2d16">Old AOL reply</div>',
        "Old AOL reply",
      ],
      [
        '<p>Fresh</p><div id="divRplyFwdMsg">From: Old Sender</div><blockquote>Old Outlook body</blockquote><p>Authored footer</p>',
        "Old Outlook body",
      ],
    ]) {
      const email = buildEmailDocument(html, false);
      const visible = parse(email.source);
      const history = parse(email.historySource);

      expect(history.body.textContent).toContain(expected);
      expect(visible.body.textContent).toContain("Fresh");
      expect(visible.body.textContent).not.toContain(expected);
    }
  });

  it("keeps authored Outlook footer siblings out of quoted history", () => {
    const email = buildEmailDocument(
      '<p>Fresh</p><div id="divRplyFwdMsg">From: Old Sender</div><blockquote>Old body</blockquote><p>Authored footer</p>',
      false,
    );

    expect(parse(email.historySource).body.textContent).toContain("Old body");
    expect(parse(email.historySource).body.textContent).not.toContain(
      "Authored footer",
    );
    expect(parse(email.source).body.textContent).toContain("Authored footer");
  });

  it("does not collapse bare dividers or Word reply headers without quoted content", () => {
    const noContent = buildEmailDocument(
      [
        "<p>Reply</p>",
        '<div><div style="border:none;border-top:solid #E1E1E1 1.0pt;padding:3.0pt 0cm 0cm 0cm">',
        "<p><b>From:</b> Old Sender<br><b>Sent:</b> Friday<br><b>To:</b> New Sender<br><b>Subject:</b> RE: Empty quote</p>",
        "</div></div>",
        "<p>&nbsp;</p>",
      ].join(""),
      false,
    );
    const bareDivider = buildEmailDocument(
      '<p>Above the divider</p><div style="border:none;border-top:solid #E1E1E1 1.0pt"><p>Quarterly report highlights</p></div><p>Below the divider</p>',
      false,
    );

    expect(noContent.historySource).toBeUndefined();
    expect(parse(noContent.source).body.textContent).toContain(
      "From: Old Sender",
    );
    expect(bareDivider.historySource).toBeUndefined();
    expect(parse(bareDivider.source).body.textContent).toContain(
      "Below the divider",
    );
  });

  it("collapses stacked Word reply headers into one history source", () => {
    const email = buildEmailDocument(
      [
        "<p>Fresh</p>",
        '<div><div style="border:none;border-top:solid #E1E1E1 1.0pt"><p><b>From:</b> One<br><b>Sent:</b> Monday<br><b>To:</b> Two<br><b>Subject:</b> RE: A</p></div></div>',
        "<p>First quoted body</p>",
        '<div><div style="border:none;border-top:solid #E1E1E1 1.0pt"><p><b>From:</b> Two<br><b>Sent:</b> Tuesday<br><b>To:</b> One<br><b>Subject:</b> RE: A</p></div></div>',
        "<p>Second quoted body</p>",
      ].join(""),
      false,
    );

    expect(parse(email.source).body.textContent).toContain("Fresh");
    expect(parse(email.source).body.textContent).not.toContain("quoted body");
    expect(parse(email.historySource).body.textContent).toContain(
      "First quoted body",
    );
    expect(parse(email.historySource).body.textContent).toContain(
      "Second quoted body",
    );
  });

  it("uses one native accessible disclosure and preserves isolated history through repeated toggles", () => {
    render(
      <HtmlMessage
        html={freshdeskReplySection}
        title="Freshdesk history"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );

    const message = screen.getByRole("document", { name: "Freshdesk history" });
    const control = screen.getByRole("button", { name: "Show history" });
    expect(screen.getAllByRole("button")).toHaveLength(1);
    expect(control).toHaveAttribute("aria-expanded", "false");
    expect(renderedEmailRoot(message)?.textContent).toContain(
      "Thank you for letting us know.",
    );
    expect(renderedEmailRoot(message)?.textContent).not.toContain(
      "Oldest customer request.",
    );
    control.focus();

    for (let cycle = 0; cycle < 2; cycle += 1) {
      fireEvent.click(control);
      expect(control).toHaveAttribute("aria-expanded", "true");
      expect(document.activeElement).toBe(control);
      expect(renderedEmailRoot(message, "history")?.textContent).toContain(
        "Oldest customer request.",
      );

      fireEvent.click(control);
      expect(control).toHaveAttribute("aria-expanded", "false");
      expect(document.activeElement).toBe(control);
    }
  });

  it("repeatedly toggles Apple Mail and Outlook history through the native control", () => {
    for (const [html, title, historyText] of [
      [
        appleMailReplySection,
        "Apple Mail history",
        "The earlier message belongs inside history.",
      ],
      [outlookWordReplySection, "Outlook history", "Kõige vanem sõnum"],
    ]) {
      const rendered = render(
        <HtmlMessage
          html={html}
          title={title}
          showHistoryLabel="Show history"
          hideHistoryLabel="Hide history"
        />,
      );
      const message = screen.getByRole("document", { name: title });
      const control = screen.getByRole("button", { name: "Show history" });

      for (let cycle = 0; cycle < 2; cycle += 1) {
        fireEvent.click(control);
        expect(control).toHaveAttribute("aria-expanded", "true");
        expect(renderedEmailRoot(message, "history")?.textContent).toContain(
          historyText,
        );
        fireEvent.click(control);
        expect(control).toHaveAttribute("aria-expanded", "false");
      }
      rendered.unmount();
    }
  });

  it("keeps the native disclosure usable when hostile email CSS hides summaries", () => {
    render(
      <HtmlMessage
        html={
          '<style>summary { display:none!important }</style><p>Fresh</p><div class="gmail_quote">Old</div>'
        }
        title="Hostile history"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );

    const button = screen.getByRole("button", { name: "Show history" });
    expect(button).toHaveClass("html-message-history-toggle");
    expect(button).toHaveAttribute("aria-expanded", "false");
  });

  it("keeps external links usable only through Dakia's system-browser handler", () => {
    const email = buildEmailDocument(
      '<a href="https://example.test/details">Open details</a>',
      false,
    );
    const anchor = parse(email.source).querySelector("a");

    expect(anchor?.getAttribute("href")).toBeNull();
    expect(anchor?.getAttribute("role")).toBe("link");
    expect(anchor?.getAttribute("tabindex")).toBe("0");
    expect((anchor as HTMLElement | null)?.dataset.dakiaExternalHref).toBe(
      "https://example.test/details",
    );
  });

  it("routes a clicked mailto link to Dakia's compose handler", () => {
    const onMailto = vi.fn();
    render(
      <HtmlMessage
        html={
          '<a href="mailto:juhan%2Btamm@example.com?subject=Tere">Write</a>'
        }
        title="Message with email link"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        onMailto={onMailto}
      />,
    );

    const root = renderedEmailRoot(
      screen.getByRole("document", { name: "Message with email link" }),
    );
    const anchor = root?.querySelector("a");
    expect(anchor).not.toBeNull();
    fireEvent.click(anchor!);

    expect(onMailto).toHaveBeenCalledOnce();
    expect(onMailto).toHaveBeenCalledWith(
      "mailto:juhan%2Btamm@example.com?subject=Tere",
    );
  });

  it("removes non-HTML navigation surfaces while retaining safe SVG links", () => {
    const email = buildEmailDocument(
      [
        '<map><area href="https://example.test/mapped"></map>',
        '<svg><a xlink:href="https://example.test/vector"><text>Vector link</text></a></svg>',
      ].join(""),
      false,
    );
    const document = parse(email.source);
    const svgAnchor = document.querySelector("svg a") as HTMLElement | null;

    expect(document.querySelector("area")).toBeNull();
    expect(svgAnchor?.getAttribute("xlink:href")).toBeNull();
    expect(svgAnchor?.getAttribute("href")).toBeNull();
    expect(svgAnchor?.dataset.dakiaExternalHref).toBe(
      "https://example.test/vector",
    );
  });
});
