import { Channel, invoke } from "@tauri-apps/api/core";
import { mailboxFamily } from "./mailActions";
import { groupMessages } from "./threads";
import type {
  Account,
  Attachment,
  AiSettings,
  ComposeAttachment,
  MailRebuildProgress,
  MailCursor,
  MailSummary,
  MessageContent,
  MailThread,
  MailThreadPage,
  MessageContentErrorKind,
  Provider,
  SyncProgress,
  SyncResult,
  RealtimeSyncStatus,
  TranslationDownloadProgress,
  TranslationLanguageDetection,
  TranslationModelFiles,
  TranslationModelStatus,
} from "./types";

const messageContentErrorKinds = new Set<MessageContentErrorKind>([
  "resource_limit",
  "malformed",
  "undecodable",
  "unsupported",
  "transient",
]);

export class MessageContentError extends Error {
  readonly retryable: boolean;

  constructor(readonly kind: MessageContentErrorKind) {
    super("Message content could not be loaded");
    this.name = "MessageContentError";
    this.retryable = kind === "transient";
  }
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
    invoke<Account>("add_account", { input: { draft, password } }),
  addOAuthAccount: (draft: Record<string, unknown>) =>
    invoke<Account>("add_oauth_account", { draft }),
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
  searchRemote: (
    text: string,
    accountIds: string[] = [],
    mailbox?: string,
    unreadOnly = false,
    flaggedOnly = false,
  ) =>
    invoke<MailSummary[]>("search_remote", {
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
  setCategory: (messageId: string, category: string) =>
    invoke<void>("set_message_category", { messageId, category }),
  setStarred: (messageId: string, starred: boolean) =>
    invoke<MailSummary>("set_message_starred", { messageId, starred }),
  setRead: (messageId: string, read: boolean) =>
    invoke<void>("set_message_read", { messageId, read }),
  starredCount: (accountIds: string[]) =>
    invoke<number>("starred_conversation_count", { accountIds }),
  classifyPending: () => invoke<number>("classify_pending"),
  startRealtimeSync: () => invoke<void>("start_realtime_sync"),
  reconcileRealtimeSync: () => invoke<void>("reconcile_realtime_sync"),
  realtimeSyncStatus: () =>
    invoke<RealtimeSyncStatus[]>("realtime_sync_status"),
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
    count: number;
    sound?: string;
  }) => invoke<void>("send_desktop_notification", { notification }),
  hydrateMessage: (messageId: string) =>
    invoke<MailSummary>("hydrate_message", { messageId }),
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
  content: async (messageId: string) => {
    try {
      return await invoke<MessageContent>("message_content", { messageId });
    } catch (error) {
      throw messageContentErrorFromUnknown(error);
    }
  },
  attachments: (messageId: string) =>
    invoke<Attachment[]>("message_attachments", { messageId }),
  saveAttachment: (messageId: string, attachmentId: string) =>
    invoke<string>("save_attachment", { messageId, attachmentId }),
  saveAllAttachments: (messageId: string) =>
    invoke<string[]>("save_all_attachments", { messageId }),
  exportMessage: (messageId: string) =>
    invoke<string>("export_message", { messageId }),
  forwardAttachments: (messageId: string) =>
    invoke<ComposeAttachment[]>("forward_attachments", { messageId }),
  readDroppedFiles: (receipt: string) =>
    invoke<ComposeAttachment[]>("read_dropped_files", { receipt }),
  send: (draft: Record<string, unknown>) =>
    invoke<string>("send_message", { draft }),
  action: (
    accountId: string,
    mailbox: string,
    uid: number,
    action: "archive" | "spam" | "not_spam" | "trash",
  ) =>
    invoke<void>("apply_mailbox_action", { accountId, mailbox, uid, action }),
  openExternal: (url: string) => invoke<void>("open_external_url", { url }),
  unsubscribe: (messageId: string) =>
    invoke<UnsubscribeResult>("unsubscribe_message", { messageId }),
  summarize: (settings: AiSettings, messageIds: string[]) =>
    invoke<string>("ai_summarize", { input: aiInput(settings, messageIds) }),
  draft: (settings: AiSettings, messageIds: string[], instruction: string) =>
    invoke<string>("ai_draft", {
      input: { ...aiInput(settings, messageIds), instruction },
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

export type UnsubscribeResult = { kind: "completed" } | { kind: "opened_web" };

const aiInput = (settings: AiSettings, messageIds: string[]) => ({
  provider: settings.provider,
  baseUrl: settings.baseUrl || null,
  model: settings.model,
  apiKey: settings.apiKey || null,
  executable: settings.executable || null,
  modelPath: settings.modelPath || null,
  messageIds,
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
];

const demoAttachments = (messageId: string): Attachment[] =>
  messageId === "demo-2"
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
      oauth: true,
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
      oauth: false,
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
  showEmailAddressContextMenu: async () => undefined,
  removeAccount: async () => undefined,
  addAccount: async () => demoAccount,
  addOAuthAccount: async () => demoAccount,
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
  searchRemote: async (text, accountIds, mailbox, unreadOnly, flaggedOnly) => {
    const allowedAccounts = new Set(accountIds);
    return demoMessages.filter(
      (message) =>
        (!allowedAccounts.size || allowedAccounts.has(message.account_id)) &&
        (!mailbox || mailboxFamily(message.mailbox) === mailbox) &&
        (!unreadOnly || !message.is_read) &&
        (!flaggedOnly || message.is_flagged) &&
        `${message.subject} ${message.body_text} ${message.from_name}`
          .toLowerCase()
          .includes(text.toLowerCase()),
    );
  },
  setCategory: async (messageId, category) => {
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
  mailRebuildStatus: async () => [],
  recordNotificationDelivered: async () => undefined,
  hydrateMessage: async (messageId) => {
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
  content: async (messageId) => {
    const message =
      demoMessages.find((item) => item.id === messageId) ?? demoMessages[0];
    return {
      body_text: message.body_text,
      body_html: message.body_html,
      unsubscribe_kind: message.unsubscribe_kind,
      attachments: demoAttachments(messageId),
    };
  },
  attachments: async (messageId) => demoAttachments(messageId),
  saveAttachment: async (_messageId, attachmentId) =>
    `~/Downloads/${attachmentId.includes("demo-3") ? "invoice-0726.pdf" : "field-notes.pdf"}`,
  saveAllAttachments: async (messageId) =>
    messageId === "demo-2"
      ? ["~/Downloads/field-notes.pdf"]
      : ["~/Downloads/invoice-0726.pdf"],
  exportMessage: async (messageId) => `~/Downloads/${messageId}.eml`,
  forwardAttachments: async () => [],
  readDroppedFiles: async () => {
    throw new Error("Native file drop support is unavailable in the web demo");
  },
  send: async () => "queued",
  action: async () => undefined,
  openExternal: async (url) => {
    window.open(url, "_blank", "noopener,noreferrer");
  },
  unsubscribe: async () => ({ kind: "completed" }),
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
export const api = isTauri || !import.meta.env.DEV ? desktopApi : demoApi;
