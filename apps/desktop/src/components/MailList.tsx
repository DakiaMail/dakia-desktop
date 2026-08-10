import {
  ActionIcon,
  Checkbox,
  Group,
  Loader,
  Menu,
  Progress,
  TextInput,
  Tooltip,
} from "@mantine/core";
import {
  IconArchive,
  IconArrowBackUp,
  IconCategory,
  IconEdit,
  IconMail,
  IconMailForward,
  IconPaperclip,
  IconRefresh,
  IconSearch,
  IconShieldCheck,
  IconShieldX,
  IconSparkles,
  IconStar,
  IconTrash,
} from "@tabler/icons-react";
import { format, isThisYear, isToday } from "date-fns";
import { AI_FEATURES_VISIBLE } from "../features";
import {
  useCallback,
  useEffect,
  useState,
  type CSSProperties,
  type UIEvent,
} from "react";
import { useTranslation } from "react-i18next";
import type { PendingMailActions } from "../mailActions";
import { cleanEmailSnippet } from "../snippet";
import type {
  MailCategory,
  MailListView,
  MailSummary,
  SmartSection,
  SmartSectionId,
  MailThread,
  SyncStatus,
} from "../types";
import { concreteThreadMessages } from "../threads";
import { EmptyState } from "./EmptyState";

type Props = {
  threads: MailThread[];
  activeThreadId?: string;
  selected: Set<string>;
  query: string;
  loading: boolean;
  loadingMore: boolean;
  hasMore: boolean;
  remoteSearchUnavailable: boolean;
  syncStatus?: SyncStatus;
  classifying: boolean;
  lastSyncAt?: string;
  aiConnected: boolean;
  mailboxTitle: string;
  view: MailListView;
  onViewChange: (view: MailListView) => void;
  onCategorize: (message: MailSummary, category: MailCategory) => void;
  onToggleStar: (message: MailSummary, starred: boolean) => void;
  smartInbox: boolean;
  smartSections?: SmartSection[];
  exitingThreadIds?: Set<string>;
  onQuery: (value: string) => void;
  onOpen: (thread: MailThread) => void;
  onDoubleOpen: (thread: MailThread) => void;
  onSelect: (ids: string[], checked: boolean) => void;
  onSync: () => void;
  onCompose: () => void;
  onArchive: () => void;
  onSpam: () => void;
  onReplyThread: (thread: MailThread) => void;
  onForwardThread: (thread: MailThread) => void;
  onActionThread: (
    thread: MailThread,
    action: "archive" | "spam" | "not_spam" | "trash",
  ) => void;
  onToggleReadThread: (thread: MailThread, read: boolean) => void;
  onToggleStarThread: (thread: MailThread, flagged: boolean) => void;
  onSummarize: () => void;
  onLoadMore: () => void;
  onLoadMoreSmart?: (id: SmartSectionId) => void;
  pendingActions: PendingMailActions;
  actionsDisabled: boolean;
  searchRef: React.RefObject<HTMLInputElement | null>;
};

export function MailList({
  threads,
  activeThreadId,
  selected,
  query,
  loading,
  loadingMore,
  hasMore,
  remoteSearchUnavailable,
  syncStatus,
  classifying,
  lastSyncAt,
  aiConnected,
  mailboxTitle,
  view,
  onViewChange,
  onCategorize,
  onToggleStar,
  smartInbox,
  smartSections = [],
  exitingThreadIds = new Set(),
  onQuery,
  onOpen,
  onDoubleOpen,
  onSelect,
  onSync,
  onCompose,
  onArchive,
  onSpam,
  onReplyThread,
  onForwardThread,
  onActionThread,
  onToggleReadThread,
  onToggleStarThread,
  onSummarize,
  onLoadMore,
  onLoadMoreSmart,
  pendingActions,
  actionsDisabled,
  searchRef,
}: Props) {
  const { t } = useTranslation();
  const displayedThreads =
    view === "smart" && smartInbox
      ? smartSections.flatMap((section) => section.threads)
      : threads;
  const selectedCount = displayedThreads.filter((thread) =>
    selected.has(thread.id),
  ).length;
  const loadNearEnd = useCallback(
    (event: UIEvent<HTMLDivElement>) => {
      const element = event.currentTarget;
      if (
        element.scrollHeight - element.scrollTop - element.clientHeight <
        800
      ) {
        if (view === "smart" && smartInbox) {
          const seen = smartSections.find((section) => section.id === "seen");
          if (seen?.nextCursor && !seen.loadingMore) onLoadMoreSmart?.("seen");
        } else if (hasMore && !loadingMore) {
          onLoadMore();
        }
      }
    },
    [
      hasMore,
      loadingMore,
      onLoadMore,
      onLoadMoreSmart,
      smartInbox,
      smartSections,
      view,
    ],
  );
  return (
    <section className="mail-list-panel" aria-label={mailboxTitle}>
      <header className="list-header">
        <div className="list-title-row">
          <div className="list-title-copy">
            <div className="list-title" title={mailboxTitle}>
              {mailboxTitle}
            </div>
          </div>
          <div className="list-header-actions">
            <div
              className="mail-view-toggle"
              role="group"
              aria-label={t("inbox.view")}
            >
              {(["smart", "list"] as const).map((value) => (
                <button
                  key={value}
                  className="mail-view-button"
                  data-active={view === value}
                  aria-pressed={view === value}
                  onClick={() => onViewChange(value)}
                >
                  {t(`inbox.view${value === "smart" ? "Smart" : "List"}`)}
                </button>
              ))}
            </div>
            <button
              className="header-compose"
              onClick={onCompose}
              title={`${t("actions.compose")} (⌘N)`}
            >
              <IconEdit size={15} stroke={1.8} />
              <span>{t("actions.compose")}</span>
            </button>
            <Tooltip label={syncTooltip(lastSyncAt, t)}>
              <ActionIcon
                variant="subtle"
                color="gray"
                onClick={onSync}
                loading={Boolean(syncStatus)}
                disabled={Boolean(syncStatus)}
                aria-label={t("actions.sync")}
              >
                <IconRefresh size={18} />
              </ActionIcon>
            </Tooltip>
          </div>
        </div>
        <TextInput
          ref={searchRef}
          value={query}
          onChange={(event) => onQuery(event.currentTarget.value)}
          leftSection={<IconSearch size={16} />}
          placeholder={t("search.placeholder")}
          aria-label={t("actions.search")}
        />
        {query.trim() && remoteSearchUnavailable ? (
          <div className="search-scope-notice" role="status">
            {t("search.localOnly")}
          </div>
        ) : null}
        {syncStatus ? <SyncIndicator status={syncStatus} compact /> : null}
        {classifying ? (
          <div className="classification-indicator" role="status">
            <Loader size={12} />
            <span>{t("inbox.classifying")}</span>
          </div>
        ) : null}
        {selected.size > 0 ? (
          <div className="selection-bar">
            <span>{t("inbox.selected", { count: selectedCount })}</span>
            <Group gap={2}>
              {AI_FEATURES_VISIBLE && aiConnected ? (
                <Tooltip label={t("actions.summarize")}>
                  <ActionIcon
                    variant="transparent"
                    color="gray"
                    onClick={onSummarize}
                    aria-label={t("actions.summarize")}
                  >
                    <IconSparkles size={17} />
                  </ActionIcon>
                </Tooltip>
              ) : null}
              <Tooltip label={t("actions.archive")}>
                <ActionIcon
                  variant="transparent"
                  color="gray"
                  onClick={onArchive}
                  disabled={actionsDisabled}
                  aria-label={t("actions.archive")}
                >
                  <IconArchive size={17} />
                </ActionIcon>
              </Tooltip>
              <Tooltip label={t("actions.spam")}>
                <ActionIcon
                  variant="transparent"
                  color="gray"
                  onClick={onSpam}
                  disabled={actionsDisabled}
                  aria-label={t("actions.spam")}
                >
                  <IconTrash size={17} />
                </ActionIcon>
              </Tooltip>
            </Group>
          </div>
        ) : null}
      </header>
      <div className="mail-scroll" onScroll={loadNearEnd}>
        {syncStatus && displayedThreads.length === 0 ? (
          <div className="sync-empty" role="status" aria-live="polite">
            <div className="sync-empty-symbol">
              <IconRefresh size={25} />
            </div>
            <div className="sync-empty-title">{t("inbox.firstSyncTitle")}</div>
            <SyncIndicator status={syncStatus} />
          </div>
        ) : null}
        {loading && !syncStatus && displayedThreads.length === 0 ? (
          <div className="empty-state">
            <Loader size="sm" />
          </div>
        ) : null}
        {!loading && !syncStatus && displayedThreads.length === 0 ? (
          <EmptyState
            title={t("search.emptyTitle")}
            body={t("search.emptyBody")}
          />
        ) : null}
        {view === "smart" && smartInbox ? (
          <SmartThreadSections
            sections={smartSections}
            exitingThreadIds={exitingThreadIds}
            activeThreadId={activeThreadId}
            selected={selected}
            pendingActions={pendingActions}
            onOpen={onOpen}
            onDoubleOpen={onDoubleOpen}
            onSelect={onSelect}
            onCategorize={onCategorize}
            onToggleStar={onToggleStar}
            onReplyThread={onReplyThread}
            onForwardThread={onForwardThread}
            onActionThread={onActionThread}
            onToggleReadThread={onToggleReadThread}
            onToggleStarThread={onToggleStarThread}
            onLoadMore={onLoadMoreSmart}
          />
        ) : (
          <ThreadRows
            threads={threads}
            exitingThreadIds={exitingThreadIds}
            activeThreadId={activeThreadId}
            selected={selected}
            pendingActions={pendingActions}
            onOpen={onOpen}
            onDoubleOpen={onDoubleOpen}
            onSelect={onSelect}
            onCategorize={onCategorize}
            onToggleStar={onToggleStar}
            onReplyThread={onReplyThread}
            onForwardThread={onForwardThread}
            onActionThread={onActionThread}
            onToggleReadThread={onToggleReadThread}
            onToggleStarThread={onToggleStarThread}
          />
        )}
        {loadingMore ? (
          <div className="mail-page-loader" role="status">
            <Loader size="xs" />
          </div>
        ) : null}
      </div>
    </section>
  );
}

const categories: Array<{ id: MailCategory; label: string }> = [
  { id: "people", label: "inbox.categoryPeople" },
  { id: "transactions", label: "inbox.categoryTransactions" },
  { id: "notifications", label: "inbox.categoryNotifications" },
  { id: "newsletters", label: "inbox.categoryNewsletters" },
  { id: "other", label: "inbox.categoryOther" },
];

type RowsProps = Pick<
  Props,
  | "activeThreadId"
  | "selected"
  | "pendingActions"
  | "onOpen"
  | "onDoubleOpen"
  | "onSelect"
  | "onCategorize"
  | "onToggleStar"
  | "onReplyThread"
  | "onForwardThread"
  | "onActionThread"
  | "onToggleReadThread"
  | "onToggleStarThread"
  | "exitingThreadIds"
> & { threads: MailThread[] };

function SmartThreadSections(
  props: Omit<RowsProps, "threads"> & {
    sections: SmartSection[];
    onLoadMore?: (id: SmartSectionId) => void;
  },
) {
  const { t } = useTranslation();
  const labels: Record<SmartSectionId, string> = {
    starred: "inbox.starred",
    people: "inbox.categoryPeople",
    transactions: "inbox.categoryTransactions",
    notifications: "inbox.categoryNotifications",
    newsletters: "inbox.categoryNewsletters",
    other: "inbox.categoryOther",
    seen: "inbox.seen",
  };
  return props.sections.map(({ id, threads, nextCursor, loadingMore }) => {
    if (!threads.length) return null;
    return (
      <section className="smart-section" key={id} aria-label={t(labels[id])}>
        <div className="smart-section-header">
          <span>{t(labels[id])}</span>
        </div>
        <ThreadRows {...props} threads={threads} />
        {nextCursor ? (
          <button
            className="smart-section-more"
            disabled={loadingMore}
            onClick={() => props.onLoadMore?.(id)}
          >
            {loadingMore ? t("inbox.loading") : t("inbox.showMore")}
          </button>
        ) : null}
      </section>
    );
  });
}

function ThreadRows({
  threads,
  activeThreadId,
  selected,
  pendingActions,
  onOpen,
  onDoubleOpen,
  onSelect,
  onCategorize,
  onReplyThread,
  onForwardThread,
  onActionThread,
  onToggleReadThread,
  onToggleStarThread,
  exitingThreadIds = new Set(),
}: RowsProps) {
  const { t } = useTranslation();
  const [context, setContext] = useState<{
    thread: MailThread;
    x: number;
    y: number;
  }>();
  const contextThread = context?.thread;
  const isSpam =
    contextThread &&
    concreteThreadMessages(contextThread).some(
      (message) => message.mailbox.split("::", 1)[0] === "Spam",
    );
  const isStarred =
    contextThread &&
    concreteThreadMessages(contextThread).some((message) => message.is_flagged);
  const isUnread = contextThread?.unread ?? false;
  useEffect(() => {
    if (!context) return;
    const closeOutside = (event: MouseEvent) => {
      if (!(event.target as Element).closest("[data-menu-dropdown]")) {
        setContext(undefined);
      }
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key === "Escape") setContext(undefined);
    };
    document.addEventListener("mousedown", closeOutside);
    document.addEventListener("keydown", closeOnEscape);
    return () => {
      document.removeEventListener("mousedown", closeOutside);
      document.removeEventListener("keydown", closeOnEscape);
    };
  }, [context]);
  const close = () => setContext(undefined);
  return (
    <>
      <Menu opened={Boolean(context)} withinPortal shadow="md" width={220}>
        <Menu.Target>
          <span
            aria-hidden
            style={{
              position: "fixed",
              left: context?.x ?? 0,
              top: context?.y ?? 0,
              width: 1,
              height: 1,
              pointerEvents: "none",
            }}
          />
        </Menu.Target>
        <Menu.Dropdown>
          <Menu.Item
            leftSection={<IconArrowBackUp size={16} />}
            onClick={() => {
              if (contextThread) onReplyThread(contextThread);
              close();
            }}
          >
            {t("actions.reply")}
          </Menu.Item>
          <Menu.Item
            leftSection={<IconMailForward size={16} />}
            onClick={() => {
              if (contextThread) onForwardThread(contextThread);
              close();
            }}
          >
            {t("actions.forward")}
          </Menu.Item>
          <Menu.Item
            leftSection={<IconStar size={16} />}
            onClick={() => {
              if (contextThread) onToggleStarThread(contextThread, !isStarred);
              close();
            }}
          >
            {t(isStarred ? "actions.contextUnstar" : "actions.contextStar")}
          </Menu.Item>
          <Menu.Item
            leftSection={<IconMail size={16} />}
            onClick={() => {
              if (contextThread) onToggleReadThread(contextThread, isUnread);
              close();
            }}
          >
            {t(isUnread ? "actions.markRead" : "actions.markUnread")}
          </Menu.Item>
          <Menu.Item
            leftSection={
              isSpam ? <IconShieldCheck size={16} /> : <IconShieldX size={16} />
            }
            onClick={() => {
              if (contextThread)
                onActionThread(contextThread, isSpam ? "not_spam" : "spam");
              close();
            }}
          >
            {t(isSpam ? "actions.notSpam" : "actions.spam")}
          </Menu.Item>
          <Menu.Item
            leftSection={<IconTrash size={16} />}
            onClick={() => {
              if (contextThread) onActionThread(contextThread, "trash");
              close();
            }}
          >
            {t("actions.delete")}
          </Menu.Item>
          <Menu.Item
            leftSection={<IconArchive size={16} />}
            disabled={
              !contextThread?.messages.some(
                (message) => message.mailbox.split("::", 1)[0] === "INBOX",
              )
            }
            onClick={() => {
              if (contextThread) onActionThread(contextThread, "archive");
              close();
            }}
          >
            {t("actions.archive")}
          </Menu.Item>
          <Menu.Sub>
            <Menu.Sub.Target>
              <Menu.Sub.Item leftSection={<IconCategory size={16} />}>
                {t("actions.categorize")}
              </Menu.Sub.Item>
            </Menu.Sub.Target>
            <Menu.Sub.Dropdown>
              {categories.map(({ id, label }) => (
                <Menu.Item
                  key={id}
                  onClick={() => {
                    if (contextThread) onCategorize(contextThread.latest, id);
                    close();
                  }}
                >
                  {t(label)}
                </Menu.Item>
              ))}
            </Menu.Sub.Dropdown>
          </Menu.Sub>
        </Menu.Dropdown>
      </Menu>
      {threads.map((thread) => {
        const message = thread.latest;
        const sourceMessages = concreteThreadMessages(thread);
        const pending = sourceMessages
          .map((item) => pendingActions[item.id])
          .find(Boolean);
        return (
          <button
            key={thread.id}
            className="mail-item"
            data-active={activeThreadId === thread.id}
            data-unread={thread.unread}
            data-smart-exiting={exitingThreadIds.has(thread.id)}
            data-action-phase={pending?.phase}
            data-action-kind={pending?.action}
            style={
              pending
                ? ({ "--action-delay": `${pending.delay}ms` } as CSSProperties)
                : undefined
            }
            disabled={pending?.phase === "exiting"}
            onClick={() => onOpen(thread)}
            onDoubleClick={() => onDoubleOpen(thread)}
            onContextMenu={(event) => {
              event.preventDefault();
              setContext({ thread, x: event.clientX, y: event.clientY });
            }}
          >
            <span
              className="mail-check"
              onClick={(event) => event.stopPropagation()}
              onDoubleClick={(event) => event.stopPropagation()}
            >
              <Checkbox
                size="xs"
                checked={selected.has(thread.id)}
                onChange={(event) =>
                  onSelect([thread.id], event.currentTarget.checked)
                }
                aria-label={t("actions.select")}
              />
            </span>
            <span className="mail-copy">
              <span className="mail-topline">
                <span className="mail-sender">
                  {thread.participants.join(", ")}
                  {thread.messages.length > 1 ? (
                    <span
                      className="thread-count"
                      aria-label={t("inbox.threadMessages", {
                        count: thread.messages.length,
                      })}
                    >
                      {thread.messages.length}
                    </span>
                  ) : null}
                </span>
                <span className="mail-date">
                  {dateLabel(message.received_at)}
                </span>
              </span>
              <span className="mail-subject">
                {message.subject || t("inbox.noSubject")}
              </span>
              <span className="mail-snippet">
                {cleanEmailSnippet(message.snippet)}
              </span>
            </span>
            <span className="mail-flags">
              <span
                className="mail-star"
                role="button"
                tabIndex={0}
                aria-label={t(
                  sourceMessages.some((item) => item.is_flagged)
                    ? "actions.unstar"
                    : "actions.star",
                )}
                data-active={sourceMessages.some((item) => item.is_flagged)}
                onDoubleClick={(event) => event.stopPropagation()}
                onClick={(event) => {
                  event.stopPropagation();
                  onToggleStarThread(
                    thread,
                    !sourceMessages.some((item) => item.is_flagged),
                  );
                }}
                onKeyDown={(event) => {
                  if (event.key === "Enter" || event.key === " ") {
                    event.preventDefault();
                    event.stopPropagation();
                    onToggleStarThread(
                      thread,
                      !sourceMessages.some((item) => item.is_flagged),
                    );
                  }
                }}
              >
                <IconStar
                  size={15}
                  fill={
                    sourceMessages.some((item) => item.is_flagged)
                      ? "currentColor"
                      : "none"
                  }
                />
              </span>
              {thread.hasAttachments ? (
                <IconPaperclip size={14} aria-label={t("inbox.attachment")} />
              ) : null}
            </span>
          </button>
        );
      })}
    </>
  );
}

function SyncIndicator({
  status,
  compact = false,
}: {
  status: SyncStatus;
  compact?: boolean;
}) {
  const { t } = useTranslation();
  const determinate = status.total !== null && status.total > 0;
  const value = determinate
    ? Math.min(100, (status.completed / status.total!) * 100)
    : 100;
  const detail = syncDetail(status, t);
  return (
    <div
      className={compact ? "sync-indicator is-compact" : "sync-indicator"}
      role={compact ? "status" : undefined}
      aria-live={compact ? "polite" : undefined}
    >
      <div className="sync-indicator-copy">
        <span>{detail}</span>
        {status.accountCount > 1 ? (
          <span className="sync-account-count">
            {t("inbox.syncAccountCount", {
              current: status.accountIndex,
              total: status.accountCount,
            })}
          </span>
        ) : null}
      </div>
      <Progress
        value={value}
        animated={!determinate || status.phase !== "complete"}
        size="xs"
        radius="xl"
        aria-label={detail}
      />
    </div>
  );
}

function syncDetail(
  status: SyncStatus,
  t: ReturnType<typeof useTranslation>["t"],
) {
  switch (status.phase) {
    case "connecting":
      return t("inbox.syncConnecting", { account: status.accountEmail });
    case "authenticating":
      return t("inbox.syncAuthenticating", { account: status.accountEmail });
    case "finding":
      return t("inbox.syncFinding");
    case "threading":
      return t("inbox.syncThreading", {
        completed: status.completed,
        total: status.total ?? status.completed,
      });
    case "downloading":
      return status.total
        ? t("inbox.syncDownloading", {
            completed: status.completed,
            total: status.total,
          })
        : t("inbox.syncPreparing");
    case "saving":
      return t("inbox.syncSaving");
    case "complete":
      return t("inbox.syncFinishing");
  }
}

function dateLabel(value: string) {
  const date = new Date(value);
  return isToday(date)
    ? format(date, "HH:mm")
    : isThisYear(date)
      ? format(date, "MMM d")
      : format(date, "MMM d, yy");
}

function syncTooltip(
  value: string | undefined,
  t: ReturnType<typeof useTranslation>["t"],
) {
  if (!value) return t("inbox.neverSynced");
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return t("inbox.neverSynced");
  return t("inbox.lastSynced", {
    date: isToday(date)
      ? format(date, "'today at' HH:mm")
      : format(date, "EEE d MMM 'at' HH:mm"),
  });
}
