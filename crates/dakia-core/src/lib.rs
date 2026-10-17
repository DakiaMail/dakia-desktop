pub mod account;
pub mod account_service;
pub mod agent_state;
pub mod ai;
pub mod classification;
pub mod drafts;
mod flowed;
pub mod mail;
pub mod mime_budget;
mod oauth;
pub mod operations;
pub mod provider;
pub mod storage;
pub mod thread_context;

pub use account::{Account, AccountAuth, AccountDraft, AccountId};
pub use account_service::{credential_secret_name, validate_account, AccountService};
pub use ai::{AiConfig, AiProvider, AiService};
pub use classification::{EmailClassificationInput, LocalEmailClassifier, ModelClassification};
pub use drafts::{DraftReconcileResult, DraftService, DraftSyncStatus, LocalDraft};
pub use mail::{
    mailbox_action_destination, normalize_sender_address, remote_mailbox, ComposeMessage,
    MailService, MailboxAction, MailboxInfo, MessageFlag, ProviderDraftAppend, RealtimeCycle,
    RealtimeMode, SenderTrashResult, SyncProgress, SyncResult, UnsubscribeOutcome,
};
pub use operations::{
    AccountOperationAcquire, AccountOperationCoordinator, AccountOperationKind,
    AccountOperationLease, AccountOperationLeaseState,
};
pub use provider::{ProviderPreset, Security};
pub use storage::{
    Attachment, AttachmentPresentation, CachedMessageContent, ConversationTarget, MailConversation,
    MailConversationPage, MailCursor, MailRebuildJob, MailSummary, ModelClassificationUpdate,
    SearchQuery, SmartInboxPage, SmartInboxQuery, SmartInboxSectionPage, Store,
};
pub use thread_context::{
    ThreadContext, ThreadContextAction, ThreadContextBundle, ThreadContextConversation,
    ThreadContextKind, ThreadContextLocator, ThreadContextRevision,
};
