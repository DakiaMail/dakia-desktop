import type { MailSummary } from "./types";

export type MailAddress = {
  name?: string;
  address: string;
};

export type MessageRecipients = {
  from: MailAddress[];
  to: MailAddress[];
  cc: MailAddress[];
  bcc: MailAddress[];
  replyTo: MailAddress[];
};

export type ReplyRecipients = {
  to: string;
  cc: string;
};

export function parseAddressList(value?: string): MailAddress[] {
  if (!value?.trim()) return [];

  return splitAddressValues(value)
    .map(parseAddress)
    .filter((address): address is MailAddress => Boolean(address));
}

export function splitAddressValues(value: string) {
  return splitHeaderList(value);
}

export function messageRecipients(message: MailSummary): MessageRecipients {
  return {
    from: [
      {
        name: message.from_name || undefined,
        address: message.from_address,
      },
    ].filter((item) => Boolean(item.address.trim())),
    to: parseAddressList(message.to_addresses),
    cc: parseAddressList(message.cc_addresses),
    bcc: parseAddressList(message.bcc_addresses),
    replyTo: parseAddressList(message.reply_to_addresses),
  };
}

export function replyRecipients(
  message: MailSummary,
  accountEmail?: string,
  replyAll = false,
): ReplyRecipients | undefined {
  const recipients = messageRecipients(message);
  const replyTargets = recipients.replyTo.length
    ? recipients.replyTo
    : recipients.from.slice(0, 1);
  if (!replyTargets.length) return undefined;

  if (!replyAll) {
    return { to: replyTargets.map(formatAddress).join(", "), cc: "" };
  }

  const excluded = new Set(
    [accountEmail, ...replyTargets.map((item) => item.address)]
      .filter((value): value is string => Boolean(value))
      .map(normalizeAddress),
  );
  const seen = new Set(excluded);
  const additionalTo: MailAddress[] = [];
  for (const recipient of recipients.to) {
    const normalized = normalizeAddress(recipient.address);
    if (!normalized || seen.has(normalized)) continue;
    seen.add(normalized);
    additionalTo.push(recipient);
  }
  const copies: MailAddress[] = [];
  for (const recipient of recipients.cc) {
    const normalized = normalizeAddress(recipient.address);
    if (!normalized || seen.has(normalized)) continue;
    seen.add(normalized);
    copies.push(recipient);
  }

  return {
    to: [...replyTargets, ...additionalTo].map(formatAddress).join(", "),
    cc: copies.map(formatAddress).join(", "),
  };
}

export function formatAddress({ name, address }: MailAddress) {
  const cleanName = name?.trim();
  if (!cleanName) return address.trim();
  const quotedName = /[",;]/.test(cleanName)
    ? `"${cleanName.replaceAll('"', '\\"')}"`
    : cleanName;
  return `${quotedName} <${address.trim()}>`;
}

function splitHeaderList(value: string) {
  const parts: string[] = [];
  let current = "";
  let quoted = false;
  let escaped = false;
  let angleDepth = 0;
  let commentDepth = 0;

  for (const character of value) {
    if (escaped) {
      current += character;
      escaped = false;
      continue;
    }
    if (character === "\\" && quoted) {
      current += character;
      escaped = true;
      continue;
    }
    if (character === '"') quoted = !quoted;
    if (!quoted) {
      if (character === "<") angleDepth += 1;
      if (character === ">") angleDepth = Math.max(0, angleDepth - 1);
      if (character === "(") commentDepth += 1;
      if (character === ")") commentDepth = Math.max(0, commentDepth - 1);
    }
    if (
      (character === "," || character === ";") &&
      !quoted &&
      angleDepth === 0 &&
      commentDepth === 0
    ) {
      if (current.trim()) parts.push(stripGroupPrefix(current));
      current = "";
      continue;
    }
    current += character;
  }
  if (current.trim()) parts.push(stripGroupPrefix(current));
  return parts;
}

function stripGroupPrefix(value: string) {
  const colon = value.indexOf(":");
  return colon >= 0 && !value.slice(0, colon).includes("@")
    ? value.slice(colon + 1).trim()
    : value.trim();
}

function parseAddress(value: string): MailAddress | undefined {
  const angle = value.match(/^(.*?)<\s*([^<>]+)\s*>\s*$/);
  const commentForm = !angle
    ? value.match(/^([^\s<>(),;]+@[^\s<>(),;]+)\s*(?:\(([^()]*)\))?\s*$/)
    : undefined;
  const address = (angle?.[2] ?? commentForm?.[1] ?? value).trim();
  if (!isUsableAddress(address)) return undefined;
  const rawName = angle?.[1]
    .trim()
    .replace(/\([^)]*\)\s*$/, "")
    .trim();
  const nameSource = rawName || commentForm?.[2]?.trim();
  const name = nameSource
    ? nameSource.replace(/^"(.*)"$/, "$1").replaceAll('\\"', '"')
    : undefined;
  return { name: name || undefined, address };
}

function isUsableAddress(value: string) {
  return /^[^\s@<>]+@(?:[^\s@<>]+\.[^\s@<>]+|\[[^\]\s]+\])$/.test(value.trim());
}

function normalizeAddress(value: string) {
  return value.trim().toLocaleLowerCase();
}
