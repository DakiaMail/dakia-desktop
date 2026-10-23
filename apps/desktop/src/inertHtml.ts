/**
 * Parses untrusted HTML through an inert template before copying it into a
 * detached document. The copy is safe to inspect and serialize without
 * starting resource requests from the app document.
 */
export function parseInertHtml(html: string) {
  const template = window.document.createElement("template");
  template.innerHTML = html;
  const document = window.document.implementation.createHTMLDocument("");
  document.head.replaceChildren();
  document.body.replaceChildren(template.content.cloneNode(true));

  // Fragment parsing omits document wrapper elements. Find real wrapper tags
  // without treating tag-shaped text in comments, quoted attributes, or raw
  // script/style text as a document attribute source. Attribute decoding is
  // still performed by a second inert template, never by string rewriting.
  applyDocumentAttributes(document.documentElement, html, "html");

  // Parsing a complete document as an inert fragment leaves head children in
  // the fragment. Put declarative head content back into the detached head so
  // CSS is preserved without becoming visible translation/snippet text.
  for (const node of [...document.body.children].filter((element) =>
    ["base", "link", "meta", "style", "title"].includes(element.localName),
  )) {
    document.head.append(node);
  }

  applyDocumentAttributes(document.body, html, "body");
  return document;
}

function applyDocumentAttributes(
  target: HTMLElement,
  html: string,
  name: "html" | "body",
) {
  for (const source of startTagAttributes(html, name)) {
    const attributesTemplate = window.document.createElement("template");
    attributesTemplate.innerHTML = `<div ${source}></div>`;
    for (const attribute of [
      ...(attributesTemplate.content.firstElementChild?.attributes ?? []),
    ]) {
      // Browsers retain the first value of duplicate attributes while later
      // document wrappers can contribute attributes that were missing before.
      if (!target.hasAttribute(attribute.name)) {
        target.setAttribute(attribute.name, attribute.value);
      }
    }
  }
}

function startTagAttributes(html: string, expectedName: "html" | "body") {
  const rawTextTags = new Set([
    "script",
    "style",
    "textarea",
    "title",
    "xmp",
    "iframe",
    "noembed",
    "noframes",
    "noscript",
  ]);
  const attributes: string[] = [];
  let index = 0;
  let rawTextTag: string | undefined;
  while (index < html.length) {
    if (rawTextTag) {
      const closing = rawTextClosing(html, rawTextTag, index);
      if (closing < 0) return attributes;
      rawTextTag = undefined;
      index = closing;
      continue;
    }
    if (html.startsWith("<!--", index)) {
      index = commentEnd(html, index + 4);
      continue;
    }
    if (/^<!\[CDATA\[/i.test(html.slice(index))) {
      const closing = html.indexOf("]]>", index + 9);
      index = closing < 0 ? html.length : closing + 3;
      continue;
    }
    if (html[index] !== "<") {
      index += 1;
      continue;
    }
    const nameStart = index + 1;
    if (
      html[nameStart] === "/" ||
      html[nameStart] === "!" ||
      html[nameStart] === "?"
    ) {
      index = tagEnd(html, nameStart + 1) + 1;
      continue;
    }
    const match = /^[A-Za-z][A-Za-z0-9:-]*/.exec(html.slice(nameStart));
    if (!match) {
      index += 1;
      continue;
    }
    const tagName = match[0].toLowerCase();
    const attributesStart = nameStart + match[0].length;
    const end = tagEnd(html, attributesStart);
    if (tagName === expectedName) {
      attributes.push(
        html.slice(attributesStart, end).replace(/\/$/, "").trim(),
      );
    }
    // In the app's scripting-enabled parser, plaintext consumes the remainder
    // as text. A later tag-shaped string cannot be a document wrapper.
    if (tagName === "plaintext") return attributes;
    if (rawTextTags.has(tagName)) rawTextTag = tagName;
    index = end + 1;
  }
  return attributes;
}

function commentEnd(html: string, start: number) {
  const normal = html.indexOf("-->", start);
  const recovered = html.indexOf("--!>", start);
  if (normal < 0 && recovered < 0) return html.length;
  if (normal < 0) return recovered + 4;
  if (recovered < 0) return normal + 3;
  return normal < recovered ? normal + 3 : recovered + 4;
}

function rawTextClosing(html: string, name: string, start: number) {
  const needle = `</${name}`;
  const source = html.toLowerCase();
  let index = source.indexOf(needle, start);
  while (index >= 0) {
    const boundary = source[index + needle.length];
    if (boundary === undefined || /[\t\n\f\r />]/.test(boundary)) {
      return index;
    }
    index = source.indexOf(needle, index + needle.length);
  }
  return -1;
}

function tagEnd(html: string, start: number) {
  let quote: '"' | "'" | undefined;
  for (let index = start; index < html.length; index += 1) {
    const character = html[index];
    if (quote) {
      if (character === quote) quote = undefined;
    } else if (character === '"' || character === "'") {
      quote = character;
    } else if (character === ">") {
      return index;
    }
  }
  return html.length;
}

export function cloneInertHtmlDocument(document: Document) {
  const clone = window.document.implementation.createHTMLDocument("");
  for (const attribute of [...document.documentElement.attributes]) {
    clone.documentElement.setAttribute(attribute.name, attribute.value);
  }
  for (const attribute of [...document.head.attributes]) {
    clone.head.setAttribute(attribute.name, attribute.value);
  }
  for (const attribute of [...document.body.attributes]) {
    clone.body.setAttribute(attribute.name, attribute.value);
  }
  clone.head.replaceChildren(
    ...[...document.head.childNodes].map((node) => node.cloneNode(true)),
  );
  clone.body.replaceChildren(
    ...[...document.body.childNodes].map((node) => node.cloneNode(true)),
  );
  return clone;
}
