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
/** A locally learned recipient that can be suggested while composing. */
export type ContactedPersonSuggestion = {
  address: string;
  display_name?: string | null;
  formatted_address?: string | null;
  account_id?: string | null;
  account_send_count?: number;
  account_last_contacted_at?: string | null;
  last_contacted_at?: string | null;
  hidden?: boolean;
};

export type ContactedPeopleSettings = {
  enabled: boolean;
};

export type ContactedPeopleChanged = ContactedPeopleSettings & {
  cleared: boolean;
};

/** A provider-discovered mailbox that can be used as an exact search scope. */
export type SearchMailbox = {
  /** Stable local catalogue path, never a provider command path. */
  localPath: string;
  selectable: boolean;
};

/** Input and response for the Rust send-parser validation boundary. */
export type ComposeRecipientsInput = {
  to: string[];
  cc: string[];
  bcc: string[];
};

export type ComposeRecipientFieldValidation = {
  valid: boolean;
  /** Original entries rejected by the exact Rust send parser. */
  invalid: string[];
};

export type ComposeRecipientValidation = {
  to: ComposeRecipientFieldValidation;
  cc: ComposeRecipientFieldValidation;
  bcc: ComposeRecipientFieldValidation;
};

/** The explicit execution mode for the versioned search contract. */
export type SearchExecutionMode = "local" | "hybrid";

export type SearchScopeV2 = {
  mailbox?: string | null;
  include_spam_trash?: boolean;
};

/** A raw query is deliberately kept opaque to the desktop client. */
export type SearchRequestV2 = {
  /** Client-reserved identity, allowing cancellation before start resolves. */
  client_request_id?: string;
  raw_query: string;
  account_ids: string[];
  scope: SearchScopeV2;
  execution_mode: SearchExecutionMode;
  page_size: number;
  continuation?: string | null;
};

export type SearchCoverageState =
  | "local_catalogue"
  | "local_body_index"
  | "provider_searched"
  | "provider_partial"
  | "offline"
  | "authentication_failed"
  | "unsupported"
  | "cancelled"
  | "mailbox_changed";

export type SearchCoverage = {
  account_id: string;
  mailbox?: string | null;
  state: SearchCoverageState;
  detail?: string | null;
};

/** Incremental coverage published by a submitted native search session. */
export type SearchProgressUpdate = {
  sessionId: string;
  revision: number;
  coverage: SearchCoverage[];
};

export type SearchMatchEvidence = {
  primary_message_id?: string | null;
  matched_message_ids: string[];
  match_count: number;
  excerpt?: string | null;
};

export type SearchPageV2 = {
  conversations: MailThread[];
  match_evidence?: Record<string, SearchMatchEvidence>;
  coverage: SearchCoverage[];
  continuation?: string | null;
  session_id: string;
  revision: number;
};

export type SearchErrorV2 = {
  position?: number | null;
  category: "parse" | "unsupported" | "provider" | "transient";
  unsupported_operator?: string | null;
  message: string;
};

/** Desktop-local shortcut to a raw query and the accounts it was saved for. */
export type SavedSearch = {
  id: string;
  name: string;
  raw_query: string;
  account_ids: string[];
  local_only: boolean;
  created_at: string;
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
