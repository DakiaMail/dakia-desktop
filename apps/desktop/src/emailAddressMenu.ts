export type EmailAddressMenuAction = {
  kind: "compose";
  accountId: string;
  address: string;
};

export function decodeNativeMenuAddress(value: string) {
  if (!value || !/^[A-Za-z0-9_-]+$/.test(value) || value.length % 4 === 1)
    return undefined;
  try {
    const standardBase64 = value
      .replaceAll("-", "+")
      .replaceAll("_", "/")
      .padEnd(Math.ceil(value.length / 4) * 4, "=");
    const bytes = Uint8Array.from(atob(standardBase64), (character) =>
      character.charCodeAt(0),
    );
    const address = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    return isSafeMenuEmailAddress(address) ? address : undefined;
  } catch {
    return undefined;
  }
}

function isSafeMenuEmailAddress(value: string) {
  return (
    value.length > 0 &&
    new TextEncoder().encode(value).length <= 320 &&
    value.split("@").length === 2 &&
    ![...value].some((character) => /[\p{White_Space}\p{Cc}]/u.test(character))
  );
}

export function parseEmailAddressMenuAction(
  action: string,
): EmailAddressMenuAction | undefined {
  if (!action.startsWith("compose-email-address:")) return undefined;
  const payload = action.slice("compose-email-address:".length);
  const separator = payload.indexOf(":");
  if (separator <= 0) return undefined;
  const accountId = payload.slice(0, separator);
  const address = decodeNativeMenuAddress(payload.slice(separator + 1));
  return address ? { kind: "compose", accountId, address } : undefined;
}
