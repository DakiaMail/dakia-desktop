import { describe, expect, it } from "vitest";
import { mailSafetyOrigin } from "./api";

describe("mail safety demo fixture origin", () => {
  it("accepts only an explicit unprivileged IPv4 loopback HTTP origin", () => {
    expect(mailSafetyOrigin("http://127.0.0.1:18888")).toBe(
      "http://127.0.0.1:18888",
    );
    expect(mailSafetyOrigin("http://127.0.0.1:18888/")).toBe(
      "http://127.0.0.1:18888",
    );
  });

  it.each([
    undefined,
    "https://127.0.0.1:18888",
    "http://localhost:18888",
    "http://127.0.0.1:80",
    "http://127.0.0.1:01888",
    "http://127.0.0.1:70000",
    "http://127.0.0.1:18888/path",
    "http://127.0.0.1:18888?query=value",
    "http://127.0.0.1:18888#fragment",
    "http://user@127.0.0.1:18888",
  ])("rejects %s", (value) => {
    expect(mailSafetyOrigin(value)).toBeUndefined();
  });

  it("keeps the native unsafe-attachment probe metadata explicit", async () => {
    const { api } = await import("./api");

    await expect(api.attachments("demo-mail-safety")).resolves.toEqual([
      expect.objectContaining({
        filename: "quarterly-report.pdf.command",
        mime_type: "application/x-sh",
        is_potentially_unsafe: true,
      }),
    ]);
  });
});
