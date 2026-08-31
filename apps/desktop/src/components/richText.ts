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

const BLOCK_TAGS = new Set([
  "blockquote",
  "div",
  "h1",
  "h2",
  "h3",
  "p",
  "pre",
  "table",
  "tr",
]);

const QUOTED_EMAIL_TAGS = new Set([
  ...ALLOWED_TAGS,
  "caption",
  "col",
  "colgroup",
  "font",
  "h4",
  "h5",
  "h6",
  "img",
  "table",
  "tbody",
  "td",
  "tfoot",
  "th",
  "thead",
  "tr",
]);

const QUOTED_EMAIL_ATTRIBUTES = new Set([
  "align",
  "alt",
  "bgcolor",
  "border",
  "cellpadding",
  "cellspacing",
  "colspan",
  "height",
  "role",
  "rowspan",
  "title",
  "valign",
  "width",
]);

const SAFE_QUOTED_STYLE_PROPERTIES = new Set([
  "background",
  "background-color",
  "border",
  "border-bottom",
  "border-color",
  "border-left",
  "border-radius",
  "border-right",
  "border-style",
  "border-top",
  "border-width",
  "box-sizing",
  "color",
  "display",
  "font-family",
  "font-size",
  "font-style",
  "font-weight",
  "height",
  "letter-spacing",
  "line-height",
  "margin",
  "margin-bottom",
  "margin-left",
  "margin-right",
  "margin-top",
  "max-width",
  "min-width",
  "padding",
  "padding-bottom",
  "padding-left",
  "padding-right",
  "padding-top",
  "text-align",
  "text-decoration",
  "text-decoration-line",
  "text-indent",
  "text-transform",
  "vertical-align",
  "white-space",
  "width",
  "word-break",
  "word-wrap",
]);

/**
 * Keeps authored outgoing HTML intentionally small and email-safe. Generated
 * quoted-history wrappers may retain a larger, separately sanitized subset of
 * email layout markup so replies and forwards do not flatten the original.
 */
export function sanitizeRichText(
  html: string,
  options: { preserveQuotedEmail?: boolean } = {},
) {
  const document = new DOMParser().parseFromString(html, "text/html");
  sanitizeChildren(document.body, options.preserveQuotedEmail === true);
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
    sanitizeRichText(html, { preserveQuotedEmail: true }),
    "text/html",
  );
  let output = "";

  const addBreak = () => {
    if (output && !output.endsWith("\n")) output += "\n";
  };
  const appendText = (text: string, quoteDepth: number) => {
    const lines = text.split("\n");
    lines.forEach((line, index) => {
      if (line && quoteDepth && (!output || output.endsWith("\n"))) {
        output += "> ".repeat(quoteDepth);
      }
      output += line;
      if (index < lines.length - 1) output += "\n";
    });
  };
  const visit = (node: Node, quoteDepth = 0) => {
    if (node.nodeType === Node.TEXT_NODE) {
      appendText(node.textContent ?? "", quoteDepth);
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
      appendText("• ", quoteDepth);
      element.childNodes.forEach((child) => {
        visit(child, quoteDepth);
      });
      addBreak();
      return;
    }
    if (tag === "blockquote") {
      addBreak();
      element.childNodes.forEach((child) => {
        visit(child, quoteDepth + 1);
      });
      addBreak();
      return;
    }
    if (tag === "td" || tag === "th") {
      addBreak();
      element.childNodes.forEach((child) => {
        visit(child, quoteDepth);
      });
      addBreak();
      return;
    }
    if (BLOCK_TAGS.has(tag)) addBreak();
    element.childNodes.forEach((child) => {
      visit(child, quoteDepth);
    });
    if (BLOCK_TAGS.has(tag)) addBreak();
  };

  document.body.childNodes.forEach((child) => {
    visit(child);
  });
  return output.replace(/\n{3,}/g, "\n\n").trim();
}

export function isRichTextEmpty(html: string) {
  return !plainTextFromRichText(html).trim();
}

function sanitizeChildren(parent: Element, preserveQuotedEmail: boolean) {
  for (const node of [...parent.childNodes]) {
    if (node.nodeType !== Node.ELEMENT_NODE) continue;
    const element = node as HTMLElement;
    const tag = element.tagName.toLowerCase();

    const quotedRoot =
      preserveQuotedEmail &&
      tag === "div" &&
      element.getAttribute("data-dakia-quoted-email") === "true";
    const insideQuotedEmail =
      preserveQuotedEmail &&
      (quotedRoot ||
        parent.closest("[data-dakia-quoted-email='true']") !== null);

    if (DISCARD_TAGS.has(tag) && !(insideQuotedEmail && tag === "img")) {
      element.remove();
      continue;
    }

    sanitizeChildren(element, preserveQuotedEmail);
    if (!(insideQuotedEmail ? QUOTED_EMAIL_TAGS : ALLOWED_TAGS).has(tag)) {
      element.replaceWith(...[...element.childNodes]);
      continue;
    }

    if (tag === "a") {
      const href = safeHref(node as HTMLAnchorElement);
      if (insideQuotedEmail) {
        sanitizeQuotedEmailElement(element, tag, quotedRoot);
      } else {
        for (const attribute of [...element.attributes]) {
          element.removeAttribute(attribute.name);
        }
      }
      if (href) {
        element.setAttribute("href", href);
        element.setAttribute("rel", "noreferrer noopener");
      } else {
        element.replaceWith(...[...element.childNodes]);
      }
    } else if (insideQuotedEmail) {
      sanitizeQuotedEmailElement(element, tag, quotedRoot);
    } else {
      const citePrefix =
        tag === "div" && element.getAttribute("class") === "moz-cite-prefix";
      const citeBlock =
        tag === "blockquote" && element.getAttribute("type") === "cite";
      const style = safeStyle(element);
      for (const attribute of [...element.attributes]) {
        element.removeAttribute(attribute.name);
      }
      if (style) element.setAttribute("style", style);
      if (citePrefix) element.setAttribute("class", "moz-cite-prefix");
      if (citeBlock) element.setAttribute("type", "cite");
    }
  }
}

function sanitizeQuotedEmailElement(
  element: HTMLElement,
  tag: string,
  quotedRoot: boolean,
) {
  const attributes = [...element.attributes];
  for (const attribute of attributes) {
    const name = attribute.name.toLowerCase();
    if (name === "style") continue;
    if (quotedRoot && name === "data-dakia-quoted-email") continue;
    if (QUOTED_EMAIL_ATTRIBUTES.has(name)) continue;
    if (tag === "img" && name === "src" && safeImageSource(attribute.value)) {
      continue;
    }
    element.removeAttribute(attribute.name);
  }
  const style = safeQuotedEmailStyle(element.getAttribute("style") ?? "");
  if (style) element.setAttribute("style", style);
  else element.removeAttribute("style");
}

function safeQuotedEmailStyle(style: string) {
  const source = document.createElement("span");
  source.setAttribute("style", style);
  const declarations: string[] = [];
  for (const property of [...source.style]) {
    const normalized = property.toLowerCase();
    const value = source.style.getPropertyValue(property).trim();
    if (
      SAFE_QUOTED_STYLE_PROPERTIES.has(normalized) &&
      value &&
      !/(?:expression\s*\(|javascript\s*:|behavior\s*:|-moz-binding|url\s*\()/i.test(
        value,
      )
    ) {
      declarations.push(`${normalized}: ${value}`);
    }
  }
  return declarations.join("; ");
}

function safeImageSource(value: string) {
  const normalized = value.trim().replace(/[\u0000-\u001f\u007f\s]+/g, "");
  return /^(?:https?:|data:image\/(?:png|gif|jpe?g|webp|svg\+xml);)/i.test(
    normalized,
  );
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
