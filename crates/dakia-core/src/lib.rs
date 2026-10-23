pub mod account;
pub mod ai;
pub mod classification;
mod flowed;
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing;
pub(crate) mod imap_mailbox;
pub mod mail;
pub mod mime_budget;
#[cfg(feature = "native-mail-safety-fixture")]
pub mod native_mail_safety_unsubscribe_fixture;
mod oauth;
pub mod provider;
pub mod storage;

#[cfg(test)]
mod gmail_provider_identity_proof_tests;
#[cfg(test)]
mod independent_server_tests;
#[cfg(test)]
mod large_mailbox_tls_smoke_tests;

pub use account::{Account, AccountAuth, AccountDraft, AccountId};
pub use ai::{AiConfig, AiProvider, AiService};
pub use classification::{EmailClassificationInput, LocalEmailClassifier, ModelClassification};
#[cfg(feature = "native-mail-safety-fixture")]
pub use mail::NativeMailSafetyOneClickTransport;
pub use mail::{
    mailbox_action_destination, normalize_sender_address, remote_mailbox, ComposeMessage,
    MailService, MailboxAction, RealtimeCycle, RealtimeMode, SendFailure, SendFailureKind,
    SenderTrashResult, SyncProgress, SyncResult, UnsubscribeOutcome,
};
pub use provider::{ProviderPreset, Security};
pub use storage::{
    Attachment, AttachmentPresentation, AttachmentSizeKind, CachedMessageContent, MailConversation,
    MailConversationPage, MailCursor, MailRebuildJob, MailSummary, MailboxMoveDestination,
    MailboxMovePublication, MailboxMoveReceipt, ModelClassificationUpdate, SearchQuery,
    SendAttemptClaim, SendAttemptStatus, SendRecovery, SendRecoveryClaim, SendRecoveryCursor,
    SendRecoveryPage, SendRecoveryStatus, SendRecoverySummary, SmartInboxPage, SmartInboxQuery,
    SmartInboxSectionPage, Store,
};
