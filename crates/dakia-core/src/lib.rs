pub mod account;
pub mod ai;
pub mod classification;
mod flowed;
pub mod mail;
pub mod mime_budget;
pub mod oauth;
pub mod provider;
pub mod storage;

pub use account::{Account, AccountAuth, AccountDraft, AccountId};
pub use ai::{AiConfig, AiProvider, AiService};
pub use classification::{EmailClassificationInput, LocalEmailClassifier, ModelClassification};
pub use mail::{
    mailbox_action_destination, remote_mailbox, ComposeMessage, MailService, MailboxAction,
    RealtimeCycle, RealtimeMode, SyncProgress, SyncResult, UnsubscribeOutcome,
};
pub use oauth::{OAuthFlow, OAuthProviderConfig, OAuthTokens};
pub use provider::{ProviderPreset, Security};
pub use storage::{
    Attachment, AttachmentPresentation, CachedMessageContent, MailConversation,
    MailConversationPage, MailCursor, MailRebuildJob, MailSummary, ModelClassificationUpdate,
    SearchQuery, SmartInboxPage, SmartInboxQuery, SmartInboxSectionPage, Store,
};
