import { describe, expect, it } from "vitest";
import {
  decodeNativeMenuAddress,
  parseEmailAddressMenuAction,
} from "./emailAddressMenu";

function encode(value: string) {
  const bytes = new TextEncoder().encode(value);
  let binary = "";
  bytes.forEach((byte) => (binary += String.fromCharCode(byte)));
  return btoa(binary)
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "");
}

describe("email address native-menu actions", () => {
  it("round-trips URL-safe UTF-8 email addresses", () => {
    const address = "müller+news@example.com";
    expect(decodeNativeMenuAddress(encode(address))).toBe(address);
    expect(
      parseEmailAddressMenuAction(`copy-email-address:${encode(address)}`),
    ).toBeUndefined();
    expect(
      parseEmailAddressMenuAction(
        `compose-email-address:account-2:${encode(address)}`,
      ),
    ).toEqual({ kind: "compose", accountId: "account-2", address });
  });

  it("rejects missing and malformed payloads", () => {
    expect(parseEmailAddressMenuAction("copy-email-address:")).toBeUndefined();
    expect(
      parseEmailAddressMenuAction("compose-email-address:no-separator"),
    ).toBeUndefined();
    expect(
      parseEmailAddressMenuAction("compose-email-address::Zm9v"),
    ).toBeUndefined();
    expect(parseEmailAddressMenuAction("unrelated")).toBeUndefined();
    expect(
      parseEmailAddressMenuAction("copy-email-address:_w"),
    ).toBeUndefined();
    expect(
      parseEmailAddressMenuAction(
        `copy-email-address:${encode("not@an@address")}`,
      ),
    ).toBeUndefined();
    expect(
      parseEmailAddressMenuAction(
        `copy-email-address:${encode("person@example.com\nforged")}`,
      ),
    ).toBeUndefined();
  });
});
