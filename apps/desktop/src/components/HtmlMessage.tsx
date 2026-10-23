import { IconChevronDown } from "@tabler/icons-react";
import {
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import type { RefObject } from "react";
import { api } from "../api";
import { cloneInertHtmlDocument, parseInertHtml } from "../inertHtml";

type Props = {
  html: string;
  title: string;
  showHistoryLabel: string;
  hideHistoryLabel: string;
  remoteContentAllowed?: boolean;
  loadRemoteContentLabel?: string;
  onLoadRemoteContent?: () => void;
  onMailto?: (href: string) => void;
};

type EmailDocument = {
  source: string;
  document: Document;
  historySource?: string;
  historyDocument?: Document;
  dark: boolean;
  hasBlockedRemoteContent: boolean;
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
const urlBearingSvgAttributes = new Set([
  "filter",
  "fill",
  "stroke",
  "mask",
  "clip-path",
  "marker",
  "marker-start",
  "marker-mid",
  "marker-end",
  "cursor",
]);

function normalizeUrl(value: string) {
  return value.trim().replace(/[\u0000-\u001f\u007f\s]+/g, "");
}

function safeUrl(value: string, attribute: string) {
  const normalized = normalizeUrl(value);
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

function safeEmbeddedImageUrl(value: string) {
  return /^data:image\/(?:png|gif|jpe?g|webp|svg\+xml);/i.test(
    normalizeUrl(value),
  );
}

function externalResourceUrl(value: string) {
  const normalized = normalizeUrl(value);
  if (/^\/\//.test(normalized)) return `https:${normalized}`;
  return /^https?:/i.test(normalized) ? normalized : undefined;
}

function isNavigationAttribute(element: Element, attribute: string) {
  return (
    (attribute === "href" || attribute === "xlink:href") &&
    element.localName === "a"
  );
}

function isImageSourceAttribute(element: Element, attribute: string) {
  return element.localName === "img" && attribute === "src";
}

function sanitizedResourceUrl(
  element: Element,
  attribute: string,
  value: string,
  allowRemoteContent: boolean,
) {
  // Consent permits only ordinary image sources. Do not let a message use it
  // to start media playback or fetch SVG subresources. This sanitizer remains
  // independent of CSP so the resource allowlist is enforced before mounting.
  if (!isImageSourceAttribute(element, attribute)) return undefined;
  if (safeEmbeddedImageUrl(value)) return value.trim();
  return allowRemoteContent ? externalResourceUrl(value) : undefined;
}

function sanitizeCssUrls(value: string) {
  // CSS parsing has too many resource syntaxes to safely reconstruct with
  // regular expressions. Email CSS may still control layout and typography,
  // but it may never load a resource, even after image consent.
  if (/\\|\/\*/.test(value)) return { value: "", blocked: true };
  let blocked = false;
  const sanitized = value
    .replace(/@import\b[\s\S]*?(?:;|$)/gi, () => {
      blocked = true;
      return "";
    })
    .replace(
      /(^|[;{])[^;{}]*(?:url\s*\(|(?:-webkit-)?image-set\s*\(|cross-fade\s*\(|image\s*\(|paint\s*\(|element\s*\(|(?:asset|tauri|ipc|file|data|javascript|vbscript)\s*:)[^;{}]*(?=;|}|$)/gi,
      (_declaration, delimiter: string) => {
        blocked = true;
        return delimiter;
      },
    )
    .replace(
      /(?:expression\s*\([^;]*\)|(?:behavior|-moz-binding)\s*:[^;]*;?)/gi,
      () => {
        blocked = true;
        return "";
      },
    );
  return { value: sanitized, blocked };
}

function sanitizeSrcSet(_value: string) {
  // srcset permits data URLs containing commas, making a partial parser easy
  // to bypass. Keep the normal src allowlist and drop srcset until it has a
  // standards-complete parser. This also prevents asset:, tauri:, file:, and
  // other local/native schemes after a user permits ordinary remote content.
  return undefined;
}

export function buildEmailDocument(
  html: string,
  darkMode: boolean,
  allowRemoteContent = false,
): EmailDocument {
  const document = parseInertHtml(html);
  const dark = darkMode && supportsDarkAppearance(document);
  document
    .querySelectorAll(blockedElements.join(","))
    .forEach((node) => node.remove());

  let hasBlockedRemoteContent = false;
  for (const link of document.querySelectorAll<HTMLLinkElement>("link")) {
    const href = link.getAttribute("href") ?? "";
    if (href && !allowRemoteContent) hasBlockedRemoteContent = true;
    link.remove();
  }

  for (const element of document.querySelectorAll("*")) {
    for (const attribute of [...element.attributes]) {
      const name = attribute.name.toLowerCase();
      const value = attribute.value.trim();
      if (name === "srcset") {
        const safe = sanitizeSrcSet(attribute.value);
        if (safe) element.setAttribute(attribute.name, safe);
        else {
          if (value && !allowRemoteContent) hasBlockedRemoteContent = true;
          element.removeAttribute(attribute.name);
        }
        continue;
      }
      if (name.startsWith("on") || name === "srcdoc") {
        element.removeAttribute(attribute.name);
        continue;
      }
      if (name === "referrerpolicy") {
        element.removeAttribute(attribute.name);
        continue;
      }
      if (
        urlBearingSvgAttributes.has(name) ||
        /url\s*\(/i.test(attribute.value)
      ) {
        const sanitized = sanitizeCssUrls(attribute.value);
        if (sanitized.blocked) hasBlockedRemoteContent = true;
        element.setAttribute(attribute.name, sanitized.value);
        continue;
      }
      if (
        (urlAttributes.has(name) &&
          !isNavigationAttribute(element, name) &&
          !sanitizedResourceUrl(
            element,
            name,
            attribute.value,
            allowRemoteContent,
          )) ||
        (urlAttributes.has(name) &&
          isNavigationAttribute(element, name) &&
          !safeUrl(attribute.value, name))
      ) {
        if (
          urlAttributes.has(name) &&
          !isNavigationAttribute(element, name) &&
          value
        ) {
          hasBlockedRemoteContent = true;
        }
        element.removeAttribute(attribute.name);
        continue;
      }
      if (urlAttributes.has(name) && !isNavigationAttribute(element, name)) {
        const safe = sanitizedResourceUrl(
          element,
          name,
          attribute.value,
          allowRemoteContent,
        );
        if (safe) element.setAttribute(attribute.name, safe);
      }
      if (
        name === "style" &&
        /(?:expression\s*\(|javascript\s*:|behavior\s*:|-moz-binding)/i.test(
          attribute.value,
        )
      ) {
        element.removeAttribute(attribute.name);
        continue;
      }
      if (name === "style") {
        const sanitized = sanitizeCssUrls(attribute.value);
        if (sanitized.blocked) hasBlockedRemoteContent = true;
        element.setAttribute(attribute.name, sanitized.value);
      }
    }
  }

  for (const style of document.querySelectorAll("style")) {
    const sanitized = sanitizeCssUrls(style.textContent ?? "");
    if (sanitized.blocked) hasBlockedRemoteContent = true;
    style.textContent = sanitized.value;
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

  normalizePathologicalTypography(document);
  if (dark) normalizeNeutralPresentation(document);
  const history = extractQuotedHistory(document);
  prepareReaderBody(document);

  const policy = document.createElement("meta");
  policy.httpEquiv = "Content-Security-Policy";
  policy.content =
    "default-src 'none'; script-src 'none'; object-src 'none'; frame-src 'none'; base-uri 'none'; form-action 'none'; " +
    (allowRemoteContent
      ? "img-src data: http: https:; style-src 'unsafe-inline'; font-src data:;"
      : "img-src data:; style-src 'unsafe-inline'; font-src data:;");
  document.head.prepend(policy);

  installCompatibilityStyles(document, dark);

  const historySource = history
    ? documentSourceWithBody(document, history)
    : undefined;
  const historyDocument = history
    ? documentWithBody(document, history)
    : undefined;
  return {
    source: documentSource(document),
    document,
    historySource,
    historyDocument,
    dark,
    hasBlockedRemoteContent: !allowRemoteContent && hasBlockedRemoteContent,
  };
}

function installCompatibilityStyles(document: Document, dark: boolean) {
  const compatibilityStyles = document.createElement("style");
  compatibilityStyles.textContent = `
    html { color-scheme: ${dark ? "dark" : "light"}; }
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
      overflow-wrap: normal;
      overflow-y: hidden;
    }
    body.dakia-reader-content { padding: 20px; overflow-wrap: anywhere; }
    body.dakia-reader-content > * { max-width: 72ch; }
    img { max-width: 100%; height: auto; }
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

function documentWithBody(document: Document, content: HTMLElement) {
  const historyDocument = cloneInertHtmlDocument(document);
  historyDocument.body.replaceChildren(content.cloneNode(true));
  prepareReaderBody(historyDocument);
  return historyDocument;
}

function documentSourceWithBody(document: Document, content: HTMLElement) {
  return documentSource(documentWithBody(document, content));
}

function prepareReaderBody(document: Document) {
  // Classify the visible surface independently of any extracted quoted mail.
  // Images alone may just be a signature or an invisible tracking pixel.
  const designedLayout = !!document.body.querySelector("table");
  document.body.classList.toggle("dakia-reader-content", !designedLayout);
  // Empty cells and rows are intentional layout primitives in HTML email.
  if (!designedLayout) collapseEmptySpacers(document);
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
  if (!rgb) return null;
  const spread = Math.max(...rgb) - Math.min(...rgb);
  // Receipts commonly use muted blue-grey ink rather than strictly grey text.
  const mutedInk =
    role === "foreground" &&
    spread <= 48 &&
    rgb[2] >= rgb[0] &&
    rgb[2] >= rgb[1];
  if (spread > 18 && !mutedInk) return null;
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
  const media = "img, picture, svg, video, canvas, iframe, table";
  if (element.matches(media) || element.querySelector(media)) return true;
  const style = element.getAttribute("style") ?? "";
  return (
    /\b(?:border(?:-[a-z]+)?|background(?:-color|-image)?)\s*:/i.test(style) ||
    element.hasAttribute("background") ||
    element.hasAttribute("bgcolor")
  );
}

function supportsDarkAppearance(document: Document) {
  // A dark logo or decorative background must not veto adaptation of an
  // otherwise light receipt. Keep the conservative fallback when there is no
  // known light surface to adapt, or when the palette cannot be resolved.
  if (hasAmbiguousPalette(document)) return false;
  const outerSurface = document.body.firstElementChild;
  if (
    [document.documentElement, document.body, outerSurface].some((element) => {
      if (!element) return false;
      const background =
        element.getAttribute("bgcolor") ??
        element
          .getAttribute("style")
          ?.match(/\bbackground(?:-color)?\s*:\s*([^;]+)/i)?.[1];
      return !!background && isExplicitDarkColor(background);
    }) ||
    hasAuthoredDarkSurface(document) ||
    hasLightArtworkSurface(document) ||
    hasImageBackedText(document)
  )
    return false;
  const lightSurface = [...document.querySelectorAll("*")].some((element) => {
    const background = element.getAttribute("bgcolor");
    if (background && darkNeutralColor(background, "background")) return true;
    const css =
      element.localName === "style"
        ? (element.textContent ?? "")
        : (element.getAttribute("style") ?? "");
    return [...css.matchAll(/\bbackground(?:-color)?\s*:\s*([^;}]+)/gi)].some(
      ([, value]) => !!darkNeutralColor(value, "background"),
    );
  });
  if (lightSurface) return true;
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

function hasLightArtworkSurface(document: Document) {
  // Transparent artwork (for example a black payment code) may depend on its
  // white backing. Without inspecting pixels, preserve its authored palette.
  const artwork = [...document.querySelectorAll("*")].filter(
    (element) =>
      !element.textContent?.trim() &&
      [
        ...element.querySelectorAll(
          ":scope > img, :scope > picture, :scope > svg, :scope > a > img",
        ),
      ].some((image) => {
        const width = Number(image.getAttribute("width"));
        const height = Number(image.getAttribute("height"));
        return !(width > 0 && width <= 2 && height > 0 && height <= 2);
      }),
  );
  const lightBackground = (css: string) =>
    [...css.matchAll(/\bbackground(?:-color)?\s*:\s*([^;}]+)/gi)].some(
      ([, value]) => !!darkNeutralColor(value, "background"),
    );
  if (
    artwork.some((element) => {
      const background = element.getAttribute("bgcolor");
      return (
        (background && darkNeutralColor(background, "background")) ||
        lightBackground(element.getAttribute("style") ?? "")
      );
    })
  )
    return true;
  for (const style of document.querySelectorAll("style")) {
    const css = (style.textContent ?? "").replace(/\/\*[\s\S]*?\*\//g, "");
    for (const [, selector, declarations] of css.matchAll(
      /([^{}]+)\{([^{}]*)\}/g,
    )) {
      if (!lightBackground(declarations)) continue;
      try {
        if (artwork.some((element) => element.matches(selector.trim())))
          return true;
      } catch {
        return true;
      }
    }
  }
  return false;
}

function hasAuthoredDarkSurface(document: Document) {
  const surfaces = [
    document.documentElement,
    document.body,
    document.body.firstElementChild,
  ];
  for (const style of document.querySelectorAll("style")) {
    const css = (style.textContent ?? "").replace(/\/\*[\s\S]*?\*\//g, "");
    for (const [, selector, declarations] of css.matchAll(
      /([^{}]+)\{([^{}]*)\}/g,
    )) {
      const darkBackground = [
        ...declarations.matchAll(/\bbackground(?:-color)?\s*:\s*([^;}]+)/gi),
      ].some(([, value]) => isExplicitDarkColor(value));
      if (!darkBackground) continue;
      try {
        if (surfaces.some((element) => element?.matches(selector.trim())))
          return true;
      } catch {
        return true;
      }
    }
  }
  return false;
}

function hasImageBackedText(document: Document) {
  const hasText = (element: Element) => !!element.textContent?.trim();
  const imageBackground =
    /\b(?:url|(?:repeating-)?(?:linear|radial|conic)-gradient)\s*\(/i;
  for (const element of document.querySelectorAll("[background], [style]")) {
    if (
      hasText(element) &&
      (element.hasAttribute("background") ||
        imageBackground.test(element.getAttribute("style") ?? ""))
    )
      return true;
  }
  // Conservatively inspect every rule, including inactive media branches.
  // Stripe includes image rules for absent template elements; those must not
  // disable dark mode for the whole receipt. Unparseable selectors stay light.
  for (const style of document.querySelectorAll("style")) {
    const css = (style.textContent ?? "").replace(/\/\*[\s\S]*?\*\//g, "");
    for (const [, selector, declarations] of css.matchAll(
      /([^{}]+)\{([^{}]*)\}/g,
    )) {
      if (
        !imageBackground.test(declarations) ||
        /^\s*@font-face\b/i.test(selector)
      )
        continue;
      try {
        if ([...document.querySelectorAll(selector.trim())].some(hasText))
          return true;
      } catch {
        return true;
      }
    }
  }
  return false;
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

type RendererMessage =
  | {
      channel: "dakia-email-renderer";
      token: string;
      type: "resize";
      height: unknown;
    }
  | {
      channel: "dakia-email-renderer";
      token: string;
      type: "external-link";
      href: unknown;
    };

const rendererChannel = "dakia-email-renderer";
// A mail body must not reserve arbitrary application height. At the cap the
// isolated document keeps its own ordinary scroll container, so long mail
// stays reachable without pushing Reader controls away.
const maxRendererHeight = 6_000;

function approvedExternalLinks(document: Document) {
  return new Set(
    [...document.querySelectorAll("a[data-dakia-external-href]")]
      .map((anchor) => anchor.getAttribute("data-dakia-external-href") ?? "")
      .filter((href) => safeUrl(href, "href") && !href.startsWith("#")),
  );
}

function hasTrustedUserActivation() {
  // Opaque iframe messages have the `null` origin, so origin is not a useful
  // signal. A current WebKit user activation prevents a retained capability
  // from opening a sender-selected link outside an actual click or Enter key.
  return !navigator.userActivation || navigator.userActivation.isActive;
}

function rendererToken() {
  const bytes = new Uint8Array(24);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join(
    "",
  );
}

// This exact source is authorized by a SHA-256 CSP hash in both the isolated
// document and the packaged Tauri policy. Keep the capability out of these
// bytes: each trusted script element carries its own random token attribute.
const rendererBootstrapSource = String.raw`(() => {
    const token = document.currentScript?.getAttribute("data-dakia-renderer-token");
    if (!/^[a-f0-9]{48}$/.test(token ?? "")) return;
    const channel = "dakia-email-renderer";
    const post = (type, payload) => parent.postMessage({ channel, token, type, ...payload }, "*");
    const contentHeight = () => {
      const body = document.body;
      if (!body) return 0;
      const range = document.createRange();
      range.selectNodeContents(body);
      const bounds = range.getBoundingClientRect();
      const style = getComputedStyle(body);
      return bounds.height + (parseFloat(style.paddingTop) || 0) + (parseFloat(style.paddingBottom) || 0);
    };
    const reportSize = () => {
      post("resize", { height: contentHeight() });
    };
    const onLink = (event) => {
      if (event.type === "keydown" && event.key !== "Enter") return;
      if (event.type === "auxclick" && event.button !== 1) return;
      if (!event.isTrusted || (navigator.userActivation && !navigator.userActivation.isActive)) return;
      const path = event.composedPath ? event.composedPath() : [];
      const anchor = path.find((item) => item && item.nodeType === Node.ELEMENT_NODE && item.localName === "a");
      if (!anchor) return;
      const fragment = anchor.getAttribute("data-dakia-fragment-href");
      if (fragment && fragment.startsWith("#")) {
        event.preventDefault();
        event.stopPropagation();
        let id = fragment.slice(1);
        try { id = decodeURIComponent(id); } catch { return; }
        document.getElementById(id)?.scrollIntoView({ block: "nearest" });
        return;
      }
      const href = anchor.getAttribute("data-dakia-external-href");
      if (!href) return;
      event.preventDefault();
      event.stopPropagation();
      post("external-link", { href });
    };
    addEventListener("click", onLink, true);
    addEventListener("auxclick", onLink, true);
    addEventListener("keydown", onLink, true);
    addEventListener("load", () => requestAnimationFrame(reportSize), { once: true });
    document.addEventListener("load", () => requestAnimationFrame(reportSize), true);
    if (typeof ResizeObserver === "function") {
      const observer = new ResizeObserver(() => requestAnimationFrame(reportSize));
      if (document.documentElement) observer.observe(document.documentElement);
      if (document.body) observer.observe(document.body);
    }
    if (typeof MutationObserver === "function" && document.documentElement) {
      new MutationObserver(() => requestAnimationFrame(reportSize)).observe(document.documentElement, { childList: true, subtree: true, attributes: true, characterData: true });
    }
    requestAnimationFrame(reportSize);
  })();`;

// SHA-256 over the UTF-8 bytes of rendererBootstrapSource. This must stay in
// lock-step with Tauri's outer CSP because srcdoc inherits the app policy.
const rendererBootstrapCspHash =
  "sha256-DQ9sEW+p9sEcZuLPMT1IkscbxbaaZo680QmKWLgSjoo=";

function sandboxedEmailSource(source: Document, token: string) {
  const document = cloneInertHtmlDocument(source);
  document
    .querySelectorAll('meta[name="referrer" i], meta[http-equiv="referrer" i]')
    .forEach((meta) => meta.remove());
  const referrer = document.createElement("meta");
  referrer.name = "referrer";
  referrer.content = "no-referrer";
  // The browser processes srcdoc from top to bottom. This must precede every
  // resource-bearing node so an author-supplied policy cannot reveal the app
  // URL while a consented image is requested.
  document.head.prepend(referrer);
  const policy = document.querySelector<HTMLMetaElement>(
    'meta[http-equiv="Content-Security-Policy"]',
  );
  if (!policy) throw new Error("Sanitized email is missing its CSP");
  policy.content = `${policy.content
    .replace(/script-src\s+'none'/i, `script-src '${rendererBootstrapCspHash}'`)
    .replace(
      /font-src\s+data:/i,
      "font-src 'none'",
    )}; connect-src 'none'; child-src 'none'; worker-src 'none'; media-src 'none'`;
  const script = document.createElement("script");
  // Sender scripts are removed before this trusted element is appended. The
  // bootstrap reads only its own attribute through currentScript, so sender
  // data attributes and IDs cannot name or retain the capability.
  script.setAttribute("data-dakia-renderer-token", token);
  script.textContent = rendererBootstrapSource;
  // An inline sender declaration such as `body { height: 100px !important;
  // overflow: hidden !important }` would otherwise beat our stylesheet and
  // make content below the frame cap unreachable. These are trusted inline
  // root constraints, applied after sanitising sender markup.
  for (const root of [document.documentElement, document.body]) {
    root.style.setProperty("height", "auto", "important");
    root.style.setProperty("min-height", "0", "important");
    root.style.setProperty("max-height", "none", "important");
    root.style.setProperty("overflow-y", "auto", "important");
    root.style.setProperty("overflow-x", "auto", "important");
  }
  const scrollPolicy = document.createElement("style");
  scrollPolicy.setAttribute("data-dakia-renderer-scroll", "");
  scrollPolicy.textContent =
    "html, body { overflow-y: auto !important; overflow-x: auto !important; }";
  document.head.append(scrollPolicy);
  document.head.append(script);
  return documentSource(document);
}

function useSandboxedEmailDocument(
  host: RefObject<HTMLDivElement | null>,
  emailDocument?: Document,
  source?: string,
  slotLightDom = false,
  onMailtoRef?: RefObject<((href: string) => void) | undefined>,
) {
  const committedSourceRef = useRef<string | undefined>(source);
  useLayoutEffect(() => {
    committedSourceRef.current = source;
  }, [source]);

  useLayoutEffect(() => {
    const element = host.current;
    if (!element) return;
    if (!emailDocument || !source) {
      element.shadowRoot?.replaceChildren();
      element.replaceChildren();
      return;
    }
    const outerShadow =
      element.shadowRoot ?? element.attachShadow({ mode: "open" });
    const token = rendererToken();
    const allowedLinks = approvedExternalLinks(emailDocument);
    const frame = window.document.createElement("iframe");
    frame.setAttribute("sandbox", "allow-scripts");
    frame.setAttribute("referrerpolicy", "no-referrer");
    frame.setAttribute("scrolling", "auto");
    frame.setAttribute("title", "Email content");
    frame.style.cssText = [
      "display:block!important",
      "width:100%!important",
      "min-width:0!important",
      "height:1px!important",
      "border:0!important",
      "margin:0!important",
      "padding:0!important",
      "overflow:auto!important",
    ].join(";");
    frame.setAttribute("srcdoc", sandboxedEmailSource(emailDocument, token));
    if (slotLightDom) {
      outerShadow.replaceChildren(frame, window.document.createElement("slot"));
    } else {
      outerShadow.replaceChildren(frame);
    }
    const onRendererMessage = (event: MessageEvent<RendererMessage>) => {
      if (event.source !== frame.contentWindow) return;
      const message = event.data;
      if (
        !message ||
        message.channel !== rendererChannel ||
        message.token !== token ||
        committedSourceRef.current !== source
      ) {
        return;
      }
      if (message.type === "resize") {
        const height = Number(message.height);
        if (!Number.isFinite(height) || height < 0) return;
        frame.style.height = `${Math.min(Math.ceil(height), maxRendererHeight)}px`;
        return;
      }
      if (message.type !== "external-link" || typeof message.href !== "string")
        return;
      const href = message.href;
      if (
        !hasTrustedUserActivation() ||
        !safeUrl(href, "href") ||
        href.startsWith("#") ||
        !allowedLinks.has(href)
      ) {
        return;
      }
      const onMailto = onMailtoRef?.current;
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
    };
    window.addEventListener("message", onRendererMessage);
    return () => {
      window.removeEventListener("message", onRendererMessage);
    };
  }, [emailDocument, host, onMailtoRef, slotLightDom, source]);
}

export function HtmlMessage({
  html,
  title,
  showHistoryLabel,
  hideHistoryLabel,
  remoteContentAllowed = false,
  loadRemoteContentLabel,
  onLoadRemoteContent,
  onMailto,
}: Props) {
  const host = useRef<HTMLDivElement>(null);
  const historyHost = useRef<HTMLDivElement>(null);
  // Parent Reader state creates a new closure on ordinary rerenders. Commit
  // the bridge callback after React has committed the matching visible tree.
  // Writing this ref during render would let an abandoned concurrent render
  // redirect the frame that remains visible.
  const onMailtoRef = useRef(onMailto);
  useLayoutEffect(() => {
    onMailtoRef.current = onMailto;
  }, [onMailto]);
  const historyId = useId();
  const [historyVisible, setHistoryVisible] = useState(false);
  const [darkMode, setDarkMode] = useState(
    () => document.documentElement.dataset.mantineColorScheme === "dark",
  );
  const email = useMemo(
    () => buildEmailDocument(html, darkMode, remoteContentAllowed),
    [darkMode, html, remoteContentAllowed],
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
  useSandboxedEmailDocument(
    host,
    email.document,
    email.source,
    true,
    onMailtoRef,
  );
  useSandboxedEmailDocument(
    historyHost,
    email.historyDocument,
    email.historySource,
    false,
    onMailtoRef,
  );

  return (
    <div
      ref={host}
      className={`html-message${email.dark ? " html-message--dark" : ""}`}
      role="document"
      aria-label={title}
      style={{ contain: "layout paint", isolation: "isolate" }}
    >
      {email.hasBlockedRemoteContent &&
      onLoadRemoteContent &&
      loadRemoteContentLabel ? (
        <button
          type="button"
          className="html-message-remote-content-toggle"
          onClick={onLoadRemoteContent}
          style={{
            background: "var(--dakia-email-control-surface)",
            borderColor: "var(--dakia-email-control-border)",
            borderStyle: "solid",
            borderWidth: 1,
            borderRadius: 8,
            color: "var(--dakia-email-control-ink)",
            cursor: "pointer",
            font: "inherit",
            marginTop: 8,
            minHeight: 36,
            padding: "6px 10px",
          }}
        >
          {loadRemoteContentLabel}
        </button>
      ) : null}
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
              background: "var(--dakia-email-control-surface)",
              borderColor: "var(--dakia-email-control-border)",
              borderStyle: "solid",
              borderWidth: 1,
              borderRadius: 8,
              color: "var(--dakia-email-control-ink)",
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
