import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { Suspense, startTransition, useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import tauriConfig from "../../src-tauri/tauri.conf.json";
import appleMailReplySection from "../test/fixtures/apple-mail-reply-section.html?raw";
import freshdeskReplySection from "../test/fixtures/freshdesk-reply-section.html?raw";
import outlookWordReplySection from "../test/fixtures/outlook-word-reply-section.html?raw";
import stripeReceiptLayout from "../test/fixtures/stripe-receipt-layout.html?raw";
import swedbankSignatureLayout from "../test/fixtures/swedbank-signature-layout.html?raw";
import { buildEmailDocument, HtmlMessage } from "./HtmlMessage";

const mocks = vi.hoisted(() => ({
  openExternal: vi.fn(async () => undefined),
}));

vi.mock("../api", () => ({
  api: { openExternal: mocks.openExternal },
}));

function parse(source: string | undefined) {
  if (!source) throw new Error("Expected an isolated history document");
  return new DOMParser().parseFromString(source, "text/html");
}

function renderedEmailFrame(message: HTMLElement, surface = "current") {
  const host =
    surface === "current"
      ? message
      : message.querySelector<HTMLElement>(
          `[data-dakia-email-surface="${surface}"]`,
        );
  return host?.shadowRoot?.querySelector("iframe") ?? null;
}

function renderedEmailRoot(message: HTMLElement, surface = "current") {
  const frame = renderedEmailFrame(message, surface);
  return frame
    ? parse(frame.getAttribute("srcdoc") ?? "").documentElement
    : null;
}

function rendererToken(message: HTMLElement, surface = "current") {
  const source = renderedEmailFrame(message, surface)?.getAttribute("srcdoc");
  const token = source
    ? parse(source)
        .querySelector("script[data-dakia-renderer-token]")
        ?.getAttribute("data-dakia-renderer-token")
    : null;
  if (!token) throw new Error("Expected sandbox renderer token");
  return token;
}

async function sha256CspHash(source: string) {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(source),
  );
  const bytes = new Uint8Array(digest);
  return `sha256-${btoa(String.fromCharCode(...bytes))}`;
}

function packagedCsp() {
  return tauriConfig.app.security.csp;
}

function sendRendererMessage(
  message: HTMLElement,
  data: Record<string, unknown>,
  surface = "current",
) {
  const frame = renderedEmailFrame(message, surface);
  if (!frame) throw new Error("Expected sandbox renderer frame");
  // jsdom does not create a browsing context for an iframe in a shadow tree.
  // Give this test-only frame a stable stand-in so the production source check
  // is exercised instead of weakening it for the test environment.
  if (!frame.contentWindow) {
    Object.defineProperty(frame, "contentWindow", {
      configurable: true,
      value: window,
    });
  }
  window.dispatchEvent(
    new MessageEvent("message", { data, source: frame.contentWindow }),
  );
}

describe("HTML email appearance", () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });
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
    expect(email.source).not.toContain("swedbank-logo.png");
    expect(email.hasBlockedRemoteContent).toBe(true);
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
    expect(email.source).not.toContain("transparent-black-qr.png");
    expect(email.hasBlockedRemoteContent).toBe(true);
    expect(email.source).toContain('alt="QR code"');
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
      expect(email.source).not.toContain("transparent-black-qr.png");
      expect(email.hasBlockedRemoteContent).toBe(true);
      expect(email.source).toContain('alt="QR code"');
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
    expect(email.source).not.toContain("Header-background.png");
    expect(email.hasBlockedRemoteContent).toBe(true);
    // Consent enables image src only, never CSS background fetches.
    const consented = buildEmailDocument(stripeReceiptLayout, true, true);
    expect(consented.source).not.toContain("Header-background.png");
    expect(
      consented.document
        .querySelector("td.Header-left img")
        ?.getAttribute("src"),
    ).toBe(
      "https://stripe-images.stripecdn.com/notifications/hosted/20180110/Header/Left.png",
    );
    expect(
      consented.document
        .querySelector("table.st-Wrapper")
        ?.getAttribute("width"),
    ).toBe("600");
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
    const currentCompatibilityStyle = [
      ...(current?.querySelectorAll("head style") ?? []),
    ]
      .map((style) => style.textContent ?? "")
      .find((style) => style.includes("body.dakia-reader-content"));

    expect(currentBody?.classList.contains("dakia-reader-content")).toBe(true);
    expect(currentBody?.textContent).toContain(longReply);
    expect(currentBody?.textContent).not.toContain("Quoted table message");
    expect(currentBody?.querySelector("table")).toBeNull();
    expect(
      currentBody?.querySelector('img[width="1"][height="1"]'),
    ).not.toBeNull();
    expect(currentCompatibilityStyle).toContain("overflow-wrap: anywhere");

    fireEvent.click(screen.getByRole("button", { name: "Show history" }));
    const history = renderedEmailRoot(message, "history");
    expect(
      history
        ?.querySelector("body")
        ?.classList.contains("dakia-reader-content"),
    ).toBe(false);
    expect(
      history?.querySelector(".quoted-spacer")?.getAttribute("height"),
    ).toBe("24");
    expect(history?.textContent).toContain("Quoted table message");
    fireEvent.click(screen.getByRole("button", { name: "Hide history" }));
    expect(
      message.querySelector('[data-dakia-email-surface="history"]')
        ?.parentElement,
    ).toHaveAttribute("aria-hidden", "true");
    expect(
      message.querySelector('[data-dakia-email-surface="history"]')
        ?.parentElement,
    ).toHaveStyle({ display: "none" });
    expect(
      renderedEmailRoot(message)
        ?.querySelector("body")
        ?.classList.contains("dakia-reader-content"),
    ).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "Show history" }));
    const reopened = renderedEmailRoot(message, "history");
    expect(
      reopened?.querySelector(".quoted-spacer")?.getAttribute("height"),
    ).toBe("24");
    expect(
      reopened
        ?.querySelector("body")
        ?.classList.contains("dakia-reader-content"),
    ).toBe(false);
    expect(reopened?.textContent).toContain("Quoted table message");
    expect(
      renderedEmailRoot(message)?.querySelector("img")?.hasAttribute("src"),
    ).toBe(false);
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

  it("blocks every remote email resource before it reaches the rendered document", () => {
    const html = [
      '<link rel="stylesheet" href="https://assets.example.test/mail.css">',
      "<style>",
      '@import url("https://assets.example.test/imported.css");',
      '@font-face { font-family: Mail; src: url("https://assets.example.test/mail.woff2"); }',
      '.hero { color: #000000; background-image: url("https://assets.example.test/hero.jpg"); }',
      "</style>",
      '<img src="https://images.example.test/newsletter.jpg" alt="Newsletter">',
      '<img src="//images.example.test/protocol-relative.jpg" alt="Second">',
      '<img srcset="https://images.example.test/one.jpg 1x, https://images.example.test/two.jpg 2x">',
      '<svg><image href="https://images.example.test/vector.svg"></image></svg>',
      '<div background="https://images.example.test/background.jpg"></div>',
      '<div style="background-image: u\\72l(https://images.example.test/escaped.jpg)"></div>',
      '<style>.retina { background-image: image-set("https://images.example.test/retina.jpg" 1x) }</style>',
      '<style>@import/**/"//images.example.test/commented-import.css";</style>',
      '<svg><defs><filter id="local-filter"></filter></defs><rect filter="u\\72l(https://images.example.test/filter.svg#blur)" fill="url(//images.example.test/fill.svg#paint)" mask="url(https://images.example.test/mask.svg#mask)" clip-path="url(https://images.example.test/clip.svg#clip)" marker-start="url(https://images.example.test/marker.svg#start)" stroke="url(#local-filter)"></rect></svg>',
      '<img src="data:image/png;base64,iVBORw0KGgo=" alt="Inline">',
    ].join("");
    const email = buildEmailDocument(html, false);
    const document = parse(email.source);

    expect(email.hasBlockedRemoteContent).toBe(true);
    expect(email.source).not.toContain("assets.example.test");
    expect(email.source).not.toContain("images.example.test");
    expect(document.querySelector("link")).toBeNull();
    expect(
      document.querySelector("img[alt='Newsletter']")?.hasAttribute("src"),
    ).toBe(false);
    expect(
      document.querySelector("img[alt='Inline']")?.getAttribute("src"),
    ).toBe("data:image/png;base64,iVBORw0KGgo=");
    expect(document.querySelector("img[srcset]")).toBeNull();
    expect(document.querySelector("svg image")?.hasAttribute("href")).toBe(
      false,
    );
    const rect = document.querySelector("svg rect");
    expect(rect?.getAttribute("filter")).toBe("");
    expect(rect?.getAttribute("fill")).toBe("");
    expect(rect?.getAttribute("mask")).toBe("");
    expect(rect?.getAttribute("clip-path")).toBe("");
    expect(rect?.getAttribute("marker-start")).toBe("");
    expect(rect?.getAttribute("stroke")).toBe("");
    expect(document.querySelector("[background]")).toBeNull();
    expect(email.source).toContain("img-src data:");
    expect(email.source).not.toContain("img-src data: http: https:");
  });

  it("loads only HTTP(S) image attributes after explicit consent", () => {
    const email = buildEmailDocument(
      [
        '<link rel="stylesheet" href="https://assets.example.test/mail.css">',
        '<style>@import url("https://assets.example.test/imported.css"); .hero { background-image:url("https://assets.example.test/hero.jpg") }</style>',
        '<img src="//images.example.test/newsletter.jpg" srcset="https://images.example.test/one.jpg 1x">',
        '<img src="http://images.example.test/plain-http.jpg" alt="HTTP image">',
        '<svg><image href="https://images.example.test/vector.svg"></image></svg>',
        '<div style="background:u\\72l(asset://localhost/private); border-image:u\\72l(https://images.example.test/escaped.jpg)"></div>',
        '<div style="background-image:image-set(&quot;asset://localhost/private.png&quot; 1x); border-image:-webkit-image-set(&quot;file:///etc/passwd&quot; 1x); mask:cross-fade(url(tauri://localhost/ipc), url(ipc://localhost/command));"></div>',
      ].join(""),
      false,
      true,
    );

    expect(email.hasBlockedRemoteContent).toBe(false);
    expect(email.source).not.toContain("assets.example.test/mail.css");
    expect(email.source).not.toContain("assets.example.test/imported.css");
    expect(email.source).not.toContain("assets.example.test/hero.jpg");
    expect(email.source).toContain(
      'src="https://images.example.test/newsletter.jpg"',
    );
    expect(email.source).toContain(
      'src="http://images.example.test/plain-http.jpg"',
    );
    expect(email.source).not.toContain("srcset=");
    expect(email.source).not.toContain("asset://");
    expect(email.source).not.toContain("file://");
    expect(email.source).not.toContain("tauri://");
    expect(email.source).not.toContain("ipc://");
    expect(email.source).toContain("img-src data: http: https:");
    expect(email.source).toContain("style-src 'unsafe-inline'; font-src data:");
  });

  it("never restores local or executable srcset schemes after consent", () => {
    const email = buildEmailDocument(
      [
        '<img srcset="asset://localhost/private.png 1x">',
        '<img srcset="tauri://localhost/ipc 1x">',
        '<img srcset="file:///etc/passwd 1x">',
        '<img srcset="java&#x0A;script:alert(1) 1x">',
        '<img srcset="data:text/html;base64,PHNjcmlwdD4= 1x">',
      ].join(""),
      false,
      true,
    );

    expect(parse(email.source).querySelectorAll("img[srcset]")).toHaveLength(0);
    expect(email.source).not.toContain("asset://");
    expect(email.source).not.toContain("tauri://");
    expect(email.source).not.toContain("file://");
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
        '<img src="data:image/png;base64,iVBORw0KGgo=" onload="url(#x);window.__owned=true" srcdoc="url(https://evil.example/frame)">',
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

  it("renders sanitized current content in an opaque sandboxed iframe", async () => {
    render(
      <HtmlMessage
        html={
          '<script data-dakia-renderer-token="sender-token">bad()</script><p id="dakia-renderer-bootstrap" data-dakia-renderer-token="sender-token">Hello</p>'
        }
        title="Secure message"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );

    const message = screen.getByRole("document", { name: "Secure message" });
    expect(renderedEmailRoot(message)).not.toBeNull();
    expect(
      renderedEmailRoot(message)?.querySelector(
        "script:not([data-dakia-renderer-token])",
      ),
    ).toBeNull();
    expect(message.style.contain).toBe("layout paint");
    const frame = renderedEmailFrame(message);
    expect(frame).not.toBeNull();
    expect(frame).toHaveAttribute("sandbox", "allow-scripts");
    expect(frame?.getAttribute("sandbox")).not.toContain("allow-same-origin");
    expect(frame).toHaveAttribute("referrerpolicy", "no-referrer");
    expect(frame).toHaveAttribute("scrolling", "auto");
    expect(frame?.getAttribute("srcdoc")).toContain("connect-src 'none'");
    expect(frame?.getAttribute("srcdoc")).toContain("media-src 'none'");
    expect(frame?.getAttribute("srcdoc")).toContain("script-src 'sha256-");
    const serialized = parse(frame?.getAttribute("srcdoc") ?? undefined);
    const csp = serialized
      .querySelector('meta[http-equiv="Content-Security-Policy"]')
      ?.getAttribute("content");
    const bootstrap = serialized.querySelector(
      "script[data-dakia-renderer-token]",
    );
    const source = bootstrap?.textContent ?? "";
    const hash = await sha256CspHash(source);
    const alteredHash = await sha256CspHash(`${source} `);
    expect(bootstrap?.getAttribute("data-dakia-renderer-token")).toMatch(
      /^[a-f0-9]{48}$/,
    );
    expect(bootstrap?.getAttribute("data-dakia-renderer-token")).not.toBe(
      "sender-token",
    );
    expect(serialized.querySelectorAll("script")).toHaveLength(1);
    expect(bootstrap?.hasAttribute("nonce")).toBe(false);
    expect(csp).toContain(`script-src '${hash}'`);
    expect(csp).not.toContain(`'${alteredHash}'`);
    expect(packagedCsp()).toContain(`script-src 'self' '${hash}'`);
    expect(packagedCsp()).not.toMatch(/script-src[^;]*'unsafe-inline'/);
    expect(packagedCsp()).not.toContain(`'${alteredHash}'`);
    expect(source).toContain("document.currentScript?.getAttribute");
    expect(source).not.toContain("sender-token");
    expect(
      serialized.querySelector("style[data-dakia-renderer-scroll]")
        ?.textContent,
    ).toContain("overflow-y: auto !important");
  });

  it("keeps bootstrap bytes static while each opaque frame gets a distinct capability", () => {
    render(
      <>
        <HtmlMessage
          html="<p>First message</p>"
          title="First static bridge"
          showHistoryLabel="Show history"
          hideHistoryLabel="Hide history"
        />
        <HtmlMessage
          html="<p>Second message</p>"
          title="Second static bridge"
          showHistoryLabel="Show history"
          hideHistoryLabel="Hide history"
        />
      </>,
    );

    const first = screen.getByRole("document", {
      name: "First static bridge",
    });
    const second = screen.getByRole("document", {
      name: "Second static bridge",
    });
    const firstSource = parse(
      renderedEmailFrame(first)?.getAttribute("srcdoc") ?? undefined,
    );
    const secondSource = parse(
      renderedEmailFrame(second)?.getAttribute("srcdoc") ?? undefined,
    );
    const firstBootstrap = firstSource.querySelector(
      "script[data-dakia-renderer-token]",
    );
    const secondBootstrap = secondSource.querySelector(
      "script[data-dakia-renderer-token]",
    );

    expect(firstBootstrap?.textContent).toBe(secondBootstrap?.textContent);
    expect(firstBootstrap?.getAttribute("data-dakia-renderer-token")).toMatch(
      /^[a-f0-9]{48}$/,
    );
    expect(secondBootstrap?.getAttribute("data-dakia-renderer-token")).toMatch(
      /^[a-f0-9]{48}$/,
    );
    expect(firstBootstrap?.getAttribute("data-dakia-renderer-token")).not.toBe(
      secondBootstrap?.getAttribute("data-dakia-renderer-token"),
    );
  });

  it("keeps remote markup out of the sandbox until the Reader grants consent", () => {
    const onLoadRemoteContent = vi.fn();
    const html = [
      "<script>window.__emailOwned = true</script>",
      '<img src="https://images.example.test/tracker.png" alt="Tracker">',
      '<style>.banner { background-image: url("https://images.example.test/banner.jpg") }</style>',
      '<p class="banner">Hello</p>',
    ].join("");
    const rendered = render(
      <HtmlMessage
        html={html}
        title="Remote content"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        loadRemoteContentLabel="Load remote content"
        onLoadRemoteContent={onLoadRemoteContent}
      />,
    );
    const message = screen.getByRole("document", { name: "Remote content" });

    expect(
      screen.getByRole("button", { name: "Load remote content" }),
    ).toBeVisible();
    expect(
      renderedEmailRoot(message)?.querySelector("img")?.hasAttribute("src"),
    ).toBe(false);
    expect(
      renderedEmailRoot(message)?.querySelector("style")?.textContent,
    ).not.toContain("images.example.test");
    fireEvent.click(
      screen.getByRole("button", { name: "Load remote content" }),
    );
    expect(onLoadRemoteContent).toHaveBeenCalledOnce();

    rendered.rerender(
      <HtmlMessage
        html={html}
        title="Remote content"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        remoteContentAllowed
        loadRemoteContentLabel="Load remote content"
        onLoadRemoteContent={onLoadRemoteContent}
      />,
    );

    expect(
      screen.queryByRole("button", { name: "Load remote content" }),
    ).toBeNull();
    expect(
      renderedEmailRoot(message)?.querySelector("img")?.getAttribute("src"),
    ).toBe("https://images.example.test/tracker.png");
    expect(
      renderedEmailRoot(message)?.querySelector(
        "script:not([data-dakia-renderer-token])",
      ),
    ).toBeNull();
  });

  it("uses app-owned contrast surfaces for Reader controls", () => {
    render(
      <HtmlMessage
        html={[
          '<img src="https://images.example.test/tracker.png" alt="Tracker">',
          '<div class="gmail_quote">Earlier message</div>',
        ].join("")}
        title="Control contrast"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        loadRemoteContentLabel="Load remote content"
        onLoadRemoteContent={vi.fn()}
      />,
    );

    const remote = screen.getByRole("button", {
      name: "Load remote content",
    });
    const history = screen.getByRole("button", { name: "Show history" });
    expect(remote).toHaveClass("html-message-remote-content-toggle");
    expect(history).toHaveClass("html-message-history-toggle");
    for (const control of [remote, history]) {
      expect(control.style.background).toBe(
        "var(--dakia-email-control-surface)",
      );
      expect(control.style.borderColor).toBe(
        "var(--dakia-email-control-border)",
      );
      expect(control.style.borderStyle).toBe("solid");
      expect(control.style.borderWidth).toBe("1px");
      expect(control.style.color).toBe("var(--dakia-email-control-ink)");
      expect(control.getAttribute("style")).not.toContain("color: inherit");
    }
  });

  it("installs a trusted no-referrer policy before consented resource markup", () => {
    const rendered = render(
      <HtmlMessage
        html={[
          '<meta name="referrer" content="unsafe-url">',
          '<img referrerpolicy="unsafe-url" src="https://images.example.test/consented.png" alt="Consented">',
          '<body data-email-body="preserved"><style>.copy { color: #123456 }</style><p class="copy">Email</p></body>',
        ].join("")}
        title="Referrer policy"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        remoteContentAllowed
      />,
    );
    const message = screen.getByRole("document", { name: "Referrer policy" });
    const source = renderedEmailFrame(message)?.getAttribute("srcdoc") ?? "";
    const document = parse(source);

    expect(source).not.toContain("unsafe-url");
    expect(document.head.firstElementChild).toMatchObject({
      localName: "meta",
    });
    expect(document.head.firstElementChild?.getAttribute("name")).toBe(
      "referrer",
    );
    expect(document.head.firstElementChild?.getAttribute("content")).toBe(
      "no-referrer",
    );
    expect(document.querySelectorAll('meta[name="referrer"]')).toHaveLength(1);
    expect(
      [...document.querySelectorAll("style")].some((style) =>
        style.textContent?.includes("#123456"),
      ),
    ).toBe(true);
    expect(document.body.getAttribute("data-email-body")).toBe("preserved");
    expect(
      document.querySelector("img")?.getAttribute("referrerpolicy"),
    ).toBeNull();
    expect(
      renderedEmailRoot(message)?.querySelector("img")?.getAttribute("src"),
    ).toBe("https://images.example.test/consented.png");

    rendered.unmount();
  });

  it("preserves well-formed embedded SVG image data through the iframe source", () => {
    const embeddedSvg =
      "data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciIHdpZHRoPSIzMjAiIGhlaWdodD0iNDgiPjx0ZXh0IHg9IjAiIHk9IjMyIj5FbWJlZGRlZCBpbWFnZSBsb2FkZWQgbG9jYWxseTwvdGV4dD48L3N2Zz4=";
    const email = buildEmailDocument(
      `<img src="${embeddedSvg}" alt="Embedded image loaded locally">`,
      false,
    );
    expect(email.source).toContain(embeddedSvg);

    render(
      <HtmlMessage
        html={`<img src="${embeddedSvg}" alt="Embedded image loaded locally">`}
        title="Embedded SVG"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );
    const message = screen.getByRole("document", { name: "Embedded SVG" });
    expect(
      renderedEmailRoot(message)?.querySelector("img")?.getAttribute("src"),
    ).toBe(embeddedSvg);
  });

  it("keeps embedded SVG sources unchanged when remote-content permission changes", () => {
    const embeddedOne =
      "data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciPjx0ZXh0PkZpcnN0PC90ZXh0Pjwvc3ZnPg==";
    const embeddedTwo =
      "data:image/svg+xml;base64,PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciPjx0ZXh0PlNlY29uZDwvdGV4dD48L3N2Zz4=";
    const html = [
      `<img src="${embeddedOne}" alt="First embedded">`,
      '<img src="https://images.example.test/allowed.png" alt="Remote">',
      `<img src="${embeddedTwo}" alt="Second embedded">`,
    ].join("");
    const rendered = render(
      <HtmlMessage
        html={html}
        title="Embedded permission boundary"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );
    const message = screen.getByRole("document", {
      name: "Embedded permission boundary",
    });

    expect(
      renderedEmailRoot(message)
        ?.querySelector("img[alt='First embedded']")
        ?.getAttribute("src"),
    ).toBe(embeddedOne);
    expect(
      renderedEmailRoot(message)
        ?.querySelector("img[alt='Second embedded']")
        ?.getAttribute("src"),
    ).toBe(embeddedTwo);

    rendered.rerender(
      <HtmlMessage
        html={html}
        title="Embedded permission boundary"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        remoteContentAllowed
      />,
    );

    const root = renderedEmailRoot(message);
    expect(
      root?.querySelector("img[alt='First embedded']")?.getAttribute("src"),
    ).toBe(embeddedOne);
    expect(
      root?.querySelector("img[alt='Second embedded']")?.getAttribute("src"),
    ).toBe(embeddedTwo);
    expect(root?.querySelector("img[alt='Remote']")?.getAttribute("src")).toBe(
      "https://images.example.test/allowed.png",
    );
  });

  it("never mounts media or SVG subresources after remote-image consent", () => {
    const rendered = render(
      <HtmlMessage
        html={[
          '<img src="https://images.example.test/allowed.png" alt="Allowed">',
          '<video autoplay src="https://media.example.test/open.mp4" poster="https://media.example.test/poster.jpg"><source src="https://media.example.test/source.mp4"><track src="https://media.example.test/captions.vtt"></video>',
          '<audio src="https://media.example.test/open.mp3"><source src="https://media.example.test/source.mp3"></audio>',
          '<svg><use href="https://svg.example.test/symbols.svg#mail"></use><image href="https://svg.example.test/image.svg"></image><filter id="f"><feImage href="https://svg.example.test/filter.svg"></feImage></filter></svg>',
        ].join("")}
        title="Remote media"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        remoteContentAllowed
      />,
    );
    const message = screen.getByRole("document", { name: "Remote media" });
    const root = renderedEmailRoot(message);

    expect(root?.querySelector("img[alt='Allowed']")?.getAttribute("src")).toBe(
      "https://images.example.test/allowed.png",
    );
    expect(root?.querySelector("video")?.hasAttribute("src")).toBe(false);
    expect(root?.querySelector("video")?.hasAttribute("poster")).toBe(false);
    expect(root?.querySelector("video source")?.hasAttribute("src")).toBe(
      false,
    );
    expect(root?.querySelector("video track")?.hasAttribute("src")).toBe(false);
    expect(root?.querySelector("audio")?.hasAttribute("src")).toBe(false);
    expect(root?.querySelector("audio source")?.hasAttribute("src")).toBe(
      false,
    );
    expect(root?.querySelector("svg use")?.hasAttribute("href")).toBe(false);
    expect(root?.querySelector("svg image")?.hasAttribute("href")).toBe(false);
    expect(root?.querySelector("feImage")?.hasAttribute("href")).toBe(false);
    expect(root?.outerHTML).not.toContain("media.example.test");
    expect(root?.outerHTML).not.toContain("svg.example.test");
    rendered.unmount();
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

    const message = screen.getByRole("document", {
      name: "Message with email link",
    });
    const root = renderedEmailRoot(message);
    const anchor = root?.querySelector("a");
    expect(anchor).not.toBeNull();
    sendRendererMessage(message, {
      channel: "dakia-email-renderer",
      token: rendererToken(message),
      type: "external-link",
      href: anchor?.getAttribute("data-dakia-external-href"),
    });

    expect(onMailto).toHaveBeenCalledOnce();
    expect(onMailto).toHaveBeenCalledWith(
      "mailto:juhan%2Btamm@example.com?subject=Tere",
    );
  });

  it("keeps the sandbox frame across parent rerenders while using the current mailto handler", () => {
    const firstMailto = vi.fn();
    const currentMailto = vi.fn();
    const html =
      '<a href="mailto:current@example.com?subject=Current">Write</a>';
    const rendered = render(
      <HtmlMessage
        html={html}
        title="Stable sandbox"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        onMailto={firstMailto}
      />,
    );
    const message = screen.getByRole("document", { name: "Stable sandbox" });
    const initialFrame = renderedEmailFrame(message);
    const initialSource = initialFrame?.getAttribute("srcdoc");

    rendered.rerender(
      <HtmlMessage
        html={html}
        title="Stable sandbox"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        onMailto={currentMailto}
      />,
    );

    expect(renderedEmailFrame(message)).toBe(initialFrame);
    expect(renderedEmailFrame(message)?.getAttribute("srcdoc")).toBe(
      initialSource,
    );

    const anchor = renderedEmailRoot(message)?.querySelector("a");
    sendRendererMessage(message, {
      channel: "dakia-email-renderer",
      token: rendererToken(message),
      type: "external-link",
      href: anchor?.getAttribute("data-dakia-external-href"),
    });

    expect(firstMailto).not.toHaveBeenCalled();
    expect(currentMailto).toHaveBeenCalledOnce();
    expect(currentMailto).toHaveBeenCalledWith(
      "mailto:current@example.com?subject=Current",
    );
  });

  it("does not redirect a visible frame to an abandoned transition callback", () => {
    const committedMailto = vi.fn();
    const abandonedMailto = vi.fn();
    let suspendTransition: (() => void) | undefined;
    const pendingTransition = new Promise<void>((resolve) => {
      suspendTransition = resolve;
    });

    function SuspendPending({ suspended }: { suspended: boolean }) {
      if (suspended) throw pendingTransition;
      return null;
    }

    function TransitionHarness() {
      const [pendingUpdate, setPendingUpdate] = useState(false);
      return (
        <>
          <button
            type="button"
            onClick={() =>
              startTransition(() => {
                setPendingUpdate(true);
              })
            }
          >
            Start suspended transition
          </button>
          <Suspense fallback={<span>Loading pending mail</span>}>
            <HtmlMessage
              html='<a href="mailto:transition@example.com">Write</a>'
              title="Transition sandbox"
              showHistoryLabel="Show history"
              hideHistoryLabel="Hide history"
              onMailto={pendingUpdate ? abandonedMailto : committedMailto}
            />
            <SuspendPending suspended={pendingUpdate} />
          </Suspense>
        </>
      );
    }

    render(<TransitionHarness />);
    const message = screen.getByRole("document", {
      name: "Transition sandbox",
    });
    const initialFrame = renderedEmailFrame(message);

    fireEvent.click(
      screen.getByRole("button", { name: "Start suspended transition" }),
    );

    expect(renderedEmailFrame(message)).toBe(initialFrame);
    const anchor = renderedEmailRoot(message)?.querySelector("a");
    sendRendererMessage(message, {
      channel: "dakia-email-renderer",
      token: rendererToken(message),
      type: "external-link",
      href: anchor?.getAttribute("data-dakia-external-href"),
    });

    expect(committedMailto).toHaveBeenCalledOnce();
    expect(abandonedMailto).not.toHaveBeenCalled();
    suspendTransition?.();
  });

  it("replaces the sandbox only when remote-content permission changes its source", () => {
    const html =
      '<img src="https://images.example.test/consented.png" alt="Consented">';
    const rendered = render(
      <HtmlMessage
        html={html}
        title="Permission source"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );
    const message = screen.getByRole("document", {
      name: "Permission source",
    });
    const blockedFrame = renderedEmailFrame(message);
    expect(
      renderedEmailRoot(message)?.querySelector("img")?.hasAttribute("src"),
    ).toBe(false);

    rendered.rerender(
      <HtmlMessage
        html={html}
        title="Permission source"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        remoteContentAllowed
      />,
    );

    expect(renderedEmailFrame(message)).not.toBe(blockedFrame);
    expect(
      renderedEmailRoot(message)?.querySelector("img")?.getAttribute("src"),
    ).toBe("https://images.example.test/consented.png");
  });

  it("rejects an old renderer token after a source-changing permission grant", () => {
    const onMailto = vi.fn();
    const html =
      '<a href="mailto:permission@example.com">Write</a><img src="https://images.example.test/consented.png">';
    const rendered = render(
      <HtmlMessage
        html={html}
        title="Old capability"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        onMailto={onMailto}
      />,
    );
    const message = screen.getByRole("document", { name: "Old capability" });
    const oldToken = rendererToken(message);

    rendered.rerender(
      <HtmlMessage
        html={html}
        title="Old capability"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
        remoteContentAllowed
        onMailto={onMailto}
      />,
    );

    const frame = renderedEmailFrame(message);
    if (!frame?.contentWindow) {
      Object.defineProperty(frame, "contentWindow", {
        configurable: true,
        value: window,
      });
    }
    window.dispatchEvent(
      new MessageEvent("message", {
        data: {
          channel: "dakia-email-renderer",
          token: oldToken,
          type: "external-link",
          href: "mailto:permission@example.com",
        },
        source: frame?.contentWindow,
      }),
    );

    expect(onMailto).not.toHaveBeenCalled();
  });

  it("preserves an exact safe link target and leaves encoded unsafe schemes inert", () => {
    render(
      <HtmlMessage
        html={[
          '<a href="https&#58;//example.test/%E2%98%83?q=a%2Bb#part">Safe target</a>',
          '<a href="java&#x09;script:alert(1)">Script target</a>',
          '<a href="tauri&#58;//localhost/delete-all">Native target</a>',
          '<a href="file&#58;///etc/passwd">File target</a>',
        ].join("")}
        title="Link boundaries"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );

    const message = screen.getByRole("document", { name: "Link boundaries" });
    const root = renderedEmailRoot(message);
    const anchors = [...(root?.querySelectorAll("a") ?? [])];
    sendRendererMessage(message, {
      channel: "dakia-email-renderer",
      token: rendererToken(message),
      type: "external-link",
      href: anchors[0]?.getAttribute("data-dakia-external-href"),
    });
    for (const anchor of anchors.slice(1)) {
      sendRendererMessage(message, {
        channel: "dakia-email-renderer",
        token: rendererToken(message),
        type: "external-link",
        href: anchor.getAttribute("data-dakia-external-href"),
      });
    }

    expect(mocks.openExternal).toHaveBeenCalledOnce();
    expect(mocks.openExternal).toHaveBeenCalledWith(
      "https://example.test/%E2%98%83?q=a%2Bb#part",
    );
    for (const anchor of anchors.slice(1)) {
      expect(anchor.getAttribute("href")).toBeNull();
      expect(anchor.getAttribute("data-dakia-external-href")).toBeNull();
    }
  });

  it("accepts renderer messages only from the current sandbox and capability token", () => {
    const rendered = render(
      <HtmlMessage
        html='<a href="https://example.test/only-this-frame">Open</a>'
        title="Bridge boundaries"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );
    const message = screen.getByRole("document", { name: "Bridge boundaries" });
    const frame = renderedEmailFrame(message);
    expect(frame).not.toBeNull();
    if (!frame?.contentWindow) {
      Object.defineProperty(frame, "contentWindow", {
        configurable: true,
        value: window,
      });
    }
    const token = rendererToken(message);

    window.dispatchEvent(
      new MessageEvent("message", {
        data: {
          channel: "dakia-email-renderer",
          token: "forged-token",
          type: "external-link",
          href: "https://example.test/forged-token",
        },
        source: frame?.contentWindow,
      }),
    );
    window.dispatchEvent(
      new MessageEvent("message", {
        data: {
          channel: "dakia-email-renderer",
          token,
          type: "external-link",
          href: "https://example.test/forged-source",
        },
        source: null,
      }),
    );
    sendRendererMessage(message, {
      channel: "dakia-email-renderer",
      token,
      type: "external-link",
      href: "https://example.test/not-in-this-message",
    });
    expect(mocks.openExternal).not.toHaveBeenCalled();

    sendRendererMessage(message, {
      channel: "dakia-email-renderer",
      token,
      type: "external-link",
      href: "https://example.test/only-this-frame",
    });
    expect(mocks.openExternal).toHaveBeenCalledWith(
      "https://example.test/only-this-frame",
    );

    rendered.rerender(
      <HtmlMessage
        html='<a href="https://example.test/next-frame">Next</a>'
        title="Bridge boundaries"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );
    const nextFrame = renderedEmailFrame(message);
    if (!nextFrame?.contentWindow) {
      Object.defineProperty(nextFrame!, "contentWindow", {
        configurable: true,
        value: window,
      });
    }
    window.dispatchEvent(
      new MessageEvent("message", {
        data: {
          channel: "dakia-email-renderer",
          token,
          type: "external-link",
          href: "https://example.test/stale-frame",
        },
        source: nextFrame?.contentWindow,
      }),
    );
    expect(mocks.openExternal).toHaveBeenCalledOnce();
  });

  it("bounds opaque frame height while keeping long content internally scrollable", () => {
    render(
      <HtmlMessage
        html="<p>Resize me</p>"
        title="Resize boundaries"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );
    const message = screen.getByRole("document", { name: "Resize boundaries" });
    const frame = renderedEmailFrame(message);
    const token = rendererToken(message);

    expect(frame?.getAttribute("srcdoc")).toContain(
      "range.getBoundingClientRect()",
    );
    expect(frame?.getAttribute("srcdoc")).not.toContain("root?.scrollHeight");
    expect(frame).toHaveAttribute("scrolling", "auto");
    expect(frame?.style.overflow).toBe("auto");

    sendRendererMessage(message, {
      channel: "dakia-email-renderer",
      token,
      type: "resize",
      height: 5_999,
    });
    expect(frame?.style.height).toBe("5999px");
    sendRendererMessage(message, {
      channel: "dakia-email-renderer",
      token,
      type: "resize",
      height: 6_000,
    });
    expect(frame?.style.height).toBe("6000px");
    sendRendererMessage(message, {
      channel: "dakia-email-renderer",
      token,
      type: "resize",
      height: 6_001,
    });
    expect(frame?.style.height).toBe("6000px");

    sendRendererMessage(message, {
      channel: "dakia-email-renderer",
      token,
      type: "resize",
      height: 640,
    });
    expect(frame?.style.height).toBe("640px");
    sendRendererMessage(message, {
      channel: "dakia-email-renderer",
      token,
      type: "resize",
      height: 84,
    });
    expect(frame?.style.height).toBe("84px");
  });

  it("overrides sender inline root caps so long email content remains scrollable", () => {
    render(
      <HtmlMessage
        html={
          '<body style="height:100px !important;max-height:100px !important;overflow:hidden !important"><p>Start</p><p>Long content stays reachable.</p></body>'
        }
        title="Root cap boundaries"
        showHistoryLabel="Show history"
        hideHistoryLabel="Hide history"
      />,
    );
    const message = screen.getByRole("document", {
      name: "Root cap boundaries",
    });
    const rendered = renderedEmailRoot(message);
    const body = rendered?.querySelector("body") as HTMLBodyElement | null;

    expect(body?.style.getPropertyValue("height")).toBe("auto");
    expect(body?.style.getPropertyPriority("height")).toBe("important");
    expect(body?.style.getPropertyValue("max-height")).toBe("none");
    expect(body?.style.getPropertyPriority("max-height")).toBe("important");
    expect(body?.style.getPropertyValue("overflow-y")).toBe("auto");
    expect(body?.style.getPropertyPriority("overflow-y")).toBe("important");
    expect(rendered?.textContent).toContain("Long content stays reachable.");
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
