import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanEmailSnippet } from "./snippet";

describe("cleanEmailSnippet", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("turns HTML email previews into clean readable text", () => {
    expect(
      cleanEmailSnippet(`
        <html>
          <head><style>.hidden { display: none }</style></head>
          <body>
            <h1>Order confirmed</h1>
            <p>Your parcel&nbsp;will arrive <strong>tomorrow</strong>.</p>
            <script>trackOpening()</script>
          </body>
        </html>
      `),
    ).toBe("Order confirmed Your parcel will arrive tomorrow.");
  });

  it("decodes entities and normalizes raw whitespace and control characters", () => {
    expect(cleanEmailSnippet("Tom &amp; Ann\r\n\t sent\u0000 an update")).toBe(
      "Tom & Ann sent an update",
    );
  });

  it("keeps words separated across compact HTML blocks", () => {
    expect(
      cleanEmailSnippet("<h1>Weekly update</h1><p>Three tasks done.</p>"),
    ).toBe("Weekly update Three tasks done.");
  });

  it("uses inert parsing for hostile markup instead of DOMParser", () => {
    vi.stubGlobal(
      "DOMParser",
      class {
        constructor() {
          throw new Error("live parser must not run for snippets");
        }
      },
    );

    expect(
      cleanEmailSnippet(
        '<img src="https://images.example.test/open.png"><p>Visible preview</p>',
      ),
    ).toBe("Visible preview");
  });
});
