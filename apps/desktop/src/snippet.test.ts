import { describe, expect, it } from "vitest";
import { cleanEmailSnippet } from "./snippet";

describe("cleanEmailSnippet", () => {
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
});
