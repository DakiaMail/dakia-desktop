import type { ComposeSeed } from "./composeWindow";

export function composeSeedFromMailto(href: string): ComposeSeed | undefined {
  if (!/^mailto:/i.test(href)) return undefined;
  const queryIndex = href.indexOf("?");
  const encodedRecipients = href.slice(
    href.indexOf(":") + 1,
    queryIndex === -1 ? undefined : queryIndex,
  );
  let recipients: string;
  try {
    recipients = decodeURIComponent(encodedRecipients);
  } catch {
    return undefined;
  }

  const fields = new Map<string, string[]>();
  try {
    for (const pair of (queryIndex === -1 ? "" : href.slice(queryIndex + 1))
      .split("&")
      .filter(Boolean)) {
      const separator = pair.indexOf("=");
      const encodedName = separator === -1 ? pair : pair.slice(0, separator);
      const encodedValue = separator === -1 ? "" : pair.slice(separator + 1);
      const name = decodeURIComponent(encodedName).toLowerCase();
      const values = fields.get(name) ?? [];
      values.push(decodeURIComponent(encodedValue));
      fields.set(name, values);
    }
  } catch {
    return undefined;
  }

  const seed: ComposeSeed = {};
  const to = [recipients, ...(fields.get("to") ?? [])]
    .filter(Boolean)
    .join(", ");
  const cc = (fields.get("cc") ?? []).filter(Boolean).join(", ");
  const subject = fields.get("subject")?.[0];
  const body = fields.get("body")?.[0];
  if (to) seed.to = to;
  if (cc) seed.cc = cc;
  if (subject !== undefined) seed.subject = subject;
  if (body !== undefined) seed.body = body;
  return seed;
}
