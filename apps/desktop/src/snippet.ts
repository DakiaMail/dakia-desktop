const HTML_MARKUP = /<\/?[a-z][^>]*>|<!--/i;
const HTML_ENTITY = /&(?:[a-z][a-z0-9]+|#\d+|#x[\da-f]+);/i;

export function cleanEmailSnippet(value: string) {
  let text = value.replace(/\r\n?/g, "\n");

  if (HTML_MARKUP.test(text) || HTML_ENTITY.test(text)) {
    const document = new DOMParser().parseFromString(text, "text/html");
    document
      .querySelectorAll("head, style, script, noscript, svg, template")
      .forEach((element) => element.remove());
    document
      .querySelectorAll(
        "br, blockquote, div, h1, h2, h3, h4, h5, h6, li, p, pre, table, tr",
      )
      .forEach((element) => {
        element.before(document.createTextNode(" "));
        element.after(document.createTextNode(" "));
      });
    text = document.body.textContent ?? "";
  }

  return text
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}
