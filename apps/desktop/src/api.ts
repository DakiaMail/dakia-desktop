import { Channel, invoke } from "@tauri-apps/api/core";
import { mailboxFamily } from "./mailActions";
import { groupMessages } from "./threads";
import type {
  Account,
  AccountConnection,
  Attachment,
  AiSettings,
  AiInput,
  AiMessageTarget,
  ComposeAttachment,
  MailRebuildProgress,
  MailCursor,
  MailSummary,
  ConversationTarget,
  MessageContent,
  MailThread,
  MailThreadPage,
  SmartInboxPage,
  RemoteSearchResult,
  SmartSectionId,
  MessageContentErrorKind,
  Provider,
  SyncProgress,
  SyncResult,
  RealtimeSyncStatus,
  SendRecoveryPage,
  SendRecoverySeed,
  SendRecoveryStatus,
  TranslationDownloadProgress,
  TranslationLanguageDetection,
  TranslationModelFiles,
  TranslationModelStatus,
  SendFailureKind,
} from "./types";

const messageContentErrorKinds = new Set<MessageContentErrorKind>([
  "resource_limit",
  "malformed",
  "undecodable",
  "unsupported",
  "transient",
]);

function normalizeSenderAddress(address: string) {
  return address
    .trim()
    .replace(/[A-Z]/g, (character) => character.toLowerCase());
}

/**
 * The hostile-mail fixture is strictly an opt-in native verification aid.
 * Limit it to a non-privileged loopback HTTP origin so a development setting
 * cannot turn the normal demo mailbox into a network client for another host.
 */
export function mailSafetyOrigin(value: string | undefined) {
  const match = /^http:\/\/127\.0\.0\.1:([1-9]\d{3,4})\/?$/.exec(value ?? "");
  if (!match) return undefined;
  const port = Number(match[1]);
  if (!Number.isInteger(port) || port < 1024 || port > 65535) return undefined;
  return `http://127.0.0.1:${port}`;
}

export class MessageContentError extends Error {
  readonly retryable: boolean;

  constructor(readonly kind: MessageContentErrorKind) {
    super("Message content could not be loaded");
    this.name = "MessageContentError";
    this.retryable = kind === "transient";
  }
}

/** A native bulk save can complete some files before a later safe failure. */
export class AttachmentSaveError extends Error {
  constructor(
    readonly savedCount?: number,
    readonly totalCount?: number,
  ) {
    super(
      savedCount === undefined || totalCount === undefined
        ? "Attachment save failed"
        : `Saved ${savedCount} of ${totalCount} attachments`,
    );
    this.name = "AttachmentSaveError";
  }
}

export function attachmentSaveErrorFromUnknown(
  error: unknown,
): AttachmentSaveError {
  if (error instanceof AttachmentSaveError) return error;
  const value =
    typeof error === "string"
      ? (() => {
          try {
            return JSON.parse(error) as unknown;
          } catch {
            return undefined;
          }
        })()
      : error;
  if (
    value &&
    typeof value === "object" &&
    "kind" in value &&
    value.kind === "partial" &&
    "savedCount" in value &&
    typeof value.savedCount === "number" &&
    Number.isSafeInteger(value.savedCount) &&
    value.savedCount >= 0 &&
    "totalCount" in value &&
    typeof value.totalCount === "number" &&
    Number.isSafeInteger(value.totalCount) &&
    value.totalCount > value.savedCount
  ) {
    return new AttachmentSaveError(value.savedCount, value.totalCount);
  }
  return new AttachmentSaveError();
}

const sendFailureKinds = new Set<SendFailureKind>([
  "not_submitted",
  "delivery_uncertain",
  "sent_copy_failed",
]);

/**
 * The native command returns a serialized, typed failure while keeping its
 * command signature compatible with existing clients. Do not surface the
 * provider diagnostic here: it can include transport details that are not
 * useful or safe in a compose window.
 */
export class SendMessageError extends Error {
  constructor(readonly kind: SendFailureKind) {
    super(
      kind === "sent_copy_failed"
        ? "Message was sent but no Sent copy was saved."
        : kind === "delivery_uncertain"
          ? "Delivery status is uncertain."
          : "Message was not sent.",
    );
    this.name = "SendMessageError";
  }
}

export function sendMessageErrorFromUnknown(error: unknown): SendMessageError {
  if (error instanceof SendMessageError) return error;
  const value =
    typeof error === "string"
      ? (() => {
          try {
            return JSON.parse(error) as unknown;
          } catch {
            return undefined;
          }
        })()
      : error;
  if (
    value &&
    typeof value === "object" &&
    "kind" in value &&
    typeof value.kind === "string" &&
    sendFailureKinds.has(value.kind as SendFailureKind)
  ) {
    return new SendMessageError(value.kind as SendFailureKind);
  }
  // IPC interruption leaves us unable to distinguish a rejected submission
  // from one accepted immediately before the response was lost. Never make
  // that state retryable without an explicit native outcome.
  return new SendMessageError("delivery_uncertain");
}

/**
 * Converts the native error envelope into the only error shape Reader needs.
 * Unknown/rejected IPC payloads remain retryable and never become user text.
 */
export function messageContentErrorFromUnknown(
  error: unknown,
): MessageContentError {
  if (error instanceof MessageContentError) return error;
  if (
    error &&
    typeof error === "object" &&
    "kind" in error &&
    typeof error.kind === "string" &&
    messageContentErrorKinds.has(error.kind as MessageContentErrorKind)
  ) {
    return new MessageContentError(error.kind as MessageContentErrorKind);
  }
  return new MessageContentError("transient");
}

const sendRecoveryStatuses = new Set<SendRecoveryStatus>([
  "not_submitted",
  "pending",
  "delivery_uncertain",
  "sent_copy_failed",
]);

export function sendRecoveryPageFromUnknown(
  value: unknown,
  allowedAccountIds?: readonly string[],
): SendRecoveryPage {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new Error("Saved messages could not be loaded.");
  }
  const page = value as Record<string, unknown>;
  if (
    !Array.isArray(page.items) ||
    !(
      page.next_cursor === null ||
      (typeof page.next_cursor === "string" && page.next_cursor.length > 0)
    ) ||
    !Number.isSafeInteger(page.total_count) ||
    Number(page.total_count) < page.items.length
  ) {
    throw new Error("Saved messages could not be loaded.");
  }
  const allowedAccounts = allowedAccountIds
    ? new Set(allowedAccountIds)
    : undefined;
  const identities = new Set<string>();
  const items = page.items.map((item) => {
    if (!item || typeof item !== "object" || Array.isArray(item)) {
      throw new Error("Saved messages could not be loaded.");
    }
    const summary = item as Record<string, unknown>;
    if (
      typeof summary.account_id !== "string" ||
      typeof summary.attempt_id !== "string" ||
      typeof summary.subject !== "string" ||
      typeof summary.status !== "string" ||
      !sendRecoveryStatuses.has(summary.status as SendRecoveryStatus) ||
      typeof summary.updated_at !== "string" ||
      (allowedAccounts !== undefined &&
        !allowedAccounts.has(summary.account_id))
    ) {
      throw new Error("Saved messages could not be loaded.");
    }
    const identity = `${summary.account_id}\u0000${summary.attempt_id}`;
    if (identities.has(identity)) {
      throw new Error("Saved messages could not be loaded.");
    }
    identities.add(identity);
    return {
      account_id: summary.account_id,
      attempt_id: summary.attempt_id,
      subject: summary.subject,
      status: summary.status as SendRecoveryStatus,
      updated_at: summary.updated_at,
    };
  });
  return {
    items,
    next_cursor: page.next_cursor as string | null,
    total_count: Number(page.total_count),
  };
}

const desktopApi = {
  providers: () => invoke<Provider[]>("provider_presets"),
  terminalCommandStatus: () =>
    invoke<"available" | "notSetUp" | "conflict">("terminal_command_status"),
  installTerminalCommand: () => invoke<void>("install_terminal_command"),
  removeTerminalCommand: () => invoke<void>("remove_terminal_command"),
  configureTray: (openLabel: string, quitLabel: string) =>
    invoke<void>("configure_tray", { openLabel, quitLabel }),
  accounts: () => invoke<Account[]>("accounts"),
  updateAccount: (input: Record<string, unknown>) =>
    invoke<Account>("update_account", { input }),
  showAccountContextMenu: (accountId: string, renameLabel: string) =>
    invoke<void>("show_account_context_menu", { accountId, renameLabel }),
  showEmailAddressContextMenu: (
    accountId: string,
    address: string,
    copyLabel: string,
    newMessageLabel: string,
  ) =>
    invoke<void>("show_email_address_context_menu", {
      accountId,
      address,
      copyLabel,
      newMessageLabel,
    }),
  removeAccount: (accountId: string) =>
    invoke<void>("remove_account", { accountId }),
  addAccount: (draft: Record<string, unknown>, password: string) =>
    invoke<AccountConnection>("add_account", { input: { draft, password } }),
  search: (
    text = "",
    accountIds: string[] = [],
    mailbox?: string,
    unreadOnly = false,
    flaggedOnly = false,
    limit = 100,
    cursor?: MailCursor | null,
    category?: string,
    unflaggedOnly = false,
    readOnly = false,
  ) =>
    invoke<MailThreadPage>("search", {
      query: {
        text,
        account_ids: accountIds,
        mailbox,
        from: null,
        unread_only: unreadOnly,
        read_only: readOnly,
        flagged_only: flaggedOnly,
        category,
        unflagged_only: unflaggedOnly,
        limit,
        cursor,
      },
    }),
  smartInbox: (accountIds: string[], limit = 3) =>
    invoke<SmartInboxPage>("search_smart_inbox", {
      query: { account_ids: accountIds, limit },
    }),
  searchRemote: (
    text: string,
    accountIds: string[] = [],
    mailbox?: string,
    unreadOnly = false,
    flaggedOnly = false,
  ) =>
    invoke<RemoteSearchResult>("search_remote", {
      query: {
        text,
        account_ids: accountIds,
        mailbox,
        from: null,
        unread_only: unreadOnly,
        flagged_only: flaggedOnly,
        limit: 500,
      },
    }),
  conversationForTarget: (target: ConversationTarget) =>
    invoke<MailThread | null>("conversation_for_target", { target }),
  setCategory: (
    messageId: string,
    category: string,
    expectedUidValidity: number | null = null,
  ) =>
    invoke<void>("set_message_category", {
      messageId,
      category,
      expectedUidValidity,
    }),
  setStarred: (
    messageId: string,
    starred: boolean,
    expectedUidValidity?: number | null,
  ) =>
    invoke<MailSummary>("set_message_starred", {
      messageId,
      starred,
      expectedUidValidity: expectedUidValidity ?? null,
    }),
  setRead: (
    messageId: string,
    read: boolean,
    expectedUidValidity?: number | null,
  ) =>
    invoke<void>("set_message_read", {
      messageId,
      read,
      expectedUidValidity: expectedUidValidity ?? null,
    }),
  starredCount: (accountIds: string[]) =>
    invoke<number>("starred_conversation_count", { accountIds }),
  classifyPending: () => invoke<number>("classify_pending"),
  startRealtimeSync: () => invoke<void>("start_realtime_sync"),
  reconcileRealtimeSync: () => invoke<void>("reconcile_realtime_sync"),
  realtimeSyncStatus: () =>
    invoke<RealtimeSyncStatus[]>("realtime_sync_status"),
  listSendRecoveries: async (accountIds: string[], cursor?: string) =>
    sendRecoveryPageFromUnknown(
      await invoke<unknown>("list_send_recoveries", {
        accountIds,
        cursor: cursor ?? null,
      }),
      accountIds,
    ),
  openSendRecoveryWindow: (accountId: string, attemptId: string) =>
    invoke<void>("open_send_recovery_window", { accountId, attemptId }),
  readSendRecoverySeed: (recoveryKey: string) =>
    invoke<SendRecoverySeed>("read_send_recovery_seed", { recoveryKey }),
  acknowledgeSendRecoverySeed: (recoveryKey: string) =>
    invoke<void>("acknowledge_send_recovery_seed", { recoveryKey }),
  recordNotificationDelivered: (
    accountId: string,
    eventId: string,
    detectedAt: string,
  ) =>
    invoke<void>("record_notification_delivered", {
      accountId,
      eventId,
      detectedAt,
    }),
  sendDesktopNotification: (notification: {
    title: string;
    body: string;
    accountId?: string;
    messageId?: string;
    rfcMessageId?: string;
    threadId?: string;
    count: number;
    sound?: string;
  }) => invoke<void>("send_desktop_notification", { notification }),
  hydrateMessage: (
    messageId: string,
    expectedUidValidity: number | null = null,
  ) =>
    invoke<MailSummary>("hydrate_message", {
      messageId,
      expectedUidValidity,
    }),
  sync: (
    accountId: string,
    onProgress?: (progress: SyncProgress) => void,
    full = false,
  ) => {
    const channel = new Channel<SyncProgress>();
    if (onProgress) channel.onmessage = onProgress;
    return invoke<SyncResult>("sync_account", {
      accountId,
      limit: full ? 250 : 50,
      full,
      onProgress: channel,
    });
  },
  mailRebuildStatus: () => invoke<MailRebuildProgress[]>("mail_rebuild_status"),
  content: async (
    messageId: string,
    expectedUidValidity: number | null = null,
  ) => {
    try {
      return await invoke<MessageContent>("message_content", {
        messageId,
        expectedUidValidity,
      });
    } catch (error) {
      throw messageContentErrorFromUnknown(error);
    }
  },
  attachments: (messageId: string, expectedUidValidity: number | null = null) =>
    invoke<Attachment[]>("message_attachments", {
      messageId,
      expectedUidValidity,
    }),
  saveAttachment: (
    messageId: string,
    attachmentId: string,
    expectedUidValidity: number | null = null,
  ) =>
    invoke<string>("save_attachment", {
      messageId,
      attachmentId,
      expectedUidValidity,
    }),
  saveAllAttachments: async (
    messageId: string,
    expectedUidValidity: number | null = null,
  ) => {
    try {
      return await invoke<string[]>("save_all_attachments", {
        messageId,
        expectedUidValidity,
      });
    } catch (error) {
      throw attachmentSaveErrorFromUnknown(error);
    }
  },
  exportMessage: (
    messageId: string,
    expectedUidValidity: number | null = null,
  ) => invoke<string>("export_message", { messageId, expectedUidValidity }),
  forwardAttachments: (
    messageId: string,
    expectedUidValidity: number | null = null,
  ) =>
    invoke<ComposeAttachment[]>("forward_attachments", {
      messageId,
      expectedUidValidity,
    }),
  readDroppedFiles: (receipt: string) =>
    invoke<ComposeAttachment[]>("read_dropped_files", { receipt }),
  send: async (draft: Record<string, unknown>) => {
    try {
      return await invoke<string>("send_message", { draft });
    } catch (error) {
      throw sendMessageErrorFromUnknown(error);
    }
  },
  action: (
    accountId: string,
    mailbox: string,
    uid: number,
    action: "archive" | "spam" | "not_spam" | "trash" | "delete",
    messageId?: string,
    expectedUidValidity?: number | null,
  ) =>
    invoke<void>("apply_mailbox_action", {
      accountId,
      mailbox,
      uid,
      action,
      messageId: messageId ?? null,
      expectedUidValidity: expectedUidValidity ?? null,
    }),
  openExternal: (url: string) => invoke<void>("open_external_url", { url }),
  unsubscribe: (messageId: string, expectedUidValidity: number | null = null) =>
    invoke<UnsubscribeResult>("unsubscribe_message", {
      messageId,
      expectedUidValidity,
    }),
  trashMessagesFromSender: (accountId: string, senderAddress: string) =>
    invoke<TrashMessagesFromSenderResult>("trash_messages_from_sender", {
      accountId,
      senderAddress,
    }),
  summarize: (settings: AiSettings, messageTargets: AiMessageTarget[]) =>
    invoke<string>("ai_summarize", {
      input: aiInput(settings, messageTargets),
    }),
  draft: (
    settings: AiSettings,
    messageTargets: AiMessageTarget[],
    instruction: string,
  ) =>
    invoke<string>("ai_draft", {
      input: { ...aiInput(settings, messageTargets), instruction },
    }),
  aiAvailable: (settings: AiSettings) =>
    invoke<boolean>("ai_available", { input: aiInput(settings, []) }),
  saveAiApiKey: (apiKey: string) => invoke<void>("set_ai_api_key", { apiKey }),
  translationModels: () =>
    invoke<TranslationModelStatus[]>("translation_models"),
  translationModelFiles: (source: string) =>
    invoke<TranslationModelFiles>("translation_model_files", { source }),
  detectTranslationLanguage: (text: string) =>
    invoke<TranslationLanguageDetection>("translation_detect_language", {
      text,
    }),
  installTranslationModel: (
    source: string,
    onProgress?: (progress: TranslationDownloadProgress) => void,
  ) => {
    const channel = new Channel<TranslationDownloadProgress>();
    if (onProgress) channel.onmessage = onProgress;
    return invoke<TranslationModelFiles>("translation_install_model", {
      source,
      onProgress: channel,
    });
  },
  cancelTranslationModelInstall: (source: string) =>
    invoke<void>("translation_cancel_install", { source }),
  removeTranslationModel: (source: string) =>
    invoke<void>("translation_remove_model", { source }),
};

export type SenderCleanupTarget = {
  readonly accountId: string;
  readonly senderName: string | null;
  readonly senderAddress: string;
};

export type UnsubscribeResult =
  | { kind: "completed"; cleanupTarget: SenderCleanupTarget | null }
  | { kind: "opened_web"; cleanupTarget: SenderCleanupTarget | null };

export type TrashMessagesFromSenderResult = {
  matched: number;
  moved: number;
  failed: number;
};

const aiInput = (
  settings: AiSettings,
  messageTargets: AiMessageTarget[],
): AiInput => ({
  provider: settings.provider,
  baseUrl: settings.baseUrl || null,
  model: settings.model,
  apiKey: settings.apiKey || null,
  executable: settings.executable || null,
  modelPath: settings.modelPath || null,
  messageIds: messageTargets.map((target) => target.messageId),
  messageTargets,
  instruction: null,
});

const demoAccount: Account = {
  id: "3b77bc44-c1f6-4f89-8dc5-176b4361c571",
  email: "hello@dakia.dev",
  account_name: "hello@dakia.dev",
  display_name: "Alex",
  provider_id: "fastmail",
  auth: { type: "password", username: "hello@dakia.dev" },
  imap_host: "imap.fastmail.com",
  imap_port: 993,
  imap_security: "tls",
  smtp_host: "smtp.fastmail.com",
  smtp_port: 465,
  smtp_security: "tls",
  archive_mailbox: "Archive",
  spam_mailbox: "Spam",
  enabled: true,
};
const demoMailSafetyOrigin = mailSafetyOrigin(
  import.meta.env.VITE_DAKIA_MAIL_SAFETY_ORIGIN,
);

const demoMessages: MailSummary[] = [
  {
    id: "demo-1",
    account_id: demoAccount.id,
    mailbox: "INBOX",
    uid: 3,
    thread_id: "demo-thread-1",
    message_id: "<demo-reply-1@dakia.dev>",
    in_reply_to: "<demo-root-1@dakia.dev>",
    reference_ids: "<demo-root-1@dakia.dev>",
    subject: "A calmer way to plan the release",
    from_name: "Mara Vale",
    from_address: "mara@northline.example.test",
    to_addresses: demoAccount.email,
    received_at: new Date().toISOString(),
    snippet:
      "I reviewed the final milestones and moved the signing work forward…",
    body_text:
      "Hi Alex,\n\nI reviewed the final milestones and moved the signing work forward. The macOS notarization credentials are ready for Friday, and the Linux smoke test is booked for Monday morning.\n\nCould you confirm who owns the final provider matrix?\n\nMara",
    body_html: `<div style="font-family: -apple-system, sans-serif; color: #24302c"><img alt="Northline release plan" width="640" height="120" style="border-radius: 14px" src="data:image/svg+xml;charset=utf-8,%3Csvg xmlns='http://www.w3.org/2000/svg' width='640' height='120' viewBox='0 0 640 120'%3E%3Crect width='640' height='120' rx='14' fill='%23e8f0ec'/%3E%3Ccircle cx='58' cy='60' r='25' fill='%233f7467'/%3E%3Cpath d='M46 61l8 8 18-21' fill='none' stroke='white' stroke-width='6' stroke-linecap='round' stroke-linejoin='round'/%3E%3Ctext x='100' y='54' font-family='Arial,sans-serif' font-size='14' fill='%23576a63'%3ENORTHLINE%3C/text%3E%3Ctext x='100' y='80' font-family='Arial,sans-serif' font-size='24' font-weight='700' fill='%2324302c'%3ERelease plan ready%3C/text%3E%3C/svg%3E"><p>Hi Alex,</p><p>I reviewed the final milestones and moved the signing work forward. The <strong>macOS notarization credentials</strong> are ready for Friday, and the Linux smoke test is booked for Monday morning.</p><p>Could you confirm who owns the final provider matrix?</p><p><a href="https://example.com/release-plan">Open release checklist</a></p><p>Mara</p></div>`,
    is_read: false,
    is_flagged: true,
    has_attachments: false,
    category: "people",
    classification_confidence: 0.93,
    classification_source: "model",
  },
  {
    id: "demo-4",
    account_id: demoAccount.id,
    mailbox: "INBOX",
    uid: 4,
    thread_id: "demo-thread-1",
    message_id: "<demo-root-1@dakia.dev>",
    subject: "A calmer way to plan the release",
    from_name: "Mara Vale",
    from_address: "mara@northline.example.test",
    to_addresses: demoAccount.email,
    received_at: new Date(Date.now() - 3600000).toISOString(),
    snippet: "I finished reviewing the release milestones…",
    body_text:
      "Hi Alex,\n\nI finished reviewing the release milestones. The signing work is the only item I still want to clarify before Friday.\n\nMara",
    is_read: true,
    is_flagged: false,
    has_attachments: false,
  },
  {
    id: "demo-2",
    account_id: demoAccount.id,
    mailbox: "INBOX",
    uid: 2,
    thread_id: "demo-thread-2",
    subject: "Field notes · July",
    from_name: "Nora Kask",
    from_address: "nora@lume.example.test",
    to_addresses: demoAccount.email,
    received_at: new Date(Date.now() - 86400000).toISOString(),
    snippet: "The new navigation tested well with keyboard-only participants…",
    body_text:
      "The new navigation tested well with keyboard-only participants. I added the complete findings to the shared folder.",
    is_read: true,
    is_flagged: false,
    has_attachments: true,
    category: "newsletters",
    classification_confidence: 0.9,
    classification_source: "model",
  },
  {
    id: "demo-3",
    account_id: demoAccount.id,
    mailbox: "INBOX",
    uid: 1,
    thread_id: "demo-thread-3",
    subject: "Invoice 0726 and project closeout",
    from_name: "Tomas Reed",
    from_address: "tomas@reedandco.uk",
    to_addresses: demoAccount.email,
    received_at: new Date(Date.now() - 172800000).toISOString(),
    snippet: "Attached is the final invoice and a short closeout note…",
    body_text:
      "Attached is the final invoice and a short closeout note. Thank you for the thoughtful collaboration.",
    is_read: true,
    is_flagged: false,
    has_attachments: true,
    category: "transactions",
    classification_confidence: 0.91,
    classification_source: "model",
  },
  ...(demoMailSafetyOrigin
    ? [
        {
          id: "demo-mail-safety",
          account_id: demoAccount.id,
          mailbox: "INBOX",
          uid: 5,
          thread_id: "demo-thread-mail-safety",
          subject: "Remote content safety check",
          from_name: "Safety Fixture",
          from_address: "fixture@privacy.example.test",
          to_addresses: demoAccount.email,
          received_at: new Date(Date.now() - 900000).toISOString(),
          snippet: "This fictional message verifies remote-content consent.",
          body_text:
            "This fictional message verifies remote-content consent. Local safety marker.",
          body_html: hostileDemoHtml(demoMailSafetyOrigin),
          is_read: false,
          is_flagged: false,
          has_attachments: true,
          category: "notifications" as const,
          classification_confidence: 1,
          classification_source: "model" as const,
        } satisfies MailSummary,
      ]
    : []),
];

function hostileDemoHtml(origin: string) {
  return `<link rel="stylesheet" href="${origin}/remote-style.css">
<style>@import url('${origin}/remote-style.css'); @font-face { font-family: SafetyFixture; src: url('${origin}/remote-font.woff2'); } .remote-style-marker { background-image: url('${origin}/background.svg'); font-family: SafetyFixture, sans-serif; }</style>
<div class="remote-style-marker"><p>Remote-content safety fixture.</p><p class="remote-style-marker">Remote style marker</p><img alt="Remote image" src="${origin}/remote-image.svg" srcset="${origin}/srcset-image.svg 1x"><img alt="Local safety marker" src="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' width='1' height='1'%3E%3C/svg%3E"><svg width="1" height="1"><image href="${origin}/svg-external.svg" width="1" height="1" /></svg><script src="${origin}/hostile-script.js"></script><iframe src="${origin}/hostile-frame.html"></iframe><form action="${origin}/form-submit"><input name="fixture" value="blocked"></form></div>`;
}

const demoAttachments = (messageId: string): Attachment[] =>
  messageId === "demo-mail-safety"
    ? [
        {
          id: "demo-mail-safety:0",
          message_id: messageId,
          filename: "quarterly-report.pdf.command",
          mime_type: "application/x-sh",
          size_bytes: 1_024,
          is_inline: false,
          is_potentially_unsafe: true,
        },
      ]
    : messageId === "demo-2"
      ? [
          {
            id: "demo-2:0",
            message_id: messageId,
            filename: "field-notes.pdf",
            mime_type: "application/pdf",
            size_bytes: 2_850_000,
            is_inline: false,
            is_potentially_unsafe: false,
          },
        ]
      : messageId === "demo-3"
        ? [
            {
              id: "demo-3:0",
              message_id: messageId,
              filename: "invoice-0726.pdf",
              mime_type: "application/pdf",
              size_bytes: 184_000,
              is_inline: false,
              is_potentially_unsafe: false,
            },
          ]
        : [];

const demoSmartSectionIds: SmartSectionId[] = [
  "starred",
  "people",
  "transactions",
  "notifications",
  "newsletters",
  "other",
  "seen",
];

const demoApi: typeof desktopApi = {
  terminalCommandStatus: async () => "notSetUp",
  installTerminalCommand: async () => undefined,
  removeTerminalCommand: async () => undefined,
  providers: async () => [
    {
      id: "gmail",
      name: "Gmail",
      domains: ["gmail.com"],
      imap_host: "imap.gmail.com",
      imap_port: 993,
      imap_security: "tls",
      smtp_host: "smtp.gmail.com",
      smtp_port: 465,
      smtp_security: "tls",
      archive_mailbox: "[Gmail]/All Mail",
      spam_mailbox: "[Gmail]/Spam",
      app_password_help:
        "https://support.google.com/accounts/answer/185833?hl=en",
    },
    {
      id: "fastmail",
      name: "Fastmail",
      domains: ["fastmail.com"],
      imap_host: "imap.fastmail.com",
      imap_port: 993,
      imap_security: "tls",
      smtp_host: "smtp.fastmail.com",
      smtp_port: 465,
      smtp_security: "tls",
      archive_mailbox: "Archive",
      spam_mailbox: "Spam",
    },
  ],
  configureTray: async () => undefined,
  accounts: async () => [demoAccount],
  updateAccount: async (input) => {
    const value = input as Record<string, string | number>;
    Object.assign(demoAccount, {
      account_name: value.accountName as string,
      display_name: value.displayName,
      imap_host: value.imapHost,
      imap_port: value.imapPort,
      imap_security: value.imapSecurity,
      smtp_host: value.smtpHost,
      smtp_port: value.smtpPort,
      smtp_security: value.smtpSecurity,
      archive_mailbox: value.archiveMailbox,
      spam_mailbox: value.spamMailbox,
    });
    return demoAccount;
  },
  showAccountContextMenu: async () => undefined,
  showEmailAddressContextMenu: (...args) =>
    "__TAURI_INTERNALS__" in window
      ? desktopApi.showEmailAddressContextMenu(...args)
      : Promise.resolve(),
  removeAccount: async () => undefined,
  addAccount: async () => ({
    account: demoAccount,
    reusedExistingAccount: false,
  }),
  search: async (
    text,
    accountIds,
    mailbox,
    unreadOnly,
    flaggedOnly,
    _limit,
    _cursor,
    category,
    unflaggedOnly,
    readOnly,
  ) => {
    const allowedAccounts = new Set(accountIds);
    const matches = demoMessages.filter(
      (message) =>
        (!allowedAccounts.size || allowedAccounts.has(message.account_id)) &&
        (!mailbox || mailboxFamily(message.mailbox) === mailbox) &&
        (!unreadOnly || !message.is_read) &&
        (!readOnly || message.is_read) &&
        (!flaggedOnly || message.is_flagged) &&
        (!unflaggedOnly || !message.is_flagged) &&
        (!category || message.category === category) &&
        !(
          !mailbox && ["Spam", "Trash"].includes(mailboxFamily(message.mailbox))
        ) &&
        `${message.subject} ${message.body_text} ${message.from_name}`
          .toLowerCase()
          .includes((text ?? "").toLowerCase()),
    );
    const matchingKeys = new Set(
      matches.map((message) => `${message.account_id}:${message.thread_id}`),
    );
    return {
      conversations: groupMessages(
        demoMessages.filter(
          (message) =>
            matchingKeys.has(`${message.account_id}:${message.thread_id}`) &&
            (mailbox === "Spam" || mailbox === "Trash"
              ? mailboxFamily(message.mailbox) === mailbox
              : !["Spam", "Trash"].includes(mailboxFamily(message.mailbox))),
        ),
      ),
      nextCursor: null,
    };
  },
  smartInbox: async (accountIds, limit = 3) => {
    const allowedAccounts = new Set(accountIds);
    const inboxThreads = groupMessages(
      demoMessages.filter(
        (message) =>
          (!allowedAccounts.size || allowedAccounts.has(message.account_id)) &&
          mailboxFamily(message.mailbox) === "INBOX",
      ),
    );
    return {
      sections: demoSmartSectionIds.map((id) => {
        const conversations = inboxThreads
          .filter((thread) => {
            const messages = thread.sourceMessages ?? thread.messages;
            if (id === "starred") {
              return messages.some((message) => message.is_flagged);
            }
            if (id === "seen") {
              return messages.every((message) => message.is_read);
            }
            return (
              thread.latest.category === id &&
              messages.some((message) => !message.is_read) &&
              !messages.some((message) => message.is_flagged)
            );
          })
          .slice(0, limit);
        return { id, conversations, nextCursor: null };
      }),
    };
  },
  searchRemote: async (text, accountIds, mailbox, unreadOnly, flaggedOnly) => {
    const allowedAccounts = new Set(accountIds);
    return {
      messages: demoMessages.filter(
        (message) =>
          (!allowedAccounts.size || allowedAccounts.has(message.account_id)) &&
          (!mailbox || mailboxFamily(message.mailbox) === mailbox) &&
          (!unreadOnly || !message.is_read) &&
          (!flaggedOnly || message.is_flagged) &&
          `${message.subject} ${message.body_text} ${message.from_name}`
            .toLowerCase()
            .includes(text.toLowerCase()),
      ),
      limitReached: false,
    };
  },
  conversationForTarget: async (target) => {
    const exact = target.localMessageId
      ? demoMessages.find(
          (message) =>
            message.account_id === target.accountId &&
            message.id === target.localMessageId,
        )
      : undefined;
    const rfc = target.rfcMessageId
      ? demoMessages.find(
          (message) =>
            message.account_id === target.accountId &&
            message.message_id?.toLowerCase() ===
              target.rfcMessageId?.toLowerCase(),
        )
      : undefined;
    const threadId = exact?.thread_id ?? rfc?.thread_id ?? target.threadId;
    if (!threadId) return null;
    return (
      groupMessages(
        demoMessages.filter(
          (message) =>
            message.account_id === target.accountId &&
            message.thread_id === threadId,
        ),
      )[0] ?? null
    );
  },
  setCategory: async (messageId, category, _expectedUidValidity = null) => {
    const message = demoMessages.find((item) => item.id === messageId);
    if (message) {
      message.category = category as typeof message.category;
      message.classification_confidence = 1;
      message.classification_source = "user";
    }
  },
  setStarred: async (messageId, starred) => {
    const message = demoMessages.find((item) => item.id === messageId);
    if (!message) throw new Error("Message not found");
    message.is_flagged = starred;
    return message;
  },
  setRead: async (messageId, read) => {
    const message = demoMessages.find((item) => item.id === messageId);
    if (!message) throw new Error("Message not found");
    message.is_read = read;
  },
  starredCount: async (accountIds) => {
    const allowed = new Set(accountIds);
    return groupMessages(
      demoMessages.filter(
        (message) =>
          (!allowed.size || allowed.has(message.account_id)) &&
          message.is_flagged,
      ),
    ).length;
  },
  classifyPending: async () => 0,
  startRealtimeSync: async () => undefined,
  reconcileRealtimeSync: async () => undefined,
  realtimeSyncStatus: async () => [],
  listSendRecoveries: async () => ({
    items: [],
    next_cursor: null,
    total_count: 0,
  }),
  openSendRecoveryWindow: async () => {
    throw new Error("Send recovery is available only in the desktop app.");
  },
  readSendRecoverySeed: async () => {
    throw new Error("Send recovery is available only in the desktop app.");
  },
  acknowledgeSendRecoverySeed: async () => undefined,
  mailRebuildStatus: async () => [],
  recordNotificationDelivered: async () => undefined,
  hydrateMessage: async (messageId, _expectedUidValidity = null) => {
    const message = demoMessages.find((item) => item.id === messageId);
    if (!message) throw new Error("Message not found");
    return message;
  },
  sync: async (_accountId, onProgress) => {
    onProgress?.({ phase: "connecting", completed: 0, total: null });
    onProgress?.({
      phase: "threading",
      completed: demoMessages.length,
      total: demoMessages.length,
    });
    onProgress?.({
      phase: "downloading",
      completed: demoMessages.length,
      total: demoMessages.length,
    });
    onProgress?.({
      phase: "complete",
      completed: demoMessages.length,
      total: demoMessages.length,
    });
    return { syncedCount: demoMessages.length, newMessages: [] };
  },
  content: async (messageId, _expectedUidValidity = null) => {
    const message =
      demoMessages.find((item) => item.id === messageId) ?? demoMessages[0];
    return {
      body_text: message.body_text,
      body_html: message.body_html,
      unsubscribe_kind: message.unsubscribe_kind,
      attachments: demoAttachments(messageId),
    };
  },
  attachments: async (messageId, _expectedUidValidity = null) =>
    demoAttachments(messageId),
  saveAttachment: async (
    _messageId,
    attachmentId,
    _expectedUidValidity = null,
  ) =>
    `~/Downloads/${attachmentId.includes("demo-3") ? "invoice-0726.pdf" : "field-notes.pdf"}`,
  saveAllAttachments: async (messageId, _expectedUidValidity = null) =>
    messageId === "demo-2"
      ? ["~/Downloads/field-notes.pdf"]
      : ["~/Downloads/invoice-0726.pdf"],
  exportMessage: async (messageId, _expectedUidValidity = null) =>
    `~/Downloads/${messageId}.eml`,
  forwardAttachments: async (_messageId, _expectedUidValidity = null) => [],
  readDroppedFiles: async () => {
    throw new Error("Native file drop support is unavailable in the web demo");
  },
  send: async () => "queued",
  action: async () => undefined,
  openExternal: async (url) => {
    window.open(url, "_blank", "noopener,noreferrer");
  },
  unsubscribe: async (_messageId, _expectedUidValidity = null) => ({
    kind: "completed" as const,
    cleanupTarget: {
      accountId: demoAccount.id,
      senderName: "Mara Chen",
      senderAddress: "mara@example.com",
    },
  }),
  trashMessagesFromSender: async (accountId, senderAddress) => {
    const normalized = normalizeSenderAddress(senderAddress);
    const matches = demoMessages.filter(
      (message) =>
        message.account_id === accountId &&
        normalizeSenderAddress(message.from_address) === normalized,
    );
    for (const message of matches) message.mailbox = "Trash";
    return { matched: matches.length, moved: matches.length, failed: 0 };
  },
  summarize: async () =>
    "Mara confirmed that notarization credentials are ready for Friday and the Linux smoke test is scheduled for Monday. She needs confirmation of who owns the final provider matrix.",
  draft: async () =>
    "Hi Mara,\n\nThanks for moving those pieces forward. I’ll own the final provider matrix and share it before Friday.\n\nBest,\nAlex",
  aiAvailable: async () => false,
  saveAiApiKey: async () => undefined,
  translationModels: async () => [
    {
      source: "et",
      sourceName: "Estonian",
      target: "en" as const,
      downloadBytes: 21943524,
      installed: true,
    },
  ],
  translationModelFiles: async () => ({
    source: "et",
    target: "en" as const,
    modelPath: "",
    shortlistPath: "",
    vocabPaths: [],
    config: {},
  }),
  detectTranslationLanguage: async () => ({
    language: "et",
    languageName: "Estonian",
    reliable: true,
  }),
  installTranslationModel: async () => ({
    source: "et",
    target: "en" as const,
    modelPath: "",
    shortlistPath: "",
    vocabPaths: [],
    config: {},
  }),
  cancelTranslationModelInstall: async () => undefined,
  removeTranslationModel: async () => undefined,
  sendDesktopNotification: async () => undefined,
};

const isTauri =
  typeof window !== "undefined" && "__TAURI_INTERNALS__" in window;
// Native UI verification can use the same fictional mailbox as browser
// development without opening or mutating a developer's real local store.
const useNativeDemoApi =
  import.meta.env.DEV && import.meta.env.VITE_DAKIA_DEMO_API === "1";
export const api =
  (isTauri && !useNativeDemoApi) || !import.meta.env.DEV ? desktopApi : demoApi;
