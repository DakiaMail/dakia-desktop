export type SplitMessageText = {
  visible: string;
  history?: string;
};

const originalMessage =
  /^-{2,}\s*(?:Original Message|Forwarded message)\s*-{2,}\s*$/i;
const beginForwarded = /^Begin forwarded message:\s*$/i;
const wroteLine = /^On .+\bwrote:\s*$/i;
const outlookHeaderStart = /^From:\s*\S/i;
const outlookHeaderLabel = /^(?:Sent|To|Cc|Subject):\s*\S/i;

// Outlook desktop replies separate new content from quoted history with a
// From/Sent/To/Subject header block instead of ">" prefixes.
function isOutlookHeaderStart(lines: string[], index: number): boolean {
  if (!outlookHeaderStart.test(lines[index].trim())) return false;
  for (const line of lines.slice(index + 1, index + 6)) {
    const trimmed = line.trim();
    if (!trimmed) return false;
    if (outlookHeaderLabel.test(trimmed)) return true;
  }
  return false;
}

export function splitQuotedText(value: string): SplitMessageText {
  const lines = value.split(/\r?\n/);
  const separator = lines.findIndex(
    (line, index) =>
      originalMessage.test(line.trim()) ||
      beginForwarded.test(line.trim()) ||
      isOutlookHeaderStart(lines, index) ||
      (wroteLine.test(line.trim()) &&
        lines.slice(index + 1).some((next) => /^\s*>/.test(next))),
  );
  if (separator >= 0) return splitAt(lines, separator);

  const quoteStart = lines.findIndex((line) => /^\s*>/.test(line));
  if (quoteStart >= 0) {
    const tail = lines.slice(quoteStart);
    const quotedLines = tail.filter((line) => /^\s*>/.test(line)).length;
    const nonBlankLines = tail.filter((line) => line.trim()).length;
    if (quotedLines >= 2 && quotedLines === nonBlankLines) {
      return splitAt(lines, quoteStart);
    }
  }
  return { visible: value };
}

function splitAt(lines: string[], index: number): SplitMessageText {
  return {
    visible: lines.slice(0, index).join("\n").trimEnd(),
    history: lines.slice(index).join("\n").trim(),
  };
}
