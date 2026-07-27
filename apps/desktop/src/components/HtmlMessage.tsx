import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api } from "../api";

type Props = {
  html: string;
  title: string;
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
    if (!href || href.startsWith("#")) continue;
    anchor.dataset.dakiaExternalHref = href;
    anchor.removeAttribute("xlink:href");
    anchor.setAttribute("href", "#dakia-external-link");
  }

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
    a[data-dakia-external-href] { cursor: pointer; text-decoration: underline; }
  `;
  document.head.append(compatibilityStyles);
  return {
    source: `<!doctype html>${document.documentElement.outerHTML}`,
    dark,
  };
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

export function HtmlMessage({ html, title }: Props) {
  const frame = useRef<HTMLIFrameElement>(null);
  const resizeObserver = useRef<ResizeObserver | null>(null);
  const documentCleanup = useRef<(() => void) | null>(null);
  const [height, setHeight] = useState(240);
  const [darkMode, setDarkMode] = useState(
    () => document.documentElement.dataset.mantineColorScheme === "dark",
  );
  const email = useMemo(
    () => buildEmailDocument(html, darkMode),
    [html, darkMode],
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

  const resize = useCallback(() => {
    const body = frame.current?.contentDocument?.body;
    const root = frame.current?.contentDocument?.documentElement;
    if (!body || !root) return;
    setHeight(
      Math.max(
        1,
        body.scrollHeight,
        body.offsetHeight,
        root.scrollHeight,
        root.offsetHeight,
      ),
    );
  }, []);

  const connectDocument = useCallback(() => {
    const document = frame.current?.contentDocument;
    if (!document) return;
    documentCleanup.current?.();
    resizeObserver.current?.disconnect();
    const FrameResizeObserver =
      document.defaultView?.ResizeObserver ?? ResizeObserver;
    resizeObserver.current = new FrameResizeObserver(resize);
    if (document.body) resizeObserver.current.observe(document.body);
    document.querySelectorAll("img").forEach((image) => {
      image.addEventListener("load", resize, { once: true });
      image.addEventListener("error", resize, { once: true });
    });
    const links = [
      ...document.querySelectorAll<HTMLAnchorElement>(
        "a[data-dakia-external-href]",
      ),
    ];
    const removeLinkListeners = links.map((anchor) => {
      const openExternalLink = (event: MouseEvent | KeyboardEvent) => {
        if (
          event.type === "keydown" &&
          (event as KeyboardEvent).key !== "Enter"
        )
          return;
        if (event.type === "auxclick" && (event as MouseEvent).button !== 1)
          return;
        event.preventDefault();
        event.stopPropagation();
        const href = anchor.dataset.dakiaExternalHref;
        if (!href) return;
        void api
          .openExternal(href)
          .catch((error) =>
            console.error(
              "Could not open email link in the default browser",
              error,
            ),
          );
      };
      anchor.addEventListener("click", openExternalLink, true);
      anchor.addEventListener("auxclick", openExternalLink, true);
      anchor.addEventListener("keydown", openExternalLink, true);
      return () => {
        anchor.removeEventListener("click", openExternalLink, true);
        anchor.removeEventListener("auxclick", openExternalLink, true);
        anchor.removeEventListener("keydown", openExternalLink, true);
      };
    });
    documentCleanup.current = () => {
      removeLinkListeners.forEach((remove) => remove());
    };
    resize();
  }, [resize]);

  useEffect(() => {
    const element = frame.current;
    if (!element) return;
    element.addEventListener("load", connectDocument);
    connectDocument();
    return () => element.removeEventListener("load", connectDocument);
  }, [connectDocument, email.source]);

  useEffect(
    () => () => {
      documentCleanup.current?.();
      resizeObserver.current?.disconnect();
    },
    [],
  );

  return (
    <iframe
      ref={frame}
      className={`html-message${email.dark ? " html-message--dark" : ""}`}
      title={title}
      srcDoc={email.source}
      sandbox="allow-same-origin"
      style={{ height }}
    />
  );
}
