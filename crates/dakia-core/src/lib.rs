pub mod account;
pub mod ai;
pub mod classification;
pub mod mail;
pub mod oauth;
pub mod provider;
pub mod storage;

pub use account::{Account, AccountAuth, AccountDraft, AccountId};
pub use ai::{AiConfig, AiProvider, AiService};
pub use classification::{LocalEmailClassifier, ModelClassification};
pub use mail::{
    mailbox_action_destination, remote_mailbox, ComposeMessage, MailService, MailboxAction,
    RealtimeCycle, RealtimeMode, SyncProgress, SyncResult, UnsubscribeOutcome,
};
pub use oauth::{OAuthFlow, OAuthProviderConfig, OAuthTokens};
pub use provider::{ProviderPreset, Security};
pub use storage::{
    Attachment, AttachmentPresentation, CachedMessageContent, MailConversation,
    MailConversationPage, MailCursor, MailRebuildJob, MailSummary, SearchQuery, Store,
};
