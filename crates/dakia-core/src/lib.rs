pub mod account;
pub mod ai;
pub mod classification;
mod flowed;
pub mod mail;
pub mod mime_budget;
mod oauth;
pub mod provider;
pub mod search;
pub mod search_eval;
pub mod search_imap;
pub mod search_session;
pub mod search_sql;
pub mod storage;

#[cfg(test)]
static LARGE_DATASET_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) async fn large_dataset_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
    LARGE_DATASET_TEST_LOCK.lock().await
}

pub use account::{Account, AccountAuth, AccountDraft, AccountId};
pub use ai::{AiConfig, AiProvider, AiService};
pub use classification::{EmailClassificationInput, LocalEmailClassifier, ModelClassification};
pub use mail::{
    mailbox_action_destination, normalize_sender_address, remote_mailbox,
    validate_compose_recipients, ComposeMessage, ComposeRecipientFieldValidation,
    ComposeRecipientValidation, MailService, MailboxAction, ProviderMailboxSearchCoverage,
    ProviderMailboxSearchState, ProviderSearchPage, RealtimeCycle, RealtimeMode, SenderTrashResult,
    SyncProgress, SyncResult, UnsubscribeOutcome,
};
pub use provider::{ProviderPreset, Security};
pub use search::{parse_search_query, SearchExpression, SearchNode, SearchParseError, SearchTerm};
pub use search_eval::{evaluate_search, SearchableAttachment, SearchableMessage};
pub use search_imap::{compile_generic_imap, ImapCompileError, ImapSearchCandidate};
pub use search_session::{
    ProviderSearchCursor, SearchContinuationV2, SearchCoverage, SearchCoverageState,
    SearchErrorCategory, SearchErrorV2, SearchExecutionMode, SearchMatchEvidence, SearchPageV2,
    SearchRequestV2, SearchScopeV2, SearchSession, SearchSessionRegistry,
};
pub use search_sql::{
    compile_sql_candidate, SqlSearchBind, SqlSearchCandidate, SqlSearchCompileError,
    SqlSearchCompileErrorKind,
};
pub use storage::{
    Attachment, AttachmentPresentation, CachedMessageContent, MailConversation,
    MailConversationPage, MailCursor, MailRebuildJob, MailSummary, ModelClassificationUpdate,
    SearchQuery, SmartInboxPage, SmartInboxQuery, SmartInboxSectionPage, Store,
};
