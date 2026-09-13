import {
  IconCheck,
  IconAlertTriangle,
  IconChevronDown,
  IconPaperclip,
  IconSend,
  IconSparkles,
  IconX,
} from "@tabler/icons-react";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { DragEvent } from "react";
import { useTranslation } from "react-i18next";
import { AI_FEATURES_VISIBLE } from "../features";
import { api } from "../api";
import type { ComposeSeed } from "../composeWindow";
import type { Account, ComposeAttachment } from "../types";
import { recipientAddressIdentity, splitAddressValues } from "../recipients";
import { RecipientCombobox } from "./RecipientCombobox";
import { RichTextEditor } from "./RichTextEditor";
import {
  isRichTextEmpty,
  plainTextFromRichText,
  richTextFromPlainText,
  sanitizeRichText,
} from "./richText";

const MAX_ATTACHMENT_BYTES = 25 * 1024 * 1024;
const MAX_ATTACHMENT_TOTAL_BYTES = 50 * 1024 * 1024;
const MAX_ATTACHMENTS = 50;

type Props = {
  accounts: Account[];
  seed?: ComposeSeed;
  sendState: "idle" | "sending" | "sent";
  aiConnected: boolean;
  onSend: (draft: Record<string, unknown>) => void;
  onAiDraft: (instruction: string) => Promise<string>;
};

export function Composer({
  accounts,
  seed,
  sendState,
  aiConnected,
  onSend,
  onAiDraft,
}: Props) {
  const { t } = useTranslation();
  const [accountId, setAccountId] = useState(
    seed?.accountId ?? accounts[0]?.id,
  );
  const [to, setTo] = useState(seed?.to ?? "");
  const [cc, setCc] = useState(seed?.cc ?? "");
  const [bcc, setBcc] = useState(seed?.bcc ?? "");
  const [subject, setSubject] = useState(seed?.subject ?? "");
  const [bodyHtml, setBodyHtml] = useState(() => {
    if (seed?.bodyHtml === undefined) {
      return richTextFromPlainText(seed?.body ?? "");
    }
    const sanitizedHtml = sanitizeRichText(seed.bodyHtml, {
      preserveQuotedEmail: true,
    });
    return seed.bodyHtml && isRichTextEmpty(sanitizedHtml)
      ? richTextFromPlainText(seed?.body ?? "")
      : sanitizedHtml;
  });
  const [showCopies, setShowCopies] = useState(Boolean(seed?.cc || seed?.bcc));
  const [recipientErrors, setRecipientErrors] = useState<{
    to?: string;
    cc?: string;
    bcc?: string;
  }>({});
  const [aiLoading, setAiLoading] = useState(false);
  const [attachments, setAttachments] = useState<ComposeAttachment[]>(
    seed?.attachments ?? [],
  );
  const [attachmentError, setAttachmentError] = useState<string>();
  const [isDraggingFiles, setIsDraggingFiles] = useState(false);
  const [sendSubmissionPending, setSendSubmissionPending] = useState(false);
  const attachmentInputRef = useRef<HTMLInputElement>(null);
  const attachmentsRef = useRef<ComposeAttachment[]>(seed?.attachments ?? []);
  const browserDropHandledRef = useRef(false);
  const sendSubmissionPendingRef = useRef(false);
  const previousSendStateRef = useRef(sendState);
  const enabledAccounts = useMemo(
    () => accounts.filter((account) => account.enabled),
    [accounts],
  );

  useEffect(() => {
    if (enabledAccounts.some((account) => account.id === accountId)) return;
    setAccountId(enabledAccounts[0]?.id);
  }, [accountId, enabledAccounts]);

  useEffect(() => {
    attachmentsRef.current = attachments;
  }, [attachments]);

  const selectedAccount = useMemo(
    () => enabledAccounts.find((account) => account.id === accountId),
    [accountId, enabledAccounts],
  );
  const recipientExclusions = useMemo(() => {
    const identities = (values: string[]) =>
      new Set(
        values
          .map(recipientAddressIdentity)
          .filter((value): value is string => Boolean(value)),
      );
    return {
      to: identities([...splitAddressValues(cc), ...splitAddressValues(bcc)]),
      cc: identities([...splitAddressValues(to), ...splitAddressValues(bcc)]),
      bcc: identities([...splitAddressValues(to), ...splitAddressValues(cc)]),
    };
  }, [bcc, cc, to]);
  const sending = sendState !== "idle";
  const busy = sending || sendSubmissionPending;
  const canSend = Boolean(accountId && to.trim() && !busy);

  useEffect(() => {
    const wasSending = previousSendStateRef.current === "sending";
    if (
      sendState === "idle" &&
      wasSending &&
      sendSubmissionPendingRef.current
    ) {
      sendSubmissionPendingRef.current = false;
      setSendSubmissionPending(false);
    }
    previousSendStateRef.current = sendState;
  }, [sendState]);

  const send = async () => {
    if (
      !accountId ||
      !to.trim() ||
      sending ||
      sendSubmissionPendingRef.current
    ) {
      return;
    }
    // React state does not update until after this event finishes. Keep a ref in
    // sync so a rapid click or Cmd/Ctrl+Enter cannot start another validation.
    sendSubmissionPendingRef.current = true;
    setSendSubmissionPending(true);
    const rawRecipients = {
      to: splitAddresses(to),
      cc: splitAddresses(cc),
      bcc: splitAddresses(bcc),
    };
    // RecipientCombobox protects interactive commits, but the controlled
    // fields also publish unfinished drafts. Normalize the final envelope
    // here so a duplicate cannot slip through by clicking Send before a draft
    // is committed. Invalid values deliberately remain untouched and are
    // still reported by the Rust validator below.
    const recipients = deduplicateComposeRecipients(rawRecipients);
    const nextTo = recipients.to.join(", ");
    const nextCc = recipients.cc.join(", ");
    const nextBcc = recipients.bcc.join(", ");
    if (nextTo !== to) setTo(nextTo);
    if (nextCc !== cc) setCc(nextCc);
    if (nextBcc !== bcc) setBcc(nextBcc);
    let validation;
    try {
      validation = await api.validateComposeRecipients(recipients);
    } catch {
      setRecipientErrors({ to: t("composer.recipientValidationUnavailable") });
      sendSubmissionPendingRef.current = false;
      setSendSubmissionPending(false);
      return;
    }
    const errors = recipientValidationErrors(validation, t);
    if (errors.to || errors.cc || errors.bcc) {
      setRecipientErrors(errors);
      sendSubmissionPendingRef.current = false;
      setSendSubmissionPending(false);
      return;
    }
    setRecipientErrors({});
    onSend({
      account_id: accountId,
      ...recipients,
      subject,
      body_text: plainTextFromRichText(bodyHtml),
      body_html: isRichTextEmpty(bodyHtml) ? null : bodyHtml,
      in_reply_to: seed?.inReplyTo ?? null,
      references: seed?.references ?? null,
      attachments: attachments.map(
        ({ filename, mime_type, content_base64 }) => ({
          filename,
          mime_type,
          content_base64,
        }),
      ),
    });
  };

  const addAttachments = useCallback(
    (incoming: ComposeAttachment[]) => {
      const additions = uniqueAttachments(attachmentsRef.current, incoming);
      if (!additions.length) {
        return;
      }
      const existingBytes = attachmentsRef.current.reduce(
        (sum, attachment) => sum + attachment.size_bytes,
        0,
      );
      const incomingBytes = additions.reduce(
        (sum, attachment) => sum + attachment.size_bytes,
        0,
      );
      if (
        additions.some(
          (attachment) => attachment.size_bytes > MAX_ATTACHMENT_BYTES,
        ) ||
        existingBytes + incomingBytes > MAX_ATTACHMENT_TOTAL_BYTES ||
        attachmentsRef.current.length + additions.length > MAX_ATTACHMENTS
      ) {
        setAttachmentError(t("composer.attachmentLimit"));
        return;
      }
      const next = [...attachmentsRef.current, ...additions];
      attachmentsRef.current = next;
      setAttachments(next);
      setAttachmentError(undefined);
    },
    [t],
  );

  const addFiles = useCallback(
    async (files: FileList | File[]) => {
      try {
        addAttachments(
          await Promise.all(Array.from(files).map(fileToAttachment)),
        );
      } catch {
        setAttachmentError(t("composer.attachmentReadError"));
      }
    },
    [addAttachments, t],
  );

  const receiveNativeDrop = useCallback(
    async (receipt: string) => {
      try {
        addAttachments(await api.readDroppedFiles(receipt));
      } catch {
        setAttachmentError(t("composer.attachmentReadError"));
      }
    },
    [addAttachments, t],
  );

  useEffect(() => {
    if (!("__TAURI_INTERNALS__" in window)) return;
    let unlistenDragDrop: (() => void) | undefined;
    let unlistenReceipts: (() => void) | undefined;
    let unlistenErrors: (() => void) | undefined;
    let disposed = false;
    const webview = getCurrentWebview();
    void webview
      .onDragDropEvent((event) => {
        switch (event.payload.type) {
          case "enter":
          case "over":
            setIsDraggingFiles(true);
            break;
          case "leave":
            setIsDraggingFiles(false);
            break;
          case "drop":
            setIsDraggingFiles(false);
            break;
        }
      })
      .then((dispose) => {
        if (disposed) dispose();
        else unlistenDragDrop = dispose;
      })
      .catch(() => {
        if (!disposed) setAttachmentError(t("composer.attachmentReadError"));
      });
    void webview
      .listen<string>("dakia://dropped-file-receipt", (event) => {
        if (busy) return;
        window.setTimeout(() => {
          if (browserDropHandledRef.current) {
            browserDropHandledRef.current = false;
            return;
          }
          void receiveNativeDrop(event.payload);
        }, 0);
      })
      .then((dispose) => {
        if (disposed) dispose();
        else unlistenReceipts = dispose;
      })
      .catch(() => {
        if (!disposed) setAttachmentError(t("composer.attachmentReadError"));
      });
    void webview
      .listen<string>("dakia://dropped-file-error", () => {
        setAttachmentError(t("composer.attachmentReadError"));
      })
      .then((dispose) => {
        if (disposed) dispose();
        else unlistenErrors = dispose;
      })
      .catch(() => {
        if (!disposed) setAttachmentError(t("composer.attachmentReadError"));
      });
    return () => {
      disposed = true;
      unlistenDragDrop?.();
      unlistenReceipts?.();
      unlistenErrors?.();
    };
  }, [busy, receiveNativeDrop, t]);

  const onBrowserDrop = (event: DragEvent<HTMLElement>) => {
    event.preventDefault();
    setIsDraggingFiles(false);
    if (!busy && event.dataTransfer.files.length) {
      browserDropHandledRef.current = true;
      window.setTimeout(() => {
        browserDropHandledRef.current = false;
      }, 250);
      void addFiles(event.dataTransfer.files);
    }
  };

  const removeAttachment = (index: number) => {
    const next = attachmentsRef.current.filter(
      (_, currentIndex) => currentIndex !== index,
    );
    attachmentsRef.current = next;
    setAttachments(next);
  };

  const aiDraft = async () => {
    setAiLoading(true);
    try {
      setBodyHtml(
        richTextFromPlainText(await onAiDraft(t("ai.draftInstruction"))),
      );
    } finally {
      setAiLoading(false);
    }
  };

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && event.key === "Enter") {
        event.preventDefault();
        if (canSend) void send();
      }
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  });

  return (
    <main
      className="compose-window"
      data-send-state={sendState}
      onDragEnter={(event) => {
        if (!event.dataTransfer.types.includes("Files")) return;
        event.preventDefault();
        setIsDraggingFiles(true);
      }}
      onDragOver={(event) => {
        if (!event.dataTransfer.types.includes("Files")) return;
        event.preventDefault();
        event.dataTransfer.dropEffect = "copy";
      }}
      onDragLeave={(event) => {
        if (event.currentTarget.contains(event.relatedTarget as Node)) return;
        setIsDraggingFiles(false);
      }}
      onDrop={onBrowserDrop}
    >
      <div className="compose-titlebar" data-tauri-drag-region />
      <section className="compose-envelope" aria-label={t("composer.title")}>
        <div className="compose-field compose-recipient-field">
          <label htmlFor="compose-to">{t("composer.to")}</label>
          <RecipientCombobox
            id="compose-to"
            value={to}
            label={t("composer.to")}
            onChange={(value) => {
              setTo(value);
              setRecipientErrors((errors) => ({ ...errors, to: undefined }));
            }}
            accountId={accountId}
            excludedAddresses={recipientExclusions.to}
            error={recipientErrors.to}
            autoFocus
            disabled={busy}
          />
          <button
            className="compose-copy-toggle"
            type="button"
            aria-expanded={showCopies}
            onClick={() => setShowCopies((value) => !value)}
            disabled={busy}
          >
            {t("composer.cc")} · {t("composer.bcc")}
          </button>
        </div>
        {showCopies && (
          <>
            <div className="compose-field">
              <label htmlFor="compose-cc">{t("composer.cc")}</label>
              <RecipientCombobox
                id="compose-cc"
                value={cc}
                label={t("composer.cc")}
                onChange={(value) => {
                  setCc(value);
                  setRecipientErrors((errors) => ({
                    ...errors,
                    cc: undefined,
                  }));
                }}
                accountId={accountId}
                excludedAddresses={recipientExclusions.cc}
                error={recipientErrors.cc}
                disabled={busy}
              />
            </div>
            <div className="compose-field">
              <label htmlFor="compose-bcc">{t("composer.bcc")}</label>
              <RecipientCombobox
                id="compose-bcc"
                value={bcc}
                label={t("composer.bcc")}
                onChange={(value) => {
                  setBcc(value);
                  setRecipientErrors((errors) => ({
                    ...errors,
                    bcc: undefined,
                  }));
                }}
                accountId={accountId}
                excludedAddresses={recipientExclusions.bcc}
                error={recipientErrors.bcc}
                disabled={busy}
              />
            </div>
          </>
        )}
        <div className="compose-field compose-from-field">
          <label htmlFor="compose-from">{t("composer.from")}</label>
          <div className="compose-account-select">
            <select
              id="compose-from"
              value={accountId ?? ""}
              onChange={(event) => setAccountId(event.currentTarget.value)}
              aria-label={t("composer.from")}
              disabled={busy}
            >
              {enabledAccounts.map((account) => (
                <option key={account.id} value={account.id}>
                  {account.display_name
                    ? `${account.display_name} <${account.email}>`
                    : account.email}
                </option>
              ))}
            </select>
            <span aria-hidden="true">
              <IconChevronDown size={14} stroke={1.8} />
            </span>
          </div>
          {selectedAccount && (
            <span
              className="compose-account-ready"
              title={selectedAccount.email}
            >
              <IconCheck size={13} stroke={2.2} />
            </span>
          )}
        </div>
        <div className="compose-field">
          <label htmlFor="compose-subject">{t("composer.subject")}</label>
          <input
            id="compose-subject"
            value={subject}
            onChange={(event) => setSubject(event.currentTarget.value)}
            disabled={busy}
          />
        </div>
      </section>

      <RichTextEditor value={bodyHtml} onChange={setBodyHtml} disabled={busy} />

      {isDraggingFiles ? (
        <section
          className="compose-drop-zone"
          aria-label={t("composer.dropFiles")}
        >
          <IconPaperclip size={24} stroke={1.6} />
          <strong>{t("composer.dropFiles")}</strong>
          <span>{t("composer.dropFilesHint")}</span>
        </section>
      ) : null}

      {attachmentError ? (
        <p className="compose-attachment-error" role="alert">
          <IconAlertTriangle size={14} /> {attachmentError}
        </p>
      ) : null}

      {attachments.length ? (
        <section
          className="compose-attachment-tray"
          aria-label={t("composer.attachments")}
        >
          {attachments.map((attachment, index) => (
            <div
              className="compose-attachment-chip"
              key={`${attachment.filename}-${index}`}
            >
              <IconPaperclip size={15} stroke={1.8} aria-hidden="true" />
              <span title={attachment.filename}>{attachment.filename}</span>
              <small>{formatBytes(attachment.size_bytes)}</small>
              <button
                type="button"
                aria-label={t("composer.removeAttachment", {
                  filename: attachment.filename,
                })}
                title={t("composer.removeAttachment", {
                  filename: attachment.filename,
                })}
                onClick={() => removeAttachment(index)}
                disabled={busy}
              >
                <IconX size={14} stroke={2} />
              </button>
            </div>
          ))}
        </section>
      ) : null}

      <footer className="compose-toolbar">
        <button
          className="compose-send-button"
          type="button"
          onClick={send}
          disabled={!canSend}
          data-send-state={sendState}
        >
          {sendState === "sent" ? (
            <IconCheck size={16} stroke={2.2} />
          ) : (
            <IconSend className="compose-send-icon" size={16} stroke={1.9} />
          )}
          <span>
            {sendState === "sent"
              ? t("composer.sent")
              : sendState === "sending"
                ? t("composer.sending")
                : t("actions.send")}
          </span>
          <kbd>⌘↵</kbd>
        </button>
        {AI_FEATURES_VISIBLE && aiConnected ? (
          <button
            className="compose-ai-button"
            type="button"
            onClick={aiDraft}
            disabled={aiLoading || busy}
          >
            <IconSparkles size={17} stroke={1.8} />
            {aiLoading ? t("ai.working") : t("actions.draftWithAi")}
          </button>
        ) : null}
        <input
          ref={attachmentInputRef}
          className="compose-attachment-input"
          type="file"
          multiple
          onChange={(event) => {
            if (event.currentTarget.files?.length)
              void addFiles(event.currentTarget.files);
            event.currentTarget.value = "";
          }}
        />
        <button
          className="compose-attachment-button"
          type="button"
          onClick={() => attachmentInputRef.current?.click()}
          disabled={busy}
        >
          <IconPaperclip size={17} stroke={1.8} />
          {t("composer.attach")}
        </button>
        <span className="compose-format-label">{t("composer.richText")}</span>
      </footer>
    </main>
  );
}

const splitAddresses = (value: string) => splitAddressValues(value);

function deduplicateComposeRecipients(fields: {
  to: string[];
  cc: string[];
  bcc: string[];
}) {
  const seen = new Set<string>();
  const deduplicate = (values: string[]) =>
    values.filter((value) => {
      const identity = recipientAddressIdentity(value);
      // Leave malformed text in place. The Rust parser owns validation and
      // must show the user exactly what needs fixing.
      if (!identity) return true;
      if (seen.has(identity)) return false;
      seen.add(identity);
      return true;
    });
  return {
    to: deduplicate(fields.to),
    cc: deduplicate(fields.cc),
    bcc: deduplicate(fields.bcc),
  };
}

function recipientValidationErrors(
  validation: Awaited<ReturnType<typeof api.validateComposeRecipients>>,
  t: ReturnType<typeof useTranslation>["t"],
) {
  const errorFor = (field: (typeof validation)["to"]) =>
    field.invalid.length
      ? t("composer.recipientRejected", {
          recipients: field.invalid.join(", "),
        })
      : field.valid
        ? undefined
        : t("composer.recipientInvalid");
  return {
    to: errorFor(validation.to),
    cc: errorFor(validation.cc),
    bcc: errorFor(validation.bcc),
  };
}

async function fileToAttachment(file: File): Promise<ComposeAttachment> {
  const bytes = new Uint8Array(await file.arrayBuffer());
  let binary = "";
  for (let offset = 0; offset < bytes.length; offset += 8192) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + 8192));
  }
  return {
    filename: file.name,
    mime_type: file.type || "application/octet-stream",
    content_base64: btoa(binary),
    size_bytes: file.size,
  };
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.ceil(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function uniqueAttachments(
  existing: ComposeAttachment[],
  incoming: ComposeAttachment[],
) {
  const content = new Set(
    existing.map((attachment) => attachment.content_base64),
  );
  return incoming.filter((attachment) => {
    if (content.has(attachment.content_base64)) return false;
    content.add(attachment.content_base64);
    return true;
  });
}
