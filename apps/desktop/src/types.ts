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
  oauth: boolean;
  app_password_help?: string;
};
export type MailSummary = {
  id: string;
  account_id: string;
  mailbox: string;
  uid: number;
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
export type Attachment = {
  id: string;
  message_id: string;
  filename: string;
  mime_type: string;
  size_bytes: number;
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
export type ComposeAttachment = {
  filename: string;
  mime_type: string;
  content_base64: string;
  size_bytes: number;
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
