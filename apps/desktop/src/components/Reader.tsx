import {
  ActionIcon,
  Avatar,
  Button,
  Loader,
  Menu,
  Tooltip,
} from "@mantine/core";
import {
  IconArchive,
  IconArrowBackUp,
  IconCopy,
  IconChevronDown,
  IconDownload,
  IconDots,
  IconFile,
  IconFiles,
  IconMailForward,
  IconLanguage,
  IconMail,
  IconPaperclip,
  IconSparkles,
  IconStar,
  IconTrash,
  IconUsers,
  IconAlertTriangle,
  IconShieldCheck,
  IconShieldX,
} from "@tabler/icons-react";
import { AI_FEATURES_VISIBLE } from "../features";
import { format } from "date-fns";
import {
  useCallback,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
} from "react";
import { useTranslation } from "react-i18next";
import { api } from "../api";
import { confirmNativeAction } from "../nativeFeedback";
import {
  detectTranslationLanguage,
  translateOffline,
} from "../offlineTranslation";
import { translateConversation } from "../translationWorkflow";
import type { Attachment, MailSummary, MessageContent } from "../types";
import { formatAddress, messageRecipients } from "../recipients";
import { splitQuotedText } from "../quotedHistory";
import { EmptyState } from "./EmptyState";
import { HtmlMessage } from "./HtmlMessage";

type Props = {
  message?: MailSummary;
  messages?: MailSummary[];
  accountEmail?: string;
  aiResult?: string;
  aiLoading: boolean;
  aiConnected: boolean;
  actionsDisabled: boolean;
  onArchive: () => void;
  onSpam: () => void;
  onTrash: () => void;
  onReply: (message: MailSummary) => void;
  onReplyAll: (message: MailSummary) => void;
  onForward: (message: MailSummary) => void;
  onToggleRead: (read: boolean) => void;
  onSummarize: () => void;
  onCopyAi: () => void;
  unsubscribeLoading: boolean;
  onUnsubscribe: (message: MailSummary) => void;
  onToggleStar: (message: MailSummary, starred: boolean) => void;
};

export function Reader({
  message,
  messages,
  accountEmail,
  aiResult,
  aiLoading,
  aiConnected,
  actionsDisabled,
  onArchive,
  onSpam,
  onTrash,
  onReply,
  onReplyAll,
  onForward,
  onToggleRead,
  onSummarize,
  onCopyAi,
  unsubscribeLoading,
  onUnsubscribe,
  onToggleStar,
}: Props) {
  const { t } = useTranslation();
  const [translation, setTranslation] = useState<{
    source: string;
    sourceName: string;
    subject: string;
    contents: Record<string, MessageContent>;
  }>();
  const [translationLoading, setTranslationLoading] = useState(false);
  const [translationProgress, setTranslationProgress] = useState<{
    source: string;
    downloadedBytes: number;
    totalBytes: number;
  }>();
  const [translationError, setTranslationError] = useState<string>();
  const translationRequest = useRef(0);
  const readerScrollRef = useRef<HTMLElement>(null);
  const subjectSentinelRef = useRef<HTMLDivElement>(null);
  const [subjectCompact, setSubjectCompact] = useState(false);
  const threadMessages = useMemo(
    () => (messages?.length ? messages : message ? [message] : []),
    [message, messages],
  );
  const latestMessage = threadMessages.at(-1);
  const threadKey = latestMessage
    ? `${latestMessage.account_id}:${latestMessage.thread_id || latestMessage.id}`
    : message
      ? `${message.account_id}:${message.thread_id || message.id}`
      : undefined;
  const [expandedMessage, setExpandedMessage] = useState<{
    threadKey?: string;
    id?: string;
  }>(() => ({ threadKey, id: latestMessage?.id }));
  // Render the final chronological message immediately when a new conversation
  // arrives. Keeping the chosen ID in state means later mailbox updates do not
  // interrupt someone reading an earlier message.
  const expandedMessageId =
    expandedMessage?.threadKey === threadKey
      ? expandedMessage?.id
      : latestMessage?.id;
  useEffect(() => {
    setExpandedMessage((current) =>
      current.threadKey === threadKey
        ? current
        : { threadKey, id: latestMessage?.id },
    );
  }, [threadKey]);
  useEffect(() => {
    setSubjectCompact(false);
    const root = readerScrollRef.current;
    const sentinel = subjectSentinelRef.current;
    if (!root || !sentinel || typeof IntersectionObserver === "undefined")
      return;
    const observer = new IntersectionObserver(
      ([entry]) => setSubjectCompact(!entry.isIntersecting),
      { root, threshold: 0 },
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
  }, [threadKey]);
  const contentRequests = useMemo(
    () => new Map<string, Promise<MessageContent>>(),
    [threadKey],
  );
  const loadContent = useCallback(
    (messageId: string) => {
      const existing = contentRequests.get(messageId);
      if (existing) return existing;
      const request = api.content(messageId).catch((error) => {
        contentRequests.delete(messageId);
        throw error;
      });
      contentRequests.set(messageId, request);
      return request;
    },
    [contentRequests],
  );

  useEffect(() => {
    translationRequest.current += 1;
    setTranslation(undefined);
    setTranslationLoading(false);
    setTranslationError(undefined);
    setTranslationProgress(undefined);
  }, [message?.thread_id, message?.id]);

  const toggleExpandedMessage = useCallback(
    (messageId: string) => {
      setExpandedMessage({
        threadKey,
        id: expandedMessageId === messageId ? undefined : messageId,
      });
    },
    [expandedMessageId, threadKey],
  );

  if (!message)
    return (
      <main className="reader">
        <EmptyState
          title={t("reader.emptyTitle")}
          body={t("reader.emptyBody")}
        />
      </main>
    );
  const isSpam = message.mailbox.split("::", 1)[0] === "Spam";
  const threadUnread = threadMessages.some((item) => !item.is_read);
  const cancelTranslationDownload = async () => {
    if (!translationProgress) return;
    await api.cancelTranslationModelInstall(translationProgress.source);
  };
  const translateThread = async () => {
    if (translation) {
      setTranslation(undefined);
      setTranslationError(undefined);
      return;
    }
    const requestId = ++translationRequest.current;
    setTranslationLoading(true);
    setTranslationError(undefined);
    setTranslationProgress(undefined);
    try {
      const result = await translateConversation(
        message.subject,
        threadMessages,
        {
          loadContent,
          detectLanguage: detectTranslationLanguage,
          listModels: api.translationModels,
          approveDownload: (model) => {
            if (requestId !== translationRequest.current) {
              return Promise.resolve(false);
            }
            return confirmNativeAction(
              t("translation.downloadTitle", {
                language: model.sourceName,
              }),
              t("translation.downloadBody", {
                language: model.sourceName,
                size: formatBytes(model.downloadBytes),
              }),
              t("translation.downloadAction"),
            );
          },
          installModel: api.installTranslationModel,
          translate: translateOffline,
        },
        (progress) => {
          if (requestId === translationRequest.current) {
            setTranslationProgress(progress);
          }
        },
      );
      if (requestId !== translationRequest.current) return;
      if (result.kind === "already-english") {
        setTranslationError(t("translation.alreadyEnglish"));
        return;
      }
      if (result.kind === "unsupported") {
        setTranslationError(
          t("translation.unsupported", {
            language: result.detection.languageName,
          }),
        );
        return;
      }
      if (result.kind === "cancelled") return;
      setTranslation({
        source: result.detection.language,
        sourceName: result.detection.languageName,
        subject: result.subject,
        contents: result.contents,
      });
    } catch (error) {
      if (requestId !== translationRequest.current) return;
      console.error("Offline conversation translation failed", error);
      setTranslationError(t("translation.failed"));
    } finally {
      if (requestId === translationRequest.current) {
        setTranslationLoading(false);
        setTranslationProgress(undefined);
      }
    }
  };

  return (
    <main className="reader" id="main-content">
      <div className="reader-toolbar">
        <Tooltip label={t("actions.archive")}>
          <ActionIcon
            variant="subtle"
            color="gray"
            onClick={onArchive}
            disabled={actionsDisabled}
            aria-label={t("actions.archive")}
          >
            <IconArchive size={19} />
          </ActionIcon>
        </Tooltip>
        <Tooltip label={t(isSpam ? "actions.notSpam" : "actions.spam")}>
          <ActionIcon
            variant="subtle"
            color="gray"
            onClick={onSpam}
            disabled={actionsDisabled}
            aria-label={t(isSpam ? "actions.notSpam" : "actions.spam")}
          >
            {isSpam ? <IconShieldCheck size={18} /> : <IconShieldX size={18} />}
          </ActionIcon>
        </Tooltip>
        <Tooltip
          label={t(threadUnread ? "actions.markRead" : "actions.markUnread")}
        >
          <ActionIcon
            variant="subtle"
            color="gray"
            onClick={() => onToggleRead(threadUnread)}
            disabled={actionsDisabled}
            aria-label={t(
              threadUnread ? "actions.markRead" : "actions.markUnread",
            )}
          >
            <IconMail size={18} />
          </ActionIcon>
        </Tooltip>
        <Tooltip label={t("actions.delete")}>
          <ActionIcon
            variant="subtle"
            color="gray"
            onClick={onTrash}
            disabled={actionsDisabled}
            aria-label={t("actions.delete")}
          >
            <IconTrash size={18} />
          </ActionIcon>
        </Tooltip>
        <div className="reader-toolbar-spacer" />
        <Tooltip
          label={t(message.is_flagged ? "actions.unstar" : "actions.star")}
        >
          <ActionIcon
            variant="subtle"
            color={message.is_flagged ? "yellow" : "gray"}
            onClick={() => onToggleStar(message, !message.is_flagged)}
            aria-label={t(
              message.is_flagged ? "actions.unstar" : "actions.star",
            )}
          >
            <IconStar
              size={18}
              fill={message.is_flagged ? "currentColor" : "none"}
            />
          </ActionIcon>
        </Tooltip>
        {AI_FEATURES_VISIBLE && aiConnected ? (
          <Button
            variant="subtle"
            color="gray"
            size="xs"
            leftSection={<IconSparkles size={16} />}
            onClick={onSummarize}
          >
            {t("actions.summarize")}
          </Button>
        ) : null}
        <Button
          variant={translation ? "light" : "subtle"}
          color={translation ? "ember" : "gray"}
          size="xs"
          leftSection={
            translationLoading ? (
              <Loader size={14} />
            ) : (
              <IconLanguage size={16} />
            )
          }
          onClick={() => void translateThread()}
          disabled={translationLoading}
        >
          {translation ? t("translation.showOriginal") : t("actions.translate")}
        </Button>
      </div>
      <article
        className="reader-content"
        key={message.thread_id || message.id}
        ref={readerScrollRef}
      >
        <div className="reader-subject-sentinel" ref={subjectSentinelRef} />
        <div className="reader-subject-surface">
          <h1
            className="reader-subject"
            data-compact={subjectCompact || undefined}
            title={
              subjectCompact
                ? translation?.subject ||
                  message.subject ||
                  t("inbox.noSubject")
                : undefined
            }
          >
            {translation?.subject || message.subject || t("inbox.noSubject")}
          </h1>
        </div>
        {aiLoading ? (
          <div className="ai-result">
            <Loader size="xs" />{" "}
          </div>
        ) : null}
        {AI_FEATURES_VISIBLE && aiResult ? (
          <aside className="ai-result">
            <div className="ai-result-title">
              <IconSparkles size={18} />
              {t("reader.summary")}
              <span style={{ flex: 1 }} />
              <ActionIcon
                variant="subtle"
                color="gray"
                size="sm"
                onClick={onCopyAi}
                aria-label={t("actions.copy")}
              >
                <IconCopy size={15} />
              </ActionIcon>
            </div>
            <div className="message-body">{aiResult}</div>
          </aside>
        ) : null}
        {translationLoading || translationError || translation ? (
          <div
            className={`translation-status${translationError ? " translation-status-error" : ""}`}
            role="status"
          >
            <IconLanguage size={16} />
            <span>
              {translation
                ? t("translation.translatedFrom", {
                    language: translation.sourceName,
                  })
                : translationProgress
                  ? t("translation.downloading", {
                      downloaded: formatBytes(
                        translationProgress.downloadedBytes,
                      ),
                      total: formatBytes(translationProgress.totalBytes),
                    })
                  : translationError || t("translation.translating")}
            </span>
            {translationProgress ? (
              <Button
                size="compact-xs"
                variant="subtle"
                color="gray"
                onClick={() => void cancelTranslationDownload()}
              >
                {t("actions.cancel")}
              </Button>
            ) : null}
          </div>
        ) : null}
        {threadMessages.map((threadMessage) => (
          <ThreadMessage
            key={threadMessage.id}
            message={threadMessage}
            isSent={
              Boolean(accountEmail) &&
              threadMessage.from_address.toLowerCase() ===
                accountEmail?.toLowerCase()
            }
            isLatest={threadMessage.id === latestMessage?.id}
            isExpanded={threadMessage.id === expandedMessageId}
            onToggleExpanded={() => toggleExpandedMessage(threadMessage.id)}
            actionsDisabled={actionsDisabled}
            onArchive={onArchive}
            onSpam={onSpam}
            onTrash={onTrash}
            onReply={() => onReply(threadMessage)}
            onReplyAll={() => onReplyAll(threadMessage)}
            onForward={() => onForward(threadMessage)}
            threadUnread={threadUnread}
            onToggleRead={onToggleRead}
            unsubscribeLoading={unsubscribeLoading}
            onUnsubscribe={onUnsubscribe}
            translatedContent={translation?.contents[threadMessage.id]}
            loadContent={loadContent}
          />
        ))}
      </article>
    </main>
  );
}

function ThreadMessage({
  message,
  isSent,
  isLatest,
  isExpanded,
  onToggleExpanded,
  actionsDisabled,
  onArchive,
  onSpam,
  onTrash,
  onReply,
  onReplyAll,
  onForward,
  threadUnread,
  onToggleRead,
  unsubscribeLoading,
  onUnsubscribe,
  translatedContent,
  loadContent,
}: {
  message: MailSummary;
  isSent: boolean;
  isLatest: boolean;
  isExpanded: boolean;
  onToggleExpanded: () => void;
  actionsDisabled: boolean;
  onArchive: () => void;
  onSpam: () => void;
  onTrash: () => void;
  onReply: () => void;
  onReplyAll: () => void;
  onForward: () => void;
  threadUnread: boolean;
  onToggleRead: (read: boolean) => void;
  unsubscribeLoading: boolean;
  onUnsubscribe: (message: MailSummary) => void;
  translatedContent?: MessageContent;
  loadContent: (messageId: string) => Promise<MessageContent>;
}) {
  const { t } = useTranslation();
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [attachmentsAuthoritative, setAttachmentsAuthoritative] =
    useState(false);
  const [content, setContent] = useState<MessageContent>();
  const [loadingContent, setLoadingContent] = useState(false);
  const [contentError, setContentError] = useState(false);
  const [contentAttempt, setContentAttempt] = useState(0);
  const [loadingAttachments, setLoadingAttachments] = useState(false);
  const [saving, setSaving] = useState<string>();
  const [saveStatus, setSaveStatus] = useState<string>();
  const [exporting, setExporting] = useState(false);
  const [exportStatus, setExportStatus] = useState<string>();
  const exportInFlight = useRef(false);
  const [recipientsExpanded, setRecipientsExpanded] = useState(false);
  const summaryDescriptionId = useId();
  const displayContent = translatedContent ?? content;
  const recipients = useMemo(() => messageRecipients(message), [message]);
  const downloadableAttachments = useMemo(
    () => attachments.filter(isDownloadableAttachment),
    [attachments],
  );
  const hasDownloadableAttachments = attachmentsAuthoritative
    ? downloadableAttachments.length > 0
    : message.has_attachments;

  useEffect(() => {
    setAttachments([]);
    setAttachmentsAuthoritative(false);
  }, [message.id]);

  useEffect(() => {
    if (!isExpanded) return;
    let current = true;
    setContentError(false);
    setLoadingContent(true);
    setLoadingAttachments(true);
    void loadContent(message.id)
      .then((next) => {
        if (current) {
          setContent(next);
          setAttachments(next.attachments);
          setAttachmentsAuthoritative(true);
        }
      })
      .catch(() => current && setContentError(true))
      .finally(() => {
        if (current) {
          setLoadingContent(false);
          setLoadingAttachments(false);
        }
      });
    return () => {
      current = false;
    };
  }, [contentAttempt, isExpanded, loadContent, message.id]);

  const saveOne = async (attachment: Attachment) => {
    if (saving) return;
    setSaving(attachment.id);
    try {
      await api.saveAttachment(message.id, attachment.id);
      setSaveStatus(t("attachments.saved", { count: 1 }));
    } catch {
      setSaveStatus(t("attachments.saveError"));
    } finally {
      setSaving(undefined);
    }
  };
  const saveAll = async () => {
    if (saving || downloadableAttachments.length < 2) return;
    setSaving("all");
    try {
      const saved = await api.saveAllAttachments(message.id);
      setSaveStatus(t("attachments.saved", { count: saved.length }));
    } catch {
      setSaveStatus(t("attachments.saveError"));
    } finally {
      setSaving(undefined);
    }
  };
  const exportMessage = async () => {
    if (exportInFlight.current) return;
    exportInFlight.current = true;
    setExporting(true);
    setExportStatus(undefined);
    try {
      const path = await api.exportMessage(message.id);
      setExportStatus(t("reader.exportSuccess", { path }));
    } catch {
      setExportStatus(t("reader.exportError"));
    } finally {
      exportInFlight.current = false;
      setExporting(false);
    }
  };

  if (!isExpanded) {
    return (
      <section
        className="thread-message thread-message-collapsed"
        aria-label={t("reader.messageFrom", {
          sender: message.from_name || message.from_address,
        })}
      >
        <button
          type="button"
          className="thread-message-summary"
          aria-expanded="false"
          aria-label={t("reader.expandMessage", {
            sender: message.from_name || message.from_address,
          })}
          aria-describedby={summaryDescriptionId}
          onClick={onToggleExpanded}
        >
          <Avatar
            className="sender-avatar thread-message-summary-avatar"
            color="ember"
            radius="md"
            aria-hidden="true"
          >
            {(message.from_name || message.from_address)[0]?.toUpperCase()}
          </Avatar>
          <span className="thread-message-summary-meta">
            <span className="thread-message-summary-sender">
              {message.from_name || message.from_address}
              {isSent ? (
                <span className="sent-by-you">{t("reader.sentByYou")}</span>
              ) : null}
            </span>
            <span className="thread-message-summary-snippet">
              {message.snippet}
            </span>
            <span className="reader-visually-hidden" id={summaryDescriptionId}>
              {message.snippet}.{" "}
              {format(new Date(message.received_at), "EEEE d MMM 'at' HH:mm")}
              {hasDownloadableAttachments ? `. ${t("inbox.attachment")}` : ""}
            </span>
          </span>
          {hasDownloadableAttachments ? (
            <span
              className="thread-message-summary-attachment"
              role="img"
              aria-label={t("inbox.attachment")}
            >
              <IconPaperclip size={16} aria-hidden="true" />
            </span>
          ) : null}
          <time className="reader-date">
            {format(new Date(message.received_at), "EEEE d MMM 'at' HH:mm")}
          </time>
        </button>
      </section>
    );
  }

  return (
    <section
      className="thread-message thread-message-expanded"
      aria-label={t("reader.messageFrom", {
        sender: message.from_name || message.from_address,
      })}
    >
      <div
        className="sender-card sender-card-collapsible"
        data-sent={isSent}
        onClick={(event) => {
          if (
            event.target instanceof Element &&
            event.target.closest("button, a, [role='menuitem']")
          )
            return;
          onToggleExpanded();
        }}
      >
        <Avatar className="sender-avatar" color="ember" radius="md">
          {(message.from_name || message.from_address)[0]?.toUpperCase()}
        </Avatar>
        <div className="sender-meta">
          <div className="sender-name">
            {message.from_name || message.from_address}
            {isSent ? (
              <span className="sent-by-you">{t("reader.sentByYou")}</span>
            ) : null}
          </div>
          <button
            type="button"
            className="recipient-summary"
            aria-expanded={recipientsExpanded}
            aria-label={t(
              recipientsExpanded
                ? "reader.hideRecipientDetails"
                : "reader.showRecipientDetails",
            )}
            onClick={() => setRecipientsExpanded((value) => !value)}
          >
            <IconChevronDown
              size={14}
              className="recipient-summary-chevron"
              aria-hidden="true"
            />
            <span>
              {message.from_address} ·{" "}
              {t("reader.to", { recipient: message.to_addresses })}
            </span>
          </button>
          {recipientsExpanded ? (
            <dl className="recipient-details">
              <RecipientRow
                label={t("composer.from")}
                values={recipients.from}
              />
              <RecipientRow label={t("composer.to")} values={recipients.to} />
              <RecipientRow label={t("composer.cc")} values={recipients.cc} />
              <RecipientRow label={t("composer.bcc")} values={recipients.bcc} />
              <RecipientRow
                label={t("reader.replyTo")}
                values={recipients.replyTo}
              />
            </dl>
          ) : null}
          {isLatest &&
          (displayContent?.unsubscribe_kind ?? message.unsubscribe_kind) ? (
            <Button
              variant="subtle"
              color="gray"
              size="compact-xs"
              loading={unsubscribeLoading}
              onClick={() => onUnsubscribe(message)}
            >
              {t("actions.unsubscribe")}
            </Button>
          ) : null}
        </div>
        <time className="reader-date">
          {format(new Date(message.received_at), "EEEE d MMM 'at' HH:mm")}
        </time>
        <Tooltip label={t("actions.reply")}>
          <ActionIcon
            className="reader-header-action"
            variant="subtle"
            color="gray"
            onClick={onReply}
            aria-label={t("reader.quickReply")}
          >
            <IconArrowBackUp size={19} stroke={1.7} />
          </ActionIcon>
        </Tooltip>
        <Menu position="bottom-end" shadow="md" width={180}>
          <Menu.Target>
            <Tooltip label={t("actions.more")}>
              <ActionIcon
                className="reader-header-action"
                variant="subtle"
                color="gray"
                aria-label={t("actions.more")}
              >
                <IconDots size={19} stroke={1.9} />
              </ActionIcon>
            </Tooltip>
          </Menu.Target>
          <Menu.Dropdown>
            <Menu.Item
              leftSection={<IconUsers size={16} />}
              onClick={onReplyAll}
            >
              {t("actions.replyAll")}
            </Menu.Item>
            <Menu.Item
              leftSection={<IconMailForward size={16} />}
              onClick={onForward}
            >
              {t("actions.forward")}
            </Menu.Item>
            <Menu.Item
              leftSection={
                exporting ? <Loader size={16} /> : <IconDownload size={16} />
              }
              onClick={() => void exportMessage()}
              disabled={exporting}
            >
              {t("actions.exportMessage")}
            </Menu.Item>
            <Menu.Item
              leftSection={<IconArchive size={16} />}
              onClick={onArchive}
              disabled={actionsDisabled}
            >
              {t("actions.archive")}
            </Menu.Item>
            <Menu.Item
              leftSection={<IconMail size={16} />}
              onClick={() => onToggleRead(threadUnread)}
              disabled={actionsDisabled}
            >
              {t(threadUnread ? "actions.markRead" : "actions.markUnread")}
            </Menu.Item>
            <Menu.Item
              leftSection={<IconTrash size={16} />}
              onClick={onSpam}
              disabled={actionsDisabled}
            >
              {t("actions.spam")}
            </Menu.Item>
            <Menu.Item
              leftSection={<IconTrash size={16} />}
              onClick={onTrash}
              disabled={actionsDisabled}
            >
              {t("actions.delete")}
            </Menu.Item>
          </Menu.Dropdown>
        </Menu>
        <Tooltip
          label={t("reader.collapseMessage", {
            sender: message.from_name || message.from_address,
          })}
        >
          <ActionIcon
            className="reader-header-action"
            variant="subtle"
            color="gray"
            aria-expanded="true"
            aria-label={t("reader.collapseMessage", {
              sender: message.from_name || message.from_address,
            })}
            onClick={onToggleExpanded}
          >
            <IconChevronDown size={19} stroke={1.9} />
          </ActionIcon>
        </Tooltip>
      </div>
      {exportStatus ? (
        <p className="attachment-status" role="status">
          {exportStatus}
        </p>
      ) : null}
      {loadingContent ? (
        <div className="message-body" role="status">
          <Loader size="xs" />{" "}
          {message.content_state && message.content_state !== "complete"
            ? t("reader.bodyLoading")
            : t("reader.loadingContent")}
        </div>
      ) : contentError ? (
        <div className="message-body">
          <p>{t("reader.loadContentError")}</p>
          <Button
            size="xs"
            variant="light"
            onClick={() => setContentAttempt((value) => value + 1)}
          >
            {t("actions.retry")}
          </Button>
        </div>
      ) : displayContent?.body_html ? (
        <HtmlMessage
          html={displayContent.body_html}
          title={message.subject}
          showHistoryLabel={t("reader.showHistory")}
          hideHistoryLabel={t("reader.hideHistory")}
        />
      ) : (
        <PlainTextMessage text={displayContent?.body_text ?? ""} />
      )}
      <div className="reader-reply-actions" aria-label={t("actions.reply")}>
        <Button
          variant="default"
          leftSection={<IconArrowBackUp size={17} />}
          onClick={onReply}
        >
          {t("actions.reply")}
        </Button>
        <Button
          variant="default"
          leftSection={<IconUsers size={17} />}
          onClick={onReplyAll}
        >
          {t("actions.replyAll")}
        </Button>
        <Button
          variant="default"
          leftSection={<IconMailForward size={17} />}
          onClick={onForward}
        >
          {t("actions.forward")}
        </Button>
      </div>
      {!loadingAttachments && downloadableAttachments.length ? (
        <section
          className="attachment-panel"
          aria-label={t("attachments.title")}
        >
          <div className="attachment-panel-heading">
            <div>
              <span className="attachment-panel-label">
                <IconPaperclip size={16} /> {t("attachments.title")}
              </span>
              <span className="attachment-panel-hint">
                {t("attachments.downloadHint")}
              </span>
            </div>
            {downloadableAttachments.length > 1 ? (
              <Button
                variant="light"
                size="xs"
                leftSection={
                  saving === "all" ? (
                    <Loader size={13} />
                  ) : (
                    <IconFiles size={15} />
                  )
                }
                onClick={() => void saveAll()}
                disabled={Boolean(saving)}
              >
                {t("attachments.saveAll")}
              </Button>
            ) : null}
          </div>
          {downloadableAttachments.map((attachment) => (
            <div className="attachment-row" key={attachment.id}>
              <span className="attachment-icon" aria-hidden="true">
                <IconFile size={18} />
              </span>
              <span className="attachment-details">
                <strong>{attachment.filename}</strong>
                <small>
                  {formatBytes(attachment.size_bytes)} · {attachment.mime_type}
                  {attachment.is_inline ? ` · ${t("attachments.inline")}` : ""}
                </small>
                {attachment.is_potentially_unsafe ? (
                  <small className="attachment-warning">
                    <IconAlertTriangle size={12} /> {t("attachments.unsafe")}
                  </small>
                ) : null}
              </span>
              <Tooltip
                label={t("attachments.save", {
                  filename: attachment.filename,
                })}
              >
                <ActionIcon
                  variant="subtle"
                  color="gray"
                  onClick={() => void saveOne(attachment)}
                  disabled={Boolean(saving)}
                  aria-label={t("attachments.save", {
                    filename: attachment.filename,
                  })}
                >
                  {saving === attachment.id ? (
                    <Loader size={15} />
                  ) : (
                    <IconDownload size={18} />
                  )}
                </ActionIcon>
              </Tooltip>
            </div>
          ))}
          {saveStatus ? (
            <p className="attachment-status">{saveStatus}</p>
          ) : null}
        </section>
      ) : null}
    </section>
  );
}

function RecipientRow({
  label,
  values,
}: {
  label: string;
  values: ReturnType<typeof messageRecipients>["to"];
}) {
  if (!values.length) return null;
  return (
    <div className="recipient-detail-row">
      <dt>{label}</dt>
      <dd>{values.map(formatAddress).join(", ")}</dd>
    </div>
  );
}

function PlainTextMessage({ text }: { text: string }) {
  const { t } = useTranslation();
  const split = useMemo(() => splitQuotedText(text), [text]);
  return (
    <div className="message-body">
      {split.visible}
      {split.history ? (
        <details className="quoted-history">
          <summary>
            <span className="history-show">{t("reader.showHistory")}</span>
            <span className="history-hide">{t("reader.hideHistory")}</span>
          </summary>
          <div className="quoted-history-content">{split.history}</div>
        </details>
      ) : null}
    </div>
  );
}

function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(bytes >= 10 * 1024 * 1024 ? 0 : 1)} MB`;
}

function isDownloadableAttachment(attachment: Attachment) {
  return attachment.presentation !== "embedded";
}
