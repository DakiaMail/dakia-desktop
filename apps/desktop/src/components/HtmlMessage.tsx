import { useEffect, useMemo, useRef, useState } from "react";
import { api } from "../api";

type Props = {
  html: string;
  title: string;
  showHistoryLabel: string;
  hideHistoryLabel: string;
};

type EmailDocument = {
  source: string;
  dark: boolean;
};

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
  historyLabels = { show: "Show history", hide: "Hide history" },
): EmailDocument {
  const document = new DOMParser().parseFromString(html, "text/html");
  const dark = darkMode && supportsDarkAppearance(document);
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

  collapseQuotedHistory(document, historyLabels);

  const policy = document.createElement("meta");
  policy.httpEquiv = "Content-Security-Policy";
  policy.content =
    "default-src 'none'; script-src 'none'; object-src 'none'; frame-src 'none'; base-uri 'none'; form-action 'none'; img-src data: http: https:; style-src 'unsafe-inline' http: https:; font-src data: http: https:;";
  document.head.prepend(policy);

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
    }
    html { overflow-x: auto; overflow-y: hidden; }
    body { overflow-wrap: anywhere; overflow-y: hidden; }
    img { max-width: 100%; height: auto; }
    table { max-width: 100%; }
    pre { white-space: pre-wrap; }
    a[data-dakia-external-href],
    a[data-dakia-fragment-href] {
      cursor: pointer;
      text-decoration: underline;
    }
    details.dakia-quoted-history {
      display: block !important;
      visibility: visible !important;
      opacity: 1 !important;
      margin-top: 1em;
    }
    details.dakia-quoted-history > summary {
      display: list-item !important;
      visibility: visible !important;
      opacity: 1 !important;
      color: #2474c6;
      cursor: pointer;
      font: 600 13px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      list-style-position: inside;
    }
    details.dakia-quoted-history[open] .dakia-history-show,
    details.dakia-quoted-history:not([open]) .dakia-history-hide {
      display: none;
    }
    details.dakia-quoted-history:not([open]) > .dakia-history-content {
      display: none !important;
    }
    .dakia-history-content { margin-top: .75em; }
  `;
  document.head.append(compatibilityStyles);
  return {
    source: `<!doctype html>${document.documentElement.outerHTML}`,
    dark,
  };
}

function collapseQuotedHistory(
  document: Document,
  labels: { show: string; hide: string },
) {
  const candidate = document.querySelector<HTMLElement>(
    [
      ".gmail_quote",
      ".yahoo_quoted",
      "[name='messageReplySection']",
      "blockquote[type='cite']",
      "[data-marker='__QUOTED_TEXT__']",
      "#divRplyFwdMsg",
    ].join(","),
  );
  const outlookHeader = findOutlookReplyHeader(document);
  let outlookBoundary: HTMLElement | null = null;
  if (
    outlookHeader &&
    (!candidate ||
      (candidate.compareDocumentPosition(outlookHeader) &
        Node.DOCUMENT_POSITION_PRECEDING) !==
        0)
  ) {
    if (outlookHeader.closest("details.dakia-quoted-history")) return;
    const boundary = outlookQuoteBoundary(outlookHeader);
    if (!hasMeaningfulFollowingContent(boundary)) return;
    outlookBoundary = boundary;
  } else if (
    !candidate ||
    candidate.closest("details.dakia-quoted-history") ||
    !hasMeaningfulQuotedContent(candidate)
  ) {
    return;
  }

  const details = document.createElement("details");
  details.className = "dakia-quoted-history";
  details.style.setProperty("all", "revert", "important");
  details.style.setProperty("display", "block", "important");
  details.style.setProperty("visibility", "visible", "important");
  details.style.setProperty("opacity", "1", "important");
  details.style.setProperty("position", "static", "important");
  details.style.setProperty("inset", "auto", "important");
  details.style.setProperty("transform", "none", "important");
  details.style.setProperty("clip", "auto", "important");
  details.style.setProperty("clip-path", "none", "important");
  details.style.setProperty("content-visibility", "visible", "important");
  details.style.setProperty("pointer-events", "auto", "important");
  details.style.setProperty("overflow", "visible", "important");
  details.style.setProperty("margin", "1em 0 0", "important");
  const summary = document.createElement("summary");
  summary.style.setProperty("all", "revert", "important");
  summary.style.setProperty("display", "list-item", "important");
  summary.style.setProperty("visibility", "visible", "important");
  summary.style.setProperty("opacity", "1", "important");
  summary.style.setProperty("position", "static", "important");
  summary.style.setProperty("inset", "auto", "important");
  summary.style.setProperty("transform", "none", "important");
  summary.style.setProperty("clip", "auto", "important");
  summary.style.setProperty("clip-path", "none", "important");
  summary.style.setProperty("content-visibility", "visible", "important");
  summary.style.setProperty("pointer-events", "auto", "important");
  summary.style.setProperty("overflow", "visible", "important");
  summary.style.setProperty("width", "fit-content", "important");
  summary.style.setProperty("height", "auto", "important");
  const show = document.createElement("span");
  show.className = "dakia-history-show";
  show.textContent = labels.show;
  const hide = document.createElement("span");
  hide.className = "dakia-history-hide";
  hide.textContent = labels.hide;
  summary.append(show, hide);
  const content = document.createElement("div");
  content.className = "dakia-history-content";

  if (outlookBoundary) {
    outlookBoundary.before(details);
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
        "blockquote, .gmail_quote, .yahoo_quoted, [data-marker='__QUOTED_TEXT__']",
      )
    ) {
      nodes.push(node);
      node = node.nextElementSibling;
    }
    candidate.before(details);
    nodes.forEach((item) => content.append(item));
  } else if (candidate) {
    const citation = precedingCitation(candidate);
    candidate.before(details);
    if (citation) content.append(citation);
    content.append(candidate);
  }
  details.append(summary, content);
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
        "blockquote, .gmail_quote, .yahoo_quoted, [data-marker='__QUOTED_TEXT__']",
      ),
    );
  }
  if (candidate.matches(".gmail_quote, .yahoo_quoted")) {
    const quote = candidate.querySelector(
      "blockquote, [type='cite'], .gmail_quote, .yahoo_quoted",
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

function supportsDarkAppearance(document: Document) {
  if (
    document.querySelector(
      "style, table, img, picture, svg, [background], [bgcolor], [color]",
    )
  ) {
    return false;
  }
  return ![...document.querySelectorAll("*")].some((element) =>
    /\b(?:background(?:-color|-image)?|color)\s*:/i.test(
      element.getAttribute("style") ?? "",
    ),
  );
}

export function HtmlMessage({
  html,
  title,
  showHistoryLabel,
  hideHistoryLabel,
}: Props) {
  const host = useRef<HTMLDivElement>(null);
  const [darkMode, setDarkMode] = useState(
    () => document.documentElement.dataset.mantineColorScheme === "dark",
  );
  const email = useMemo(
    () =>
      buildEmailDocument(html, darkMode, {
        show: showHistoryLabel,
        hide: hideHistoryLabel,
      }),
    [darkMode, hideHistoryLabel, html, showHistoryLabel],
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

  useEffect(() => {
    const element = host.current;
    if (!element) return;
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
    const parsedDocument = new DOMParser().parseFromString(
      email.source,
      "text/html",
    );
    const root = parsedDocument.documentElement;
    root.querySelector('meta[http-equiv="Content-Security-Policy"]')?.remove();
    shadow.replaceChildren(root);
    outerShadow.replaceChildren(surface);

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
      const target = shadow.getElementById(id);
      target?.scrollIntoView({ block: "nearest" });
    };
    shadow.addEventListener("click", activateLink, true);
    shadow.addEventListener("auxclick", activateLink, true);
    shadow.addEventListener("keydown", activateLink, true);
    return () => {
      shadow.removeEventListener("click", activateLink, true);
      shadow.removeEventListener("auxclick", activateLink, true);
      shadow.removeEventListener("keydown", activateLink, true);
    };
  }, [email.source]);

  return (
    <div
      ref={host}
      className={`html-message${email.dark ? " html-message--dark" : ""}`}
      role="document"
      aria-label={title}
      style={{ contain: "layout paint", isolation: "isolate" }}
    />
  );
}
