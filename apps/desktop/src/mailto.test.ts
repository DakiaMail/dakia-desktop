import { describe, expect, it } from "vitest";
import { composeSeedFromMailto } from "./mailto";

describe("composeSeedFromMailto", () => {
  it("preserves standard mailto compose fields", () => {
    expect(
      composeSeedFromMailto(
        "mailto:juhan%2Btamm@example.com?cc=abi%40example.com&subject=Tere%20Juhan&body=Kohtume%20homme",
      ),
    ).toEqual({
      to: "juhan+tamm@example.com",
      cc: "abi@example.com",
      subject: "Tere Juhan",
      body: "Kohtume homme",
    });
  });

  it("opens recipient-free links and treats field names case-insensitively", () => {
    expect(
      composeSeedFromMailto(
        "mailto:?TO=juhan+tag@example.com&CC=abi+tag@example.com&SUBJECT=C++",
      ),
    ).toEqual({
      to: "juhan+tag@example.com",
      cc: "abi+tag@example.com",
      subject: "C++",
    });
    expect(composeSeedFromMailto("mailto:?subject=Feedback")).toEqual({
      subject: "Feedback",
    });
  });

  it("rejects non-mailto and malformed links", () => {
    expect(composeSeedFromMailto("https://example.com")).toBeUndefined();
    expect(composeSeedFromMailto("mailto:%E0%A4%A")).toBeUndefined();
    expect(
      composeSeedFromMailto("mailto:user@example.com?subject=%E0%A4%A"),
    ).toBeUndefined();
  });
});
