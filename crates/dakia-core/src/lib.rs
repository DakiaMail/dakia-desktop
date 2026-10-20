pub mod account;
pub mod ai;
pub mod classification;
pub mod connection_budget;
mod flowed;
pub mod imap_search;
pub mod mail;
pub mod mail_metrics;
pub mod mime_budget;
mod oauth;
pub mod provider;
pub mod storage;

pub use account::{Account, AccountAuth, AccountDraft, AccountId};
pub use ai::{AiConfig, AiProvider, AiService};
pub use classification::{EmailClassificationInput, LocalEmailClassifier, ModelClassification};
pub use mail::{
    mailbox_action_destination, mailbox_action_outcome_is_uncertain, normalize_sender_address,
    remote_mailbox, ComposeMessage, MailService, MailboxAction, MoveDestination,
    PreparedOutgoingMessage, RealtimeCycle, RealtimeMode, SendOutcome, SenderMessageDiscovery,
    SenderTrashResult, SentCopyOutcome, SentCopyPresence, SentCopyStatus, SupportedMailbox,
    SyncProgress, SyncResult, UnsubscribeOutcome,
};
pub use provider::{ProviderPreset, Security};
pub use storage::{
    Attachment, AttachmentPresentation, CachedMessageContent, MailConversation,
    MailConversationPage, MailCursor, MailRebuildJob, MailSummary, ModelClassificationUpdate,
    SearchQuery, SmartInboxPage, SmartInboxQuery, SmartInboxSectionPage, Store,
};
