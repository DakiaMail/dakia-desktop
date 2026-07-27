const ALLOWED_TAGS = new Set([
  "a",
  "b",
  "blockquote",
  "br",
  "code",
  "div",
  "em",
  "h1",
  "h2",
  "h3",
  "i",
  "li",
  "ol",
  "p",
  "pre",
  "s",
  "span",
  "strike",
  "strong",
  "u",
  "ul",
]);

const DISCARD_TAGS = new Set([
  "applet",
  "embed",
  "form",
  "iframe",
  "img",
  "link",
  "meta",
  "object",
  "script",
  "style",
  "svg",
  "video",
]);

const BLOCK_TAGS = new Set(["blockquote", "div", "h1", "h2", "h3", "p", "pre"]);

/**
 * Keeps the outgoing HTML intentionally small and email-safe. In particular,
 * images and their data URLs are excluded until inline attachments are built
 * as proper MIME related parts.
 */
export function sanitizeRichText(html: string) {
  const document = new DOMParser().parseFromString(html, "text/html");
  sanitizeChildren(document.body);
  return document.body.innerHTML;
}

export function richTextFromPlainText(text: string) {
  if (!text) return "";
  return text
    .replace(/\r\n?/g, "\n")
    .split(/\n{2,}/)
    .map(
      (paragraph) => `<p>${escapeHtml(paragraph).replace(/\n/g, "<br>")}</p>`,
    )
    .join("");
}

export function plainTextFromRichText(html: string) {
  const document = new DOMParser().parseFromString(
    sanitizeRichText(html),
    "text/html",
  );
  let output = "";

  const addBreak = () => {
    if (output && !output.endsWith("\n")) output += "\n";
  };
  const visit = (node: Node) => {
    if (node.nodeType === Node.TEXT_NODE) {
      output += node.textContent ?? "";
      return;
    }
    if (node.nodeType !== Node.ELEMENT_NODE) return;

    const element = node as HTMLElement;
    const tag = element.tagName.toLowerCase();
    if (tag === "br") {
      output += "\n";
      return;
    }
    if (tag === "li") {
      addBreak();
      output += "• ";
      element.childNodes.forEach(visit);
      addBreak();
      return;
    }
    if (BLOCK_TAGS.has(tag)) addBreak();
    element.childNodes.forEach(visit);
    if (BLOCK_TAGS.has(tag)) addBreak();
  };

  document.body.childNodes.forEach(visit);
  return output.replace(/\n{3,}/g, "\n\n").trim();
}

export function isRichTextEmpty(html: string) {
  return !plainTextFromRichText(html).trim();
}

function sanitizeChildren(parent: Element) {
  for (const node of [...parent.childNodes]) {
    if (node.nodeType !== Node.ELEMENT_NODE) continue;
    const element = node as HTMLElement;
    const tag = element.tagName.toLowerCase();

    if (DISCARD_TAGS.has(tag)) {
      element.remove();
      continue;
    }

    sanitizeChildren(element);
    if (!ALLOWED_TAGS.has(tag)) {
      element.replaceWith(...[...element.childNodes]);
      continue;
    }

    if (tag === "a") {
      const href = safeHref(node as HTMLAnchorElement);
      for (const attribute of [...element.attributes]) {
        element.removeAttribute(attribute.name);
      }
      if (href) {
        element.setAttribute("href", href);
        element.setAttribute("rel", "noreferrer noopener");
      } else {
        element.replaceWith(...[...element.childNodes]);
      }
    } else {
      const style = safeStyle(element);
      for (const attribute of [...element.attributes]) {
        element.removeAttribute(attribute.name);
      }
      if (style) element.setAttribute("style", style);
    }
  }
}

function safeStyle(element: HTMLElement) {
  const declarations: string[] = [];
  const weight = element.style.fontWeight;
  if (/^(normal|bold|[1-9]00)$/.test(weight)) {
    declarations.push(`font-weight: ${weight}`);
  }
  if (/^(normal|italic)$/.test(element.style.fontStyle)) {
    declarations.push(`font-style: ${element.style.fontStyle}`);
  }
  const decoration = element.style.textDecorationLine;
  if (
    /^(none|underline|line-through|underline line-through)$/.test(decoration)
  ) {
    declarations.push(`text-decoration: ${decoration}`);
  }
  return declarations.join("; ");
}

function safeHref(anchor: HTMLAnchorElement) {
  const href = anchor.getAttribute("href")?.trim() ?? "";
  if (!href) return null;
  try {
    const url = new URL(
      href.startsWith("mailto:") || href.includes("://")
        ? href
        : `https://${href}`,
    );
    return ["http:", "https:", "mailto:"].includes(url.protocol)
      ? url.toString()
      : null;
  } catch {
    return null;
  }
}

function escapeHtml(value: string) {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}
