import { IconChevronDown } from "@tabler/icons-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import type { RefObject } from "react";
import { api } from "../api";

type Props = {
  html: string;
  title: string;
  showHistoryLabel: string;
  hideHistoryLabel: string;
  onMailto?: (href: string) => void;
};

type EmailDocument = {
  source: string;
  historySource?: string;
  dark: boolean;
};

const providerQuoteSelector = [
  ".gmail_quote",
  ".yahoo_quoted",
  ".freshdesk_quote",
  "[id^='AOLMsgPart_']",
].join(", ");
const recognizedQuoteSelector = [
  providerQuoteSelector,
  "[name='messageReplySection']",
  "blockquote[type='cite']",
  "[data-marker='__QUOTED_TEXT__']",
].join(", ");

const blockedElements = [
  "script",
  "iframe",
  "frame",
  "frameset",
  "object",
  "embed",
  "applet",
  "form",
  "input",
  "button",
  "textarea",
  "select",
  "option",
  "area",
  "meta",
  "base",
  "animate",
  "animateMotion",
  "animateTransform",
  "discard",
  "set",
];

const urlAttributes = new Set([
  "href",
  "src",
  "poster",
  "background",
  "action",
  "formaction",
  "xlink:href",
]);

function safeUrl(value: string, attribute: string) {
  const normalized = value.trim().replace(/[\u0000-\u001f\u007f\s]+/g, "");
  if (attribute === "href" && normalized.startsWith("#")) return true;
  if (/^(https?:|mailto:)/i.test(normalized)) return true;
  if (
    attribute !== "href" &&
    /^data:image\/(?:png|gif|jpe?g|webp|svg\+xml);/i.test(normalized)
  ) {
    return true;
  }
  return false;
}

export function buildEmailDocument(
  html: string,
  darkMode: boolean,
): EmailDocument {
  const document = new DOMParser().parseFromString(html, "text/html");
  document
    .querySelectorAll(blockedElements.join(","))
    .forEach((node) => node.remove());

  for (const element of document.querySelectorAll("*")) {
    for (const attribute of [...element.attributes]) {
      const name = attribute.name.toLowerCase();
      const value = attribute.value.trim();
      if (urlAttributes.has(name) && value.startsWith("//")) {
        element.setAttribute(attribute.name, `https:${value}`);
        continue;
      }
      if (
        name.startsWith("on") ||
        name === "srcdoc" ||
        (urlAttributes.has(name) && !safeUrl(attribute.value, name))
      ) {
        element.removeAttribute(attribute.name);
      }
      if (
        name === "style" &&
        /(?:expression\s*\(|javascript\s*:|behavior\s*:|-moz-binding)/i.test(
          attribute.value,
        )
      ) {
        element.removeAttribute(attribute.name);
      }
    }
  }

  for (const anchor of document.querySelectorAll<HTMLElement>("a")) {
    const href = (
      anchor.getAttribute("href") ??
      anchor.getAttribute("xlink:href") ??
      ""
    ).trim();
    if (!href) continue;
    if (href.startsWith("#")) {
      anchor.dataset.dakiaFragmentHref = href;
    } else {
      anchor.dataset.dakiaExternalHref = href;
    }
    anchor.removeAttribute("xlink:href");
    anchor.removeAttribute("href");
    anchor.removeAttribute("target");
    anchor.removeAttribute("download");
    anchor.setAttribute("role", "link");
    if (!anchor.hasAttribute("tabindex")) anchor.tabIndex = 0;
  }

  const dark = darkMode && supportsDarkAppearance(document);
  normalizePathologicalTypography(document);
  if (dark) normalizeNeutralPresentation(document);
  collapseEmptySpacers(document);
  if (!document.body.querySelector("table, img, picture, svg")) {
    document.body.classList.add("dakia-reader-content");
  }
  const history = extractQuotedHistory(document);

  const policy = document.createElement("meta");
  policy.httpEquiv = "Content-Security-Policy";
  policy.content =
    "default-src 'none'; script-src 'none'; object-src 'none'; frame-src 'none'; base-uri 'none'; form-action 'none'; img-src data: http: https:; style-src 'unsafe-inline' http: https:; font-src data: http: https:;";
  document.head.prepend(policy);

  installCompatibilityStyles(document, dark);

  const historySource = history
    ? documentSourceWithBody(document, history)
    : undefined;
  return {
    source: documentSource(document),
    historySource,
    dark,
  };
}

function installCompatibilityStyles(document: Document, dark: boolean) {
  const compatibilityStyles = document.createElement("style");
  compatibilityStyles.textContent = `
    :root { color-scheme: ${dark ? "dark" : "light"}; }
    html, body {
      margin: 0;
      padding: 0;
      min-width: 0;
      background: ${dark ? "#191a19" : "transparent"};
      color: ${dark ? "#d9dedb" : "CanvasText"};
      font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      font-size: 16px;
    }
    html { overflow-x: auto; overflow-y: hidden; }
    body {
      box-sizing: border-box;
      padding: 20px;
      overflow-wrap: anywhere;
      overflow-y: hidden;
    }
    body.dakia-reader-content > * { max-width: 72ch; }
    img { max-width: 100%; height: auto; }
    table { max-width: 100%; }
    pre { white-space: pre-wrap; }
    a[data-dakia-external-href],
    a[data-dakia-fragment-href] {
      cursor: pointer;
      text-decoration: underline;
    }
  `;
  document.head.append(compatibilityStyles);
}

function documentSource(document: Document) {
  return `<!doctype html>${document.documentElement.outerHTML}`;
}

function documentSourceWithBody(document: Document, content: HTMLElement) {
  const historyDocument = new DOMParser().parseFromString(
    documentSource(document),
    "text/html",
  );
  historyDocument.body.replaceChildren(content.cloneNode(true));
  return documentSource(historyDocument);
}

function extractQuotedHistory(document: Document) {
  const candidate =
    [
      ...document.querySelectorAll<HTMLElement>(
        [recognizedQuoteSelector, "#divRplyFwdMsg"].join(","),
      ),
    ].find(
      (element) =>
        !element.closest("details.dakia-quoted-history") &&
        hasMeaningfulQuotedContent(element),
    ) ?? null;
  const outlookHeader = findOutlookReplyHeader(document);
  let outlookBoundary: HTMLElement | null = null;
  if (
    outlookHeader &&
    (!candidate ||
      (candidate.compareDocumentPosition(outlookHeader) &
        Node.DOCUMENT_POSITION_PRECEDING) !==
        0)
  ) {
    const boundary = outlookQuoteBoundary(outlookHeader);
    if (!hasMeaningfulFollowingContent(boundary)) return null;
    outlookBoundary = boundary;
  } else if (!candidate) {
    return null;
  }

  const content = document.createElement("div");
  content.className = "dakia-history-content";

  if (outlookBoundary) {
    let node: Node | null = outlookBoundary;
    while (node) {
      const next: Node | null = node.nextSibling;
      content.append(node);
      node = next;
    }
  } else if (candidate?.id === "divRplyFwdMsg") {
    const nodes: Node[] = [candidate];
    let node = candidate.nextElementSibling;
    while (
      node?.matches(
        `blockquote, ${providerQuoteSelector}, [data-marker='__QUOTED_TEXT__']`,
      )
    ) {
      nodes.push(node);
      node = node.nextElementSibling;
    }
    nodes.forEach((item) => content.append(item));
  } else if (candidate) {
    const nodes = contiguousQuotedNodes(candidate);
    const citation = precedingCitation(nodes[0] as HTMLElement);
    if (citation) content.append(citation);
    nodes.forEach((node) => content.append(node));
  }
  return content.childNodes.length ? content : null;
}

function contiguousQuotedNodes(candidate: HTMLElement) {
  const nodes: ChildNode[] = [candidate];
  let previous = candidate.previousSibling;
  while (previous) {
    if (
      (previous.nodeType === Node.TEXT_NODE && !previous.textContent?.trim()) ||
      (previous.nodeType === Node.ELEMENT_NODE &&
        (previous as Element).matches(recognizedQuoteSelector))
    ) {
      nodes.unshift(previous);
      previous = previous.previousSibling;
      continue;
    }
    break;
  }

  let next = candidate.nextSibling;
  while (next) {
    if (
      (next.nodeType === Node.TEXT_NODE && !next.textContent?.trim()) ||
      (next.nodeType === Node.ELEMENT_NODE &&
        (next as Element).matches(recognizedQuoteSelector))
    ) {
      nodes.push(next);
      next = next.nextSibling;
      continue;
    }
    break;
  }
  return nodes;
}

// Word-generated Outlook desktop replies mark the start of quoted history
// with a top-border divider above a From/Sent/To/Subject header block.
function findOutlookReplyHeader(document: Document) {
  for (const element of document.querySelectorAll<HTMLElement>("div[style]")) {
    if (!/border-top\s*:/i.test(element.getAttribute("style") ?? "")) continue;
    const text = (element.textContent ?? "").replace(/\s+/g, " ").trim();
    if (
      /^From:\s*\S/i.test(text) &&
      /(?:Sent|To|Cc|Subject):\s*\S/i.test(text)
    ) {
      return element;
    }
  }
  return null;
}

// Word often nests the divider in layout-only wrapper divs; collapse from
// the outermost wrapper that adds no content of its own.
function outlookQuoteBoundary(element: HTMLElement) {
  let boundary = element;
  while (
    boundary.parentElement &&
    boundary.parentElement.childElementCount === 1 &&
    (boundary.parentElement.textContent ?? "").trim() ===
      (boundary.textContent ?? "").trim()
  ) {
    boundary = boundary.parentElement;
  }
  return boundary;
}

// The Outlook header separates new content from the quoted message below
// it, so the header and everything after it belongs to the history. A
// header with nothing meaningful after it (a stripped quote) stays visible.
function hasMeaningfulFollowingContent(boundary: HTMLElement) {
  let node = boundary.nextSibling;
  while (node) {
    if (node.nodeType === Node.ELEMENT_NODE) {
      const element = node as HTMLElement;
      if (element.querySelector("img, table, blockquote")) return true;
      if ((element.textContent ?? "").replace(/\s+/g, "")) return true;
    } else if (node.nodeType === Node.TEXT_NODE && node.textContent?.trim()) {
      return true;
    }
    node = node.nextSibling;
  }
  return false;
}

function hasMeaningfulQuotedContent(candidate: HTMLElement) {
  const text = candidate.textContent?.replace(/\s+/g, " ").trim() ?? "";
  if (!text) return false;
  if (isCitationLine(text)) return false;
  if (candidate.id === "divRplyFwdMsg") {
    return Boolean(
      candidate.nextElementSibling?.matches(
        `blockquote, ${providerQuoteSelector}, [data-marker='__QUOTED_TEXT__']`,
      ),
    );
  }
  if (candidate.matches(providerQuoteSelector)) {
    const quote = candidate.querySelector(
      `blockquote, [type='cite'], ${providerQuoteSelector}`,
    );
    if (quote && quote !== candidate && quote.textContent?.trim()) return true;
    return !isCitationLine(text);
  }
  return true;
}

function precedingCitation(candidate: HTMLElement) {
  let node: ChildNode | null = candidate.previousSibling;
  while (node?.nodeType === Node.TEXT_NODE && !node.textContent?.trim()) {
    node = node.previousSibling;
  }
  return node?.textContent && isCitationLine(node.textContent) ? node : null;
}

function isCitationLine(value: string) {
  return /^On\s.+(?:,\s*)?wrote:\s*$/i.test(value.replace(/\s+/g, " ").trim());
}

type ColorRole = "background" | "foreground";

function normalizeNeutralPresentation(document: Document) {
  for (const element of document.querySelectorAll<HTMLElement>("*")) {
    for (const attribute of ["bgcolor", "color"] as const) {
      const value = element.getAttribute(attribute);
      if (!value) continue;
      const normalized = darkNeutralColor(
        value,
        attribute === "bgcolor" ? "background" : "foreground",
      );
      if (normalized) element.setAttribute(attribute, normalized);
    }
    const style = element.getAttribute("style");
    if (style) element.setAttribute("style", normalizePresentationStyle(style));
  }
  for (const style of document.querySelectorAll("style")) {
    style.textContent = normalizePresentationStyle(style.textContent ?? "");
  }
}

function normalizePathologicalTypography(document: Document) {
  const style = document.body.getAttribute("style");
  if (style) document.body.setAttribute("style", capFontSizes(style));
}

function normalizePresentationStyle(style: string) {
  const colorDeclarations =
    /((?:^|[;{])\s*)((?:color|background(?:-color)?))(\s*:\s*)([^;}]+)/gi;
  return style.replace(
    colorDeclarations,
    (
      match,
      leading: string,
      property: string,
      colon: string,
      value: string,
    ) => {
      if (/\b(?:url|gradient)\s*\(/i.test(value)) return match;
      const role: ColorRole = /^background/i.test(property)
        ? "background"
        : "foreground";
      return `${leading}${property}${colon}${replaceNeutralColors(value, role)}`;
    },
  );
}

function capFontSizes(value: string) {
  return value.replace(
    /(font-size\s*:\s*)(-?\d+(?:\.\d+)?)px/gi,
    (match, prefix: string, size: string) =>
      Number(size) > 48 ? `${prefix}32px` : match,
  );
}

function replaceNeutralColors(value: string, role: ColorRole) {
  return value.replace(
    /#(?:[\da-f]{3}|[\da-f]{6})\b|rgba?\([^)]*\)|\b(?:white|black|gray|grey)\b/gi,
    (color) => darkNeutralColor(color, role) ?? color,
  );
}

function darkNeutralColor(value: string, role: ColorRole) {
  const rgb = parseRgb(value);
  if (!rgb || Math.max(...rgb) - Math.min(...rgb) > 18) return null;
  const luminance = (rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722) / 255;
  if (role === "background") {
    if (luminance > 0.72) return "#191a19";
    if (luminance > 0.32) return "#242625";
    return null;
  }
  if (luminance < 0.55) return "#d9dedb";
  return null;
}

function parseRgb(value: string): [number, number, number] | null {
  const normalized = value
    .trim()
    .replace(/\s*!important\s*$/i, "")
    .toLowerCase();
  if (normalized === "white") return [255, 255, 255];
  if (normalized === "black") return [0, 0, 0];
  if (normalized === "gray" || normalized === "grey") return [128, 128, 128];
  const hex = normalized.match(/^#([\da-f]{3}|[\da-f]{6})$/i)?.[1];
  if (hex) {
    const expanded =
      hex.length === 3 ? [...hex].map((part) => part + part).join("") : hex;
    return [
      Number.parseInt(expanded.slice(0, 2), 16),
      Number.parseInt(expanded.slice(2, 4), 16),
      Number.parseInt(expanded.slice(4, 6), 16),
    ];
  }
  const channels = normalized.match(
    /^rgba?\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)/i,
  );
  if (!channels) return null;
  const rgb = channels.slice(1, 4).map(Number);
  return rgb.every((channel) => channel >= 0 && channel <= 255)
    ? (rgb as [number, number, number])
    : null;
}

function collapseEmptySpacers(document: Document) {
  for (const row of [
    ...document.querySelectorAll<HTMLTableRowElement>("tr"),
  ].reverse()) {
    if (isEmptySpacerRow(row)) row.remove();
  }
  for (const element of [
    ...document.querySelectorAll<HTMLElement>(
      "[height], [style], [class], [id], p",
    ),
  ].reverse()) {
    const style = element.getAttribute("style") ?? "";
    const hasSpacerSignal =
      element.hasAttribute("height") || /\b(?:min-)?height\s*:/i.test(style);
    const bareEmptyParagraph =
      element.localName === "p" &&
      !element.id &&
      !element.classList.length &&
      !style &&
      !element.hasAttribute("height");
    if (element.matches("td, th, tr")) continue;
    if (
      (!hasSpacerSignal && !bareEmptyParagraph) ||
      hasMeaningfulSpacerContent(element) ||
      ((element.id || element.classList.length) && !isSpacerElement(element))
    ) {
      continue;
    }
    element.remove();
  }
}

function isEmptySpacerRow(row: HTMLTableRowElement) {
  if (hasMeaningfulSpacerContent(row)) return false;
  const cells = [
    ...row.querySelectorAll<HTMLElement>(":scope > td, :scope > th"),
  ];
  if (!cells.length) return false;
  return (
    isSpacerElement(row) ||
    cells.every((cell) => {
      const style = cell.getAttribute("style") ?? "";
      return (
        (isSpacerElement(cell) || (!cell.id && !cell.classList.length)) &&
        (cell.hasAttribute("height") ||
          /\b(?:min-)?height\s*:/i.test(style) ||
          isSpacerElement(cell))
      );
    })
  );
}

function isSpacerElement(element: HTMLElement) {
  return [...element.classList].some((className) =>
    /(?:^|[-_])spacer(?:$|[-_])/i.test(className),
  );
}

function hasMeaningfulSpacerContent(element: HTMLElement) {
  if ((element.textContent ?? "").replace(/[\s\u00a0]/g, "")) return true;
  if (element.querySelector("img, picture, svg, video, canvas, iframe, table"))
    return true;
  const style = element.getAttribute("style") ?? "";
  return (
    /\b(?:border(?:-[a-z]+)?|background(?:-color|-image)?)\s*:/i.test(style) ||
    element.hasAttribute("background") ||
    element.hasAttribute("bgcolor")
  );
}

function supportsDarkAppearance(document: Document) {
  if (document.querySelector("[background]")) return false;
  if (
    [...document.querySelectorAll("style")].some((element) =>
      /\b(?:background-image|url|gradient)\s*\(/i.test(
        element.textContent ?? "",
      ),
    )
  ) {
    return false;
  }
  for (const element of document.querySelectorAll<HTMLElement>(
    "[bgcolor], [style]",
  )) {
    const style = element.getAttribute("style") ?? "";
    if (/\b(?:background-image|url|gradient)\s*\(/i.test(style)) return false;
    const background =
      element.getAttribute("bgcolor") ??
      style.match(/\bbackground(?:-color)?\s*:\s*([^;]+)/i)?.[1];
    if (background && isExplicitDarkColor(background)) return false;
  }
  return !hasAmbiguousPalette(document);
}

function hasAmbiguousPalette(document: Document) {
  const declarations = /\b(?:color|background(?:-color)?)\s*:\s*([^;}]+)/gi;
  const values = [
    ...[...document.querySelectorAll<HTMLElement>("[style]")].map(
      (element) => element.getAttribute("style") ?? "",
    ),
    ...[...document.querySelectorAll("style")].map(
      (element) => element.textContent ?? "",
    ),
  ];
  return values.some((value) => {
    for (const declaration of value.matchAll(declarations)) {
      const palette = declaration[1].trim();
      if (
        /\b(?:var|hsla?|transparent)\s*\(/i.test(palette) ||
        /\btransparent\b/i.test(palette) ||
        /\brgb\(\s*\d+\s+\d+/i.test(palette)
      ) {
        return true;
      }
      const alpha = palette.match(
        /\brgba\(\s*\d+\s*,\s*\d+\s*,\s*\d+\s*,\s*([\d.]+)/i,
      )?.[1];
      if (alpha && Number(alpha) < 1) return true;
    }
    return false;
  });
}

function isExplicitDarkColor(value: string) {
  const rgb = parseRgb(value);
  if (!rgb) return false;
  return (rgb[0] + rgb[1] + rgb[2]) / 3 < 60;
}

function useShadowEmailDocument(
  host: RefObject<HTMLDivElement | null>,
  source?: string,
  slotLightDom = false,
  onMailto?: (href: string) => void,
) {
  useEffect(() => {
    const element = host.current;
    if (!element) return;
    if (!source) {
      element.replaceChildren();
      return;
    }
    const outerShadow =
      element.shadowRoot ?? element.attachShadow({ mode: "open" });
    const surface = window.document.createElement("div");
    surface.style.cssText = [
      "display:block!important",
      "position:static!important",
      "width:auto!important",
      "height:auto!important",
      "min-width:0!important",
      "margin:0!important",
      "padding:0!important",
      "border:0!important",
      "transform:none!important",
    ].join(";");
    const shadow = surface.attachShadow({ mode: "open" });
    const parsedDocument = new DOMParser().parseFromString(source, "text/html");
    const root = parsedDocument.documentElement;
    root.querySelector('meta[http-equiv="Content-Security-Policy"]')?.remove();
    shadow.replaceChildren(root);
    if (slotLightDom) {
      outerShadow.replaceChildren(
        surface,
        window.document.createElement("slot"),
      );
    } else {
      outerShadow.replaceChildren(surface);
    }

    const activateLink = (event: Event) => {
      if (event.type === "keydown" && (event as KeyboardEvent).key !== "Enter")
        return;
      if (event.type === "auxclick" && (event as MouseEvent).button !== 1)
        return;
      const anchor = event
        .composedPath()
        .find(
          (item): item is Element =>
            item instanceof Element && item.localName === "a",
        );
      if (!anchor) return;
      event.preventDefault();
      event.stopPropagation();
      const href = anchor.getAttribute("data-dakia-external-href");
      if (href && safeUrl(href, "href") && !href.startsWith("#")) {
        if (/^mailto:/i.test(href) && onMailto) {
          onMailto(href);
          return;
        }
        void api
          .openExternal(href)
          .catch((error) =>
            console.error(
              "Could not open email link in the default browser",
              error,
            ),
          );
        return;
      }
      const fragment = anchor.getAttribute("data-dakia-fragment-href");
      if (!fragment?.startsWith("#")) return;
      let id = fragment.slice(1);
      try {
        id = decodeURIComponent(id);
      } catch {
        return;
      }
      shadow.getElementById(id)?.scrollIntoView({ block: "nearest" });
    };
    shadow.addEventListener("click", activateLink, true);
    shadow.addEventListener("auxclick", activateLink, true);
    shadow.addEventListener("keydown", activateLink, true);
    return () => {
      shadow.removeEventListener("click", activateLink, true);
      shadow.removeEventListener("auxclick", activateLink, true);
      shadow.removeEventListener("keydown", activateLink, true);
    };
  }, [host, onMailto, slotLightDom, source]);
}

export function HtmlMessage({
  html,
  title,
  showHistoryLabel,
  hideHistoryLabel,
  onMailto,
}: Props) {
  const host = useRef<HTMLDivElement>(null);
  const historyHost = useRef<HTMLDivElement>(null);
  const historyId = useId();
  const [historyVisible, setHistoryVisible] = useState(false);
  const [darkMode, setDarkMode] = useState(
    () => document.documentElement.dataset.mantineColorScheme === "dark",
  );
  const email = useMemo(
    () => buildEmailDocument(html, darkMode),
    [darkMode, html],
  );

  useEffect(() => {
    const root = document.documentElement;
    const updateDarkMode = () => {
      setDarkMode(root.dataset.mantineColorScheme === "dark");
    };
    const observer = new MutationObserver(updateDarkMode);
    observer.observe(root, {
      attributes: true,
      attributeFilter: ["data-mantine-color-scheme"],
    });
    updateDarkMode();
    return () => observer.disconnect();
  }, []);

  useEffect(() => setHistoryVisible(false), [html]);
  useShadowEmailDocument(host, email.source, true, onMailto);
  useShadowEmailDocument(historyHost, email.historySource, false, onMailto);

  return (
    <div
      ref={host}
      className={`html-message${email.dark ? " html-message--dark" : ""}`}
      role="document"
      aria-label={title}
      style={{ contain: "layout paint", isolation: "isolate" }}
    >
      {email.historySource && (
        <div style={{ marginTop: 8 }}>
          <button
            type="button"
            className="html-message-history-toggle"
            aria-expanded={historyVisible}
            aria-controls={historyId}
            onClick={() => setHistoryVisible((visible) => !visible)}
            style={{
              alignItems: "center",
              background: "transparent",
              border: 0,
              borderRadius: 8,
              color: "inherit",
              cursor: "pointer",
              display: "inline-flex",
              font: "inherit",
              fontWeight: 600,
              gap: 7,
              minHeight: 44,
              padding: "8px 10px",
            }}
          >
            <IconChevronDown
              size={18}
              aria-hidden="true"
              style={{
                transform: historyVisible ? "rotate(180deg)" : "rotate(0deg)",
                transition: "transform 120ms ease",
              }}
            />
            {historyVisible ? hideHistoryLabel : showHistoryLabel}
          </button>
          <div
            id={historyId}
            aria-hidden={!historyVisible}
            style={{ display: historyVisible ? "block" : "none" }}
          >
            <div ref={historyHost} data-dakia-email-surface="history" />
          </div>
        </div>
      )}
    </div>
  );
}
