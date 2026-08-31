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
import { splitAddressValues } from "../recipients";
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
    const sanitizedHtml = sanitizeRichText(seed?.bodyHtml ?? "");
    return seed.bodyHtml && isRichTextEmpty(sanitizedHtml)
      ? richTextFromPlainText(seed?.body ?? "")
      : sanitizedHtml;
  });
  const [showCopies, setShowCopies] = useState(Boolean(seed?.cc || seed?.bcc));
  const [aiLoading, setAiLoading] = useState(false);
  const [attachments, setAttachments] = useState<ComposeAttachment[]>(
    seed?.attachments ?? [],
  );
  const [attachmentError, setAttachmentError] = useState<string>();
  const [isDraggingFiles, setIsDraggingFiles] = useState(false);
  const attachmentInputRef = useRef<HTMLInputElement>(null);
  const attachmentsRef = useRef<ComposeAttachment[]>(seed?.attachments ?? []);
  const browserDropHandledRef = useRef(false);

  useEffect(() => {
    if (!accountId && accounts[0]) setAccountId(accounts[0].id);
  }, [accountId, accounts]);

  useEffect(() => {
    attachmentsRef.current = attachments;
  }, [attachments]);

  const selectedAccount = useMemo(
    () => accounts.find((account) => account.id === accountId),
    [accountId, accounts],
  );
  const sending = sendState !== "idle";
  const canSend = Boolean(accountId && to.trim() && !sending);
  const send = () => {
    if (!accountId || !to.trim()) return;
    onSend({
      account_id: accountId,
      to: splitAddresses(to),
      cc: splitAddresses(cc),
      bcc: splitAddresses(bcc),
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
        if (sending) return;
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
  }, [receiveNativeDrop, sending, t]);

  const onBrowserDrop = (event: DragEvent<HTMLElement>) => {
    event.preventDefault();
    setIsDraggingFiles(false);
    if (!sending && event.dataTransfer.files.length) {
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
        if (canSend) send();
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
          <input
            id="compose-to"
            value={to}
            onChange={(event) => setTo(event.currentTarget.value)}
            autoFocus
            autoComplete="off"
            spellCheck={false}
            disabled={sending}
          />
          <button
            className="compose-copy-toggle"
            type="button"
            aria-expanded={showCopies}
            onClick={() => setShowCopies((value) => !value)}
            disabled={sending}
          >
            {t("composer.cc")} · {t("composer.bcc")}
          </button>
        </div>
        {showCopies && (
          <>
            <div className="compose-field">
              <label htmlFor="compose-cc">{t("composer.cc")}</label>
              <input
                id="compose-cc"
                value={cc}
                onChange={(event) => setCc(event.currentTarget.value)}
                autoComplete="off"
                spellCheck={false}
                disabled={sending}
              />
            </div>
            <div className="compose-field">
              <label htmlFor="compose-bcc">{t("composer.bcc")}</label>
              <input
                id="compose-bcc"
                value={bcc}
                onChange={(event) => setBcc(event.currentTarget.value)}
                autoComplete="off"
                spellCheck={false}
                disabled={sending}
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
              disabled={sending}
            >
              {accounts.map((account) => (
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
            disabled={sending}
          />
        </div>
      </section>

      <RichTextEditor
        value={bodyHtml}
        onChange={setBodyHtml}
        disabled={sending}
      />

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
                disabled={sending}
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
            disabled={aiLoading || sending}
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
          disabled={sending}
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
