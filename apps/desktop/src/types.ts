export type Security = "tls" | "start_tls";
export type Account = {
  id: string;
  email: string;
  account_name: string;
  display_name: string;
  provider_id: string;
  auth: {
    type: "password" | "oauth2";
    username: string;
    provider?: string;
    access_token_expires_at?: string | null;
  };
  imap_host: string;
  imap_port: number;
  imap_security: Security;
  smtp_host: string;
  smtp_port: number;
  smtp_security: Security;
  archive_mailbox: string;
  spam_mailbox: string;
  enabled: boolean;
};
export type AccountConnection = {
  account: Account;
  reusedExistingAccount: boolean;
};
export type Provider = {
  id: string;
  name: string;
  domains: string[];
  imap_host: string;
  imap_port: number;
  imap_security: Security;
  smtp_host: string;
  smtp_port: number;
  smtp_security: Security;
  archive_mailbox: string;
  spam_mailbox: string;
  app_password_help?: string;
};
export type MailSummary = {
  id: string;
  account_id: string;
  mailbox: string;
  uid: number;
  mailbox_uid_validity?: number | null;
  message_id?: string | null;
  in_reply_to?: string | null;
  reference_ids?: string | null;
  thread_id: string;
  subject: string;
  from_name?: string | null;
  from_address: string;
  to_addresses: string;
  cc_addresses?: string;
  bcc_addresses?: string;
  reply_to_addresses?: string;
  received_at: string;
  snippet: string;
  body_text: string;
  body_html?: string | null;
  content_state?: "headers_only" | "hydrating" | "complete" | "failed";
  unsubscribe_kind?: "one_click" | "web" | "mailto" | null;
  is_read: boolean;
  is_flagged: boolean;
  has_attachments: boolean;
  category?: MailCategory | null;
  classification_confidence?: number | null;
  classification_source?: ClassificationSource | null;
  classification_signals?: string;
};
export type MailThread = {
  id: string;
  accountId?: string;
  threadId?: string;
  messages: MailSummary[];
  /** All concrete mailbox/UID rows, including logical duplicate copies. */
  sourceMessages?: MailSummary[];
  latest: MailSummary;
  messageCount?: number;
  unread: boolean;
  hasAttachments: boolean;
  participants: string[];
};
export type MailCursor = {
  received_at: string;
  id: string;
};
export type MailThreadPage = {
  conversations: MailThread[];
  nextCursor: MailCursor | null;
};
export type ConversationTarget = {
  accountId: string;
  localMessageId?: string;
  rfcMessageId?: string;
  threadId?: string;
  mailbox?: string;
};
export type NotificationAction = {
  accountId?: string;
  messageId?: string;
  rfcMessageId?: string;
  threadId?: string;
  count: number;
};
export type SmartSectionId = "starred" | MailCategory | "seen";
export type SmartSection = {
  id: SmartSectionId;
  threads: MailThread[];
  nextCursor: MailCursor | null;
  loadingMore: boolean;
};
export type SmartInboxPage = {
  sections: Array<{
    id: SmartSectionId;
    conversations: MailThread[];
    nextCursor: MailCursor | null;
  }>;
};
export type RemoteSearchResult = {
  messages: MailSummary[];
  limitReached: boolean;
};
export type Attachment = {
  id: string;
  message_id: string;
  filename: string;
  mime_type: string;
  size_bytes: number;
  /**
   * A numeric size is only a decoded file size once the attachment has been
   * downloaded. Older payloads omit this and are treated as unknown.
   */
  size_kind?: "decoded" | "encoded" | "unknown";
  is_inline: boolean;
  /**
   * Whether this MIME part is embedded in the rendered message, available for
   * download, or intentionally both. Older cached payloads omit this field;
   * those remain displayable until refreshed by the backend.
   */
  presentation?: "embedded" | "downloadable" | "both";
  is_potentially_unsafe: boolean;
};
export type MessageContent = {
  body_text: string;
  body_html?: string | null;
  unsubscribe_kind?: "one_click" | "web" | "mailto" | null;
  attachments: Attachment[];
};
export type MessageContentErrorKind =
  "resource_limit" | "malformed" | "undecodable" | "unsupported" | "transient";
/**
 * A send failure is intentionally explicit about what happened after Dakia
 * began SMTP submission. Consumers must never turn an uncertain or
 * Sent-copy failure into an ordinary retry.
 */
export type SendFailureKind =
  "not_submitted" | "delivery_uncertain" | "sent_copy_failed";
export type ComposeAttachment = {
  filename: string;
  mime_type: string;
  content_base64: string;
  size_bytes: number;
};
export type SendRecoveryStatus =
  "not_submitted" | "pending" | "delivery_uncertain" | "sent_copy_failed";
export type SendRecoverySummary = {
  account_id: string;
  attempt_id: string;
  subject: string;
  status: SendRecoveryStatus;
  updated_at: string;
};
export type SendRecoveryPage = {
  items: SendRecoverySummary[];
  next_cursor: string | null;
  total_count: number;
};
export type SendRecoverySeed = {
  accountId: string;
  attemptId: string;
  status: SendRecoveryStatus;
  message: {
    accountId: string;
    to: string[];
    cc: string[];
    bcc: string[];
    subject: string;
    bodyText: string;
    bodyHtml: string | null;
    inReplyTo: string | null;
    references: string | null;
    attachments: Array<{
      filename: string;
      mimeType: string;
      contentBase64: string;
      sizeBytes: number;
    }>;
  };
};
export type MailCategory =
  "people" | "transactions" | "notifications" | "newsletters" | "other";
export type ClassificationSource = "model" | "override" | "user";
export type MailListView = "smart" | "list";
export type SyncProgress = {
  phase:
    | "connecting"
    | "authenticating"
    | "finding"
    | "threading"
    | "downloading"
    | "saving"
    | "complete";
  completed: number;
  total: number | null;
};
export type SyncStatus = SyncProgress & {
  accountEmail: string;
  accountIndex: number;
  accountCount: number;
};
export type MailRebuildProgress = SyncProgress & {
  accountId: string;
};
export type MailRebuildFinished = {
  accountId: string;
  outcome: "completed" | "failed" | "cancelled";
};
export type SyncResult = {
  syncedCount: number;
  newMessages: MailSummary[];
};
export type NotificationSettings = {
  enabled: boolean;
  soundEnabled: boolean;
  showPreview: boolean;
};
export type MailArrival = {
  eventId: string;
  accountId: string;
  messages: MailSummary[];
  detectedAt: string;
};
export type MailHydrated = {
  accountId: string;
  messageId: string;
};
export type RealtimeSyncStatus = {
  accountId: string;
  state: "connecting" | "idle" | "polling" | "retrying" | "paused";
  retryAt?: string | null;
  errorKind?: "connection" | "authentication" | null;
};
export type AiSettings = {
  provider: "ollama" | "openai" | "local";
  baseUrl: string;
  model: string;
  apiKey: string;
  executable: string;
  modelPath: string;
};
export type AiMessageTarget = {
  messageId: string;
  expectedUidValidity: number | null;
};
export type AiInput = {
  provider: AiSettings["provider"];
  baseUrl: string | null;
  model: string;
  apiKey: string | null;
  executable: string | null;
  modelPath: string | null;
  /** Kept for native command compatibility. */
  messageIds: string[];
  /** Immutable identities required whenever messageIds is non-empty. */
  messageTargets: AiMessageTarget[];
  instruction: string | null;
};
export type TranslationModelStatus = {
  source: string;
  sourceName: string;
  target: "en";
  downloadBytes: number;
  installed: boolean;
};
export type TranslationLanguageDetection = {
  language: string;
  languageName: string;
  reliable: boolean;
};
export type TranslationDownloadProgress = {
  source: string;
  downloadedBytes: number;
  totalBytes: number;
  fileIndex: number;
  fileCount: number;
};
export type TranslationModelFiles = {
  source: string;
  target: "en";
  modelPath: string;
  shortlistPath: string;
  vocabPaths: string[];
  config: Record<string, string>;
};
