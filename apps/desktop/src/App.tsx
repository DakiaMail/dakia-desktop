import { useDebouncedValue, useHotkeys } from "@mantine/hooks";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  api,
  type SenderCleanupTarget,
  type TrashMessagesFromSenderResult,
} from "./api";
import { AI_FEATURES_VISIBLE } from "./features";
import { parseEmailAddressMenuAction } from "./emailAddressMenu";
import {
  onComposeSent,
  onOutboxChanged,
  openComposeWindow,
} from "./composeWindow";
import { EmptyState } from "./components/EmptyState";
import { ActionStatus } from "./components/ActionStatus";
import { MailboxNav } from "./components/MailboxNav";
import { MailList } from "./components/MailList";
import { SendRecoveryList } from "./components/SendRecoveryList";
import { useSendRecoveries } from "./useSendRecoveries";
import { outboxItemId } from "./outboxIdentity";
import { Reader } from "./components/Reader";
import {
  UpdateBanner,
  type UpdateBannerState,
} from "./components/UpdateBanner";
import {
  conversationActionMessages,
  nextMessageAfterAction,
  removeConcreteMessage,
  removeConcreteMessageFromThreads,
  restoreThreads,
  sameMessageLocator,
  type MailAction,
  type PendingMailActions,
} from "./mailActions";
import { replyRecipients } from "./recipients";
import { confirmNativeAction, showNativeMessage } from "./nativeFeedback";
import { concreteThreadMessages, groupMessages } from "./threads";
import { formatForwardHistory, forwardSubject } from "./forward";
import { createFeedbackComposeSeed } from "./feedback";
import { formatReplyHistory } from "./replyHistory";
import {
  onNotificationAction,
  readNotificationSettings,
  requestInitialNotificationAccess,
  sendNewMailNotification,
} from "./notifications";
import {
  onReaderWindowFailed,
  onReaderWindowMutated,
  openReaderWindow,
  type ReaderWindowSeed,
} from "./readerWindow";
import {
  onAccountConnected,
  onAccountRemoved,
  onAccountUpdated,
  onMailIndexRebuilt,
  onMailRebuildFinished,
  onMailRebuildProgress,
  onMailSyncState,
  onDesktopNotificationAction,
  onNativeMenuAction,
  onNotificationSettingsChanged,
  onSettingsChanged,
  openAccountWindow,
  openSettingsWindow,
  openSettingsWindowForAccount,
} from "./nativeWindows";
import { subscribeRealtimeEvents } from "./realtimeEvents";
import { subscribeNativeGroup } from "./nativeSubscriptions";
import type {
  Account,
  AiSettings,
  MailListView,
  MailCursor,
  MailRebuildProgress,
  SmartSection,
  SmartSectionId,
  MailThread,
  MailSummary,
  NotificationSettings,
  SyncResult,
  SyncStatus,
} from "./types";
import {
  checkForUpdate,
  downloadUpdate,
  installUpdateAndRelaunch,
} from "./updater";
import {
  readAnalyticsSettings,
  reportUsageIfConsented,
  listenForAnalyticsConsent,
  setAnalyticsConsent,
  type AnalyticsSettings,
} from "./analytics";
import { AnalyticsConsentDialog } from "./components/AnalyticsConsentDialog";

const defaultAi: AiSettings = {
  provider: "ollama",
  baseUrl: "http://127.0.0.1:11434/",
  model: "qwen2.5:1.5b",
  apiKey: "",
  executable: "",
  modelPath: "",
};

const mailPageSize = 100;
const smartPageSize = 3;
const smartMorePageSize = 20;
const smartSectionIds: SmartSectionId[] = [
  "starred",
  "people",
  "transactions",
  "notifications",
  "newsletters",
  "other",
  "seen",
];
type MailViewRequest = {
  accountIds: string[];
  query: string;
  mailbox: string;
  view: MailListView;
};
type PendingActionPageObservation = {
  revision: number;
  complete: boolean;
  listThread?: MailThread;
  smartThreads?: Partial<Record<SmartSectionId, MailThread>>;
};

function usesSmartInbox(view: MailViewRequest) {
  return (
    view.view === "smart" && view.mailbox === "INBOX" && !view.query.trim()
  );
}

function sameMailView(left: MailViewRequest, right: MailViewRequest) {
  return (
    left.query === right.query &&
    left.mailbox === right.mailbox &&
    left.view === right.view &&
    left.accountIds.length === right.accountIds.length &&
    left.accountIds.every(
      (accountId, index) => accountId === right.accountIds[index],
    )
  );
}

function threadsWithinAccountScope(
  threads: MailThread[],
  accountIds: string[],
) {
  const allowed = new Set(accountIds);
  return threads.flatMap((thread) => {
    const messages = concreteThreadMessages(thread);
    const scoped = messages.filter((message) =>
      allowed.has(message.account_id),
    );
    if (!scoped.length) return [];
    if (scoped.length === messages.length) return [thread];
    return groupMessages(scoped);
  });
}

function emptySmartSections(): Record<SmartSectionId, SmartSection> {
  return smartSectionIds.reduce<Record<SmartSectionId, SmartSection>>(
    (sections, id) => {
      sections[id] = { id, threads: [], nextCursor: null, loadingMore: false };
      return sections;
    },
    {} as Record<SmartSectionId, SmartSection>,
  );
}

function discardRetainedSmartThreadsReplacedByAuthoritativeStarred(
  retainedThreads: Map<
    string,
    { sectionId: SmartSectionId; thread: MailThread }
  >,
  sections: Array<Pick<SmartSection, "id" | "threads">>,
) {
  const starredThreadIds = new Set(
    sections
      .find((section) => section.id === "starred")
      ?.threads.map((thread) => thread.id),
  );
  if (!starredThreadIds.size) return retainedThreads;

  const next = new Map(retainedThreads);
  for (const threadId of starredThreadIds) next.delete(threadId);
  return next.size === retainedThreads.size ? retainedThreads : next;
}

type ActionStatusState = {
  id: number;
  message: string;
  tone: "success" | "error";
  action?: { label: string; onAction: () => void; disabled?: boolean };
};

function normalizeSenderAddress(address: string) {
  return address
    .trim()
    .replace(/[A-Z]/g, (character) => character.toLowerCase());
}

function isSenderCleanupMessage(
  message: MailSummary,
  target: SenderCleanupTarget,
) {
  return (
    message.account_id === target.accountId &&
    normalizeSenderAddress(message.from_address) ===
      normalizeSenderAddress(target.senderAddress)
  );
}

function removeSenderCleanupMessages(
  threads: MailThread[],
  target: SenderCleanupTarget,
) {
  return threads.flatMap((thread) => {
    const messages = concreteThreadMessages(thread);
    const remaining = messages.filter(
      (message) => !isSenderCleanupMessage(message, target),
    );
    if (remaining.length === messages.length) return [thread];
    return remaining.length ? groupMessages(remaining) : [];
  });
}

function restoreSenderCleanupMessages(
  current: MailThread[],
  original: MailThread[],
  target: SenderCleanupTarget,
) {
  const messages = new Map(
    current
      .flatMap(concreteThreadMessages)
      .map((message) => [message.id, message] as const),
  );
  for (const message of original.flatMap(concreteThreadMessages)) {
    if (isSenderCleanupMessage(message, target) && !messages.has(message.id))
      messages.set(message.id, message);
  }
  return groupMessages([...messages.values()]);
}

function sameStringSet(left: Set<string>, right: Set<string>) {
  return (
    left.size === right.size && [...left].every((value) => right.has(value))
  );
}

function senderCleanupOutcome(
  t: ReturnType<typeof useTranslation>["t"],
  result: TrashMessagesFromSenderResult,
) {
  if (result.matched === 0) return t("feedback.unsubscribeCleanupNone");
  if (result.moved === result.matched && result.failed === 0) {
    return t("feedback.unsubscribeCleanupSuccess", { count: result.moved });
  }
  if (result.moved > 0) {
    return t("feedback.unsubscribeCleanupPartial", result);
  }
  return t("feedback.unsubscribeCleanupFailed");
}

export default function App() {
  const { t, i18n } = useTranslation();
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [accountsLoaded, setAccountsLoaded] = useState(false);
  const [analytics, setAnalytics] = useState<AnalyticsSettings>(
    readAnalyticsSettings,
  );
  const [threads, setThreads] = useState<MailThread[]>([]);
  const [smartSections, setSmartSections] =
    useState<Record<SmartSectionId, SmartSection>>(emptySmartSections);
  const [active, setActive] = useState<MailSummary>();
  const [activeThreadSnapshot, setActiveThreadSnapshot] =
    useState<MailThread>();
  const [selected, setSelected] = useState(new Set<string>());
  const [selectedAccountId, setSelectedAccountId] = useState<string>();
  const [mailbox, setMailbox] = useState("INBOX");
  const [mailListView, setMailListView] = useState<MailListView>(
    () =>
      (localStorage.getItem("dakia.mail-list-view") as MailListView) || "smart",
  );
  const [query, setQuery] = useState("");
  const [debouncedQuery] = useDebouncedValue(query, 220);
  const [loading, setLoading] = useState(true);
  const [loadingMore, setLoadingMore] = useState(false);
  const [hasMore, setHasMore] = useState(false);
  const [remoteSearchUnavailable, setRemoteSearchUnavailable] = useState(false);
  const [remoteSearchLimitReached, setRemoteSearchLimitReached] =
    useState(false);
  const [syncStatus, setSyncStatus] = useState<SyncStatus>();
  const [classifying, setClassifying] = useState(false);
  const [lastSyncAt, setLastSyncAt] = useState<string | undefined>(
    () => localStorage.getItem("dakia.last-sync-at") ?? undefined,
  );
  const [aiSettings, setAiSettings] = useState<AiSettings>(() =>
    readJson("dakia.ai", defaultAi),
  );
  const [notificationSettings, setNotificationSettings] =
    useState<NotificationSettings>(readNotificationSettings);
  const [aiResult, setAiResult] = useState<string>();
  const [aiLoading, setAiLoading] = useState(false);
  const [aiConnected, setAiConnected] = useState(false);
  const [unsubscribeLoading, setUnsubscribeLoading] = useState(false);
  const [senderCleanupLoading, setSenderCleanupLoading] = useState(false);
  const [permanentDeleteLoading, setPermanentDeleteLoading] = useState(false);
  const [pendingActions, setPendingActions] = useState<PendingMailActions>({});
  const [outbox, setOutbox] = useState<MailSummary[]>([]);
  const [starredCount, setStarredCount] = useState(0);
  const [exitingSmartThreadIds, setExitingSmartThreadIds] = useState(
    new Set<string>(),
  );
  const [retainedSmartThreads, setRetainedSmartThreads] = useState(
    new Map<string, { sectionId: SmartSectionId; thread: MailThread }>(),
  );
  const [actionStatus, setActionStatus] = useState<ActionStatusState>();
  const [updateState, setUpdateState] = useState<UpdateBannerState>();
  const searchRef = useRef<HTMLInputElement>(null);
  const statusId = useRef(0);
  const actionBusyRef = useRef(false);
  const senderCleanupInFlightRef = useRef(false);
  const activeThreadRef = useRef<MailThread | undefined>(undefined);
  const mailboxActionsInFlightRef = useRef(0);
  const mailboxActionThreadIdsRef = useRef(new Set<string>());
  const mailboxActionThreadCountsRef = useRef(new Map<string, number>());
  const mailboxActionPageObservationsRef = useRef(
    new Map<string, PendingActionPageObservation>(),
  );
  const mountedRef = useRef(true);
  const readMutationGenerationRef = useRef(0);
  const readMutationByMessageRef = useRef(new Map<string, number>());
  const starMutationGenerationRef = useRef(0);
  const starMutationByMessageRef = useRef(new Map<string, number>());
  const loadRequestIdRef = useRef(0);
  const smartLoadRequestIdRef = useRef(0);
  const mailViewFirstPageGenerationRef = useRef(0);
  const mailViewHasAppendedPagesRef = useRef(false);
  const starredCountRequestIdRef = useRef(0);
  const nextCursorRef = useRef<MailCursor | null>(null);
  const loadingMoreRef = useRef(false);
  const smartLoadingMoreRef = useRef(new Set<SmartSectionId>());
  const mailFirstPageReadyRef = useRef(false);
  const smartFirstPageReadyRef = useRef(false);
  const loadedViewRef = useRef<MailViewRequest | undefined>(undefined);
  const catalogueRevisionRef = useRef(0);
  const initialClassificationStartedRef = useRef(false);
  const classifyingRef = useRef(false);
  const classificationRequestedRef = useRef(false);
  const manualUpdateCheckInFlightRef = useRef(false);
  const smartExitTimersRef = useRef(new Map<string, number>());
  const accountsRef = useRef<Account[]>([]);
  const selectedAccountIdRef = useRef<string | undefined>(undefined);
  const removedAccountIdsRef = useRef(new Set<string>());
  const accountStateGenerationRef = useRef(0);
  const senderCleanupTargetRef = useRef<SenderCleanupTarget | undefined>(
    undefined,
  );
  const threadsRef = useRef<MailThread[]>([]);
  const smartSectionsRef = useRef(smartSections);
  const retainedSmartThreadsRef = useRef(retainedSmartThreads);
  const activeRef = useRef(active);
  const activeThreadSnapshotRef = useRef(activeThreadSnapshot);
  const selectedRef = useRef(selected);

  accountsRef.current = accounts;
  selectedAccountIdRef.current = selectedAccountId;
  threadsRef.current = threads;
  smartSectionsRef.current = smartSections;
  retainedSmartThreadsRef.current = retainedSmartThreads;
  activeRef.current = active;
  activeThreadSnapshotRef.current = activeThreadSnapshot;
  selectedRef.current = selected;

  useEffect(() => {
    // React StrictMode replays effects in development, so each setup must
    // reassert that this mounted instance may publish async completions.
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
      smartExitTimersRef.current.forEach((timer) => window.clearTimeout(timer));
    };
  }, []);

  const recordPendingActionListPage = (
    pageThreads: MailThread[],
    revision: number,
    complete: boolean,
  ) => {
    const pendingIds = mailboxActionThreadIdsRef.current;
    const byId = new Map(pageThreads.map((thread) => [thread.id, thread]));
    for (const id of pendingIds) {
      mailboxActionPageObservationsRef.current.set(id, {
        revision,
        complete,
        listThread: byId.get(id),
      });
    }
  };
  const recordPendingActionSmartPage = (
    sections: Array<{
      id: SmartSectionId;
      conversations: MailThread[];
    }>,
    revision: number,
    complete: boolean,
  ) => {
    const pendingIds = mailboxActionThreadIdsRef.current;
    for (const id of pendingIds) {
      const smartThreads: Partial<Record<SmartSectionId, MailThread>> = {};
      for (const section of sections) {
        const thread = section.conversations.find((item) => item.id === id);
        if (thread) smartThreads[section.id] = thread;
      }
      mailboxActionPageObservationsRef.current.set(id, {
        revision,
        complete,
        smartThreads,
      });
    }
  };

  const showStatus = useCallback(
    (
      message: string,
      tone: "success" | "error" = "success",
      action?: ActionStatusState["action"],
    ) => {
      statusId.current += 1;
      setActionStatus({ id: statusId.current, message, tone, action });
    },
    [],
  );

  useEffect(() => {
    if (!actionStatus || actionStatus.action) return;
    const timer = window.setTimeout(() => setActionStatus(undefined), 2500);
    return () => window.clearTimeout(timer);
  }, [actionStatus]);

  useEffect(() => {
    let current = true;
    void checkForUpdate()
      .then((update) => {
        if (current && update) setUpdateState({ phase: "available", update });
      })
      .catch((error) => {
        console.warn("Automatic update check failed", error);
      });
    return () => {
      current = false;
    };
  }, []);

  const runManualUpdateCheck = useCallback(async () => {
    if (manualUpdateCheckInFlightRef.current) return;
    if (updateState && updateState.phase !== "available") return;
    manualUpdateCheckInFlightRef.current = true;
    try {
      const update = await checkForUpdate(true);
      if (update) {
        setUpdateState({ phase: "available", update });
      } else {
        await showNativeMessage(
          t("updates.upToDateTitle"),
          t("updates.upToDateBody"),
        );
      }
    } catch (error) {
      await showNativeMessage(
        t("updates.checkFailedTitle"),
        String(error),
        "error",
      );
    } finally {
      manualUpdateCheckInFlightRef.current = false;
    }
  }, [t, updateState]);

  const startUpdateDownload = useCallback(async () => {
    const update = updateState?.update;
    if (!update) return;
    setUpdateState({
      phase: "downloading",
      update,
      progress: { downloadedBytes: 0 },
    });
    try {
      await downloadUpdate((progress) =>
        setUpdateState({ phase: "downloading", update, progress }),
      );
      setUpdateState({ phase: "ready", update });
    } catch (error) {
      setUpdateState({
        phase: "error",
        update,
        operation: "download",
        message: String(error),
      });
    }
  }, [updateState]);

  const installUpdate = useCallback(async () => {
    const update = updateState?.update;
    if (!update) return;
    setUpdateState({ phase: "installing", update });
    try {
      await installUpdateAndRelaunch();
    } catch (error) {
      setUpdateState({
        phase: "error",
        update,
        operation: "install",
        message: String(error),
      });
    }
  }, [updateState]);

  const changeMailListView = useCallback((view: MailListView) => {
    localStorage.setItem("dakia.mail-list-view", view);
    setMailListView(view);
  }, []);

  const markSynced = useCallback(() => {
    const value = new Date().toISOString();
    localStorage.setItem("dakia.last-sync-at", value);
    setLastSyncAt(value);
  }, []);

  useEffect(() => {
    if (!AI_FEATURES_VISIBLE) return;
    let current = true;
    void api
      .aiAvailable(aiSettings)
      .then((available) => current && setAiConnected(available))
      .catch(() => current && setAiConnected(false));
    return () => {
      current = false;
    };
  }, [aiSettings]);

  useEffect(() => {
    const initialGeneration = accountStateGenerationRef.current;
    api
      .accounts()
      .then((accountData) => {
        if (initialGeneration !== accountStateGenerationRef.current) return;
        const next = accountData.filter(
          (account) => !removedAccountIdsRef.current.has(account.id),
        );
        accountsRef.current = next;
        setAccounts(next);
        setAccountsLoaded(true);
        if (next.length === 0) {
          setLoading(false);
          void openAccountWindow();
        }
      })
      .catch((error) => {
        showError(error);
        setLoading(false);
      });
  }, []);
  useEffect(() => {
    if (!accountsLoaded || analytics.consent !== "enabled") return;
    void reportUsageIfConsented(accounts);
  }, [accounts, accountsLoaded, analytics.consent]);
  useEffect(() => {
    let dispose: () => void = () => undefined;
    void listenForAnalyticsConsent(setAnalytics)
      .then((unlisten) => (dispose = unlisten))
      .catch(() => undefined);
    return () => dispose();
  }, []);
  const activeAccounts = useMemo(
    () =>
      selectedAccountId
        ? [selectedAccountId]
        : accounts.map((account) => account.id),
    [accounts, selectedAccountId],
  );
  const recoveries = useSendRecoveries(activeAccounts);
  const refreshSendRecoveries = recoveries.refresh;
  const refreshSendRecoveriesRef = useRef(refreshSendRecoveries);
  refreshSendRecoveriesRef.current = refreshSendRecoveries;
  const visibleOutbox = useMemo(
    () =>
      outbox.filter((message) => activeAccounts.includes(message.account_id)),
    [activeAccounts, outbox],
  );
  const visibleRecoveries = useMemo(() => {
    const sendingIds = new Set(visibleOutbox.map((message) => message.id));
    return recoveries.items.filter(
      (item) => !sendingIds.has(outboxItemId(item.account_id, item.attempt_id)),
    );
  }, [recoveries.items, visibleOutbox]);
  const recoveryCount = Math.max(
    0,
    recoveries.total - (recoveries.items.length - visibleRecoveries.length),
  );
  useEffect(() => {
    if (mailbox === "Outbox") void refreshSendRecoveries();
  }, [mailbox, refreshSendRecoveries]);
  const currentViewRef = useRef<MailViewRequest>({
    accountIds: activeAccounts,
    query: debouncedQuery,
    mailbox,
    view: mailListView,
  });
  currentViewRef.current = {
    accountIds: activeAccounts,
    query: debouncedQuery,
    mailbox,
    view: mailListView,
  };
  const refreshStarredCount = useCallback(async (accountIds: string[]) => {
    const requestId = ++starredCountRequestIdRef.current;
    try {
      const count = await api.starredCount(accountIds);
      if (requestId === starredCountRequestIdRef.current) {
        setStarredCount(count);
      }
    } catch {
      // The message list remains usable if a count refresh fails offline.
    }
  }, []);
  useEffect(() => {
    void refreshStarredCount(activeAccounts);
  }, [activeAccounts, refreshStarredCount]);

  const loadMessages = useCallback(async (requestedAccountIds?: string[]) => {
    mailViewFirstPageGenerationRef.current += 1;
    mailViewHasAppendedPagesRef.current = false;
    const requestId = ++loadRequestIdRef.current;
    const smartRequestId = ++smartLoadRequestIdRef.current;
    loadingMoreRef.current = false;
    smartLoadingMoreRef.current.clear();
    mailFirstPageReadyRef.current = false;
    smartFirstPageReadyRef.current = false;
    nextCursorRef.current = null;
    setLoadingMore(false);
    setHasMore(false);
    const currentView = {
      ...currentViewRef.current,
      accountIds: requestedAccountIds ?? currentViewRef.current.accountIds,
    };
    if (
      loadedViewRef.current &&
      !sameMailView(loadedViewRef.current, currentView)
    ) {
      catalogueRevisionRef.current += 1;
      smartExitTimersRef.current.forEach((timer) => window.clearTimeout(timer));
      smartExitTimersRef.current.clear();
      retainedSmartThreadsRef.current = new Map();
      setThreads([]);
      setSmartSections(emptySmartSections());
      setRetainedSmartThreads(new Map());
      setExitingSmartThreadIds(new Set());
    }
    const accountIds = currentView.accountIds;
    const smartInbox = usesSmartInbox(currentView);
    setRemoteSearchLimitReached(false);
    if (currentView.mailbox === "Outbox") {
      if (
        requestId === loadRequestIdRef.current &&
        sameMailView(currentView, currentViewRef.current)
      ) {
        setThreads([]);
        setSmartSections(emptySmartSections());
        setLoading(false);
        setHasMore(false);
      }
      return;
    }
    if (!accountIds.length) {
      if (
        requestId === loadRequestIdRef.current &&
        sameMailView(currentView, currentViewRef.current)
      ) {
        setThreads([]);
        setLoading(false);
        setSmartSections(emptySmartSections());
        setHasMore(false);
      }
      return;
    }
    setLoading(true);
    setRemoteSearchUnavailable(false);
    try {
      if (smartInbox) {
        const page = await api.smartInbox(accountIds, smartPageSize);
        if (
          requestId === loadRequestIdRef.current &&
          smartRequestId === smartLoadRequestIdRef.current &&
          sameMailView(currentView, currentViewRef.current)
        ) {
          catalogueRevisionRef.current += 1;
          const acceptedRevision = catalogueRevisionRef.current;
          loadedViewRef.current = currentView;
          smartFirstPageReadyRef.current = true;
          const scopedSections = page.sections.map((section) => ({
            ...section,
            conversations: threadsWithinAccountScope(
              section.conversations,
              accountIds,
            ),
          }));
          recordPendingActionSmartPage(
            scopedSections,
            acceptedRevision,
            page.sections.every((section) => section.nextCursor === null),
          );
          const nextSmartSections = emptySmartSections();
          for (const section of scopedSections) {
            const visible = excludeThreads(
              section.conversations,
              mailboxActionThreadIdsRef.current,
            );
            nextSmartSections[section.id] = {
              id: section.id,
              threads: senderCleanupTargetRef.current
                ? removeSenderCleanupMessages(
                    visible,
                    senderCleanupTargetRef.current,
                  )
                : visible,
              nextCursor: section.nextCursor,
              loadingMore: false,
            };
          }
          setSmartSections(nextSmartSections);
          setRetainedSmartThreads((current) =>
            discardRetainedSmartThreadsReplacedByAuthoritativeStarred(
              current,
              smartSectionIds.map((id) => nextSmartSections[id]),
            ),
          );
          setHasMore(false);
        }
        return;
      }
      const specialUnread = currentView.mailbox === "unread";
      const specialFlagged = currentView.mailbox === "starred";
      const actualMailbox =
        ["unread", "starred"].includes(currentView.mailbox) ||
        currentView.mailbox === ""
          ? undefined
          : currentView.mailbox;
      const pageSize = mailPageSize;
      const page = await api.search(
        currentView.query,
        accountIds,
        actualMailbox,
        specialUnread,
        specialFlagged,
        pageSize,
        null,
      );
      if (
        requestId !== loadRequestIdRef.current ||
        !sameMailView(currentView, currentViewRef.current)
      )
        return;
      catalogueRevisionRef.current += 1;
      const acceptedRevision = catalogueRevisionRef.current;
      loadedViewRef.current = currentView;
      mailFirstPageReadyRef.current = true;
      nextCursorRef.current = page.nextCursor;
      const scopedThreads = threadsWithinAccountScope(
        page.conversations,
        accountIds,
      );
      recordPendingActionListPage(
        scopedThreads,
        acceptedRevision,
        !currentView.query.trim() && page.nextCursor === null,
      );
      const visible = excludeThreads(
        scopedThreads,
        mailboxActionThreadIdsRef.current,
      );
      setThreads(
        senderCleanupTargetRef.current
          ? removeSenderCleanupMessages(visible, senderCleanupTargetRef.current)
          : visible,
      );
      setHasMore(page.nextCursor !== null);
      if (currentView.query.trim()) {
        try {
          const remoteResult = await api.searchRemote(
            currentView.query,
            accountIds,
            actualMailbox,
            specialUnread,
            specialFlagged,
          );
          if (
            requestId === loadRequestIdRef.current &&
            sameMailView(currentView, currentViewRef.current)
          ) {
            setRemoteSearchLimitReached(remoteResult.limitReached);
            const remote = remoteResult.messages;
            const remoteThreads = groupMessages(
              remote.filter((message) =>
                accountIds.includes(message.account_id),
              ),
            );
            const observedThreads = new Map(
              scopedThreads.map((thread) => [thread.id, thread]),
            );
            for (const thread of remoteThreads) {
              if (!observedThreads.has(thread.id))
                observedThreads.set(thread.id, thread);
            }
            recordPendingActionListPage(
              [...observedThreads.values()],
              acceptedRevision,
              false,
            );
            setThreads((current) => {
              const merged = new Map(
                threadsWithinAccountScope(current, accountIds).map((thread) => [
                  thread.id,
                  thread,
                ]),
              );
              for (const thread of remoteThreads) {
                if (!merged.has(thread.id)) merged.set(thread.id, thread);
              }
              const visible = excludeThreads(
                [...merged.values()].sort(
                  (left, right) =>
                    new Date(right.latest.received_at).getTime() -
                    new Date(left.latest.received_at).getTime(),
                ),
                mailboxActionThreadIdsRef.current,
              );
              return senderCleanupTargetRef.current
                ? removeSenderCleanupMessages(
                    visible,
                    senderCleanupTargetRef.current,
                  )
                : visible;
            });
          }
        } catch {
          // Local catalogue results remain useful when remote search is
          // unavailable or the device goes offline mid-query.
          if (
            requestId === loadRequestIdRef.current &&
            sameMailView(currentView, currentViewRef.current)
          ) {
            setRemoteSearchUnavailable(true);
          }
        }
      }
    } catch (error) {
      if (
        requestId === loadRequestIdRef.current &&
        sameMailView(currentView, currentViewRef.current)
      ) {
        if (smartInbox) {
          setSmartSections(emptySmartSections());
          setRetainedSmartThreads(new Map());
        }
        showError(error);
      }
    } finally {
      if (
        requestId === loadRequestIdRef.current &&
        sameMailView(currentView, currentViewRef.current)
      )
        setLoading(false);
    }
  }, []);
  const reconcileCurrentFirstPage = useCallback(
    async (preservePagination: boolean) => {
      const currentView = {
        ...currentViewRef.current,
        accountIds: [...currentViewRef.current.accountIds],
      };
      if (currentView.mailbox === "Outbox" || !currentView.accountIds.length) {
        return;
      }
      const firstPageGeneration = mailViewFirstPageGenerationRef.current;
      const requestId = ++loadRequestIdRef.current;
      const smartRequestId = ++smartLoadRequestIdRef.current;
      loadingMoreRef.current = false;
      smartLoadingMoreRef.current.clear();
      setLoadingMore(false);
      setSmartSections(
        (current) =>
          Object.fromEntries(
            smartSectionIds.map((id) => [
              id,
              { ...current[id], loadingMore: false },
            ]),
          ) as Record<SmartSectionId, SmartSection>,
      );
      const smartInbox = usesSmartInbox(currentView);
      if (smartInbox) {
        const page = await api.smartInbox(
          currentView.accountIds,
          smartPageSize,
        );
        if (
          requestId !== loadRequestIdRef.current ||
          smartRequestId !== smartLoadRequestIdRef.current ||
          firstPageGeneration !== mailViewFirstPageGenerationRef.current ||
          !sameMailView(currentView, currentViewRef.current)
        ) {
          return;
        }
        const scopedSections = page.sections.map((section) => ({
          ...section,
          conversations: threadsWithinAccountScope(
            section.conversations,
            currentView.accountIds,
          ),
        }));
        const authoritativeSections = scopedSections.map((section) => {
          const visible = excludeThreads(
            section.conversations,
            mailboxActionThreadIdsRef.current,
          );
          return {
            id: section.id,
            threads: senderCleanupTargetRef.current
              ? removeSenderCleanupMessages(
                  visible,
                  senderCleanupTargetRef.current,
                )
              : visible,
          };
        });
        const authoritativeSectionByThreadId = new Map<
          string,
          SmartSectionId
        >();
        for (const section of authoritativeSections) {
          for (const thread of section.threads) {
            authoritativeSectionByThreadId.set(thread.id, section.id);
          }
        }
        setSmartSections((current) => {
          const next = Object.fromEntries(
            smartSectionIds.map((id) => [
              id,
              {
                ...current[id],
                threads: current[id].threads.filter((thread) => {
                  const authoritativeSection =
                    authoritativeSectionByThreadId.get(thread.id);
                  return !authoritativeSection || authoritativeSection === id;
                }),
              },
            ]),
          ) as Record<SmartSectionId, SmartSection>;
          for (const section of authoritativeSections) {
            // The startup reconciliation intentionally retains appended older
            // rows that are absent from a partial first page. It must still
            // remove a row that the same authoritative response has moved to
            // another Smart section, such as a read conversation externally
            // starred by another device.
            next[section.id] = {
              ...next[section.id],
              threads: mergeFirstPageThreads(
                next[section.id].threads,
                section.threads,
                readMutationByMessageRef.current,
                starMutationByMessageRef.current,
              ),
              nextCursor: preservePagination
                ? current[section.id].nextCursor
                : scopedSections.find((item) => item.id === section.id)!
                    .nextCursor,
            };
          }
          return next;
        });
        setRetainedSmartThreads((current) =>
          discardRetainedSmartThreadsReplacedByAuthoritativeStarred(
            current,
            authoritativeSections,
          ),
        );
        return;
      }

      const specialUnread = currentView.mailbox === "unread";
      const specialFlagged = currentView.mailbox === "starred";
      const actualMailbox =
        ["unread", "starred"].includes(currentView.mailbox) ||
        currentView.mailbox === ""
          ? undefined
          : currentView.mailbox;
      const page = await api.search(
        currentView.query,
        currentView.accountIds,
        actualMailbox,
        specialUnread,
        specialFlagged,
        mailPageSize,
        null,
      );
      if (
        requestId !== loadRequestIdRef.current ||
        firstPageGeneration !== mailViewFirstPageGenerationRef.current ||
        !sameMailView(currentView, currentViewRef.current)
      ) {
        return;
      }
      const visible = excludeThreads(
        threadsWithinAccountScope(page.conversations, currentView.accountIds),
        mailboxActionThreadIdsRef.current,
      );
      const reconciled = senderCleanupTargetRef.current
        ? removeSenderCleanupMessages(visible, senderCleanupTargetRef.current)
        : visible;
      // This first-page read closes a missed startup event. It only adds rows
      // absent from the visible range so a partial page cannot overwrite a
      // newer optimistic mutation or remove an appended older page.
      setThreads((current) =>
        mergeFirstPageThreads(
          current,
          reconciled,
          readMutationByMessageRef.current,
          starMutationByMessageRef.current,
        ),
      );
      if (!preservePagination) {
        nextCursorRef.current = page.nextCursor;
        setHasMore(page.nextCursor !== null);
      }
    },
    [],
  );
  const removeAccountFromMain = useCallback(
    (accountId: string) => {
      removedAccountIdsRef.current.add(accountId);
      accountStateGenerationRef.current += 1;
      const next = accountsRef.current.filter(
        (account) => account.id !== accountId,
      );
      const selectedAccountStillExists =
        selectedAccountIdRef.current === accountId
          ? undefined
          : selectedAccountIdRef.current;
      accountsRef.current = next;
      selectedAccountIdRef.current = selectedAccountStillExists;
      setAccounts(next);
      setSelectedAccountId(selectedAccountStillExists);
      setThreads((current) =>
        current.filter((thread) => thread.latest.account_id !== accountId),
      );
      setSmartSections(
        (current) =>
          Object.fromEntries(
            smartSectionIds.map((id) => [
              id,
              {
                ...current[id],
                threads: current[id].threads.filter(
                  (thread) => thread.latest.account_id !== accountId,
                ),
              },
            ]),
          ) as Record<SmartSectionId, SmartSection>,
      );
      setActiveThreadSnapshot((current) =>
        current?.latest.account_id === accountId ? undefined : current,
      );
      setActive((current) =>
        current?.account_id === accountId ? undefined : current,
      );
      setSelected(new Set());
      setOutbox((current) =>
        current.filter((message) => message.account_id !== accountId),
      );
      const nextAccountIds = selectedAccountStillExists
        ? [selectedAccountStillExists]
        : next.map((account) => account.id);
      currentViewRef.current = {
        ...currentViewRef.current,
        accountIds: nextAccountIds,
      };
      void loadMessages(nextAccountIds);
    },
    [loadMessages],
  );
  const classifyPending = useCallback(async () => {
    classificationRequestedRef.current = true;
    if (classifyingRef.current) return;
    classifyingRef.current = true;
    setClassifying(true);
    try {
      while (classificationRequestedRef.current) {
        classificationRequestedRef.current = false;
        await api.classifyPending();
        await loadMessages();
      }
    } catch (error) {
      showError(error);
    } finally {
      classifyingRef.current = false;
      setClassifying(false);
      if (classificationRequestedRef.current) void classifyPending();
    }
  }, [loadMessages]);
  useEffect(() => {
    let disposed = false;
    let dispose: () => void = () => undefined;
    void onMailRebuildFinished(() => {
      // A cancelled or failed rebuild can occur after the finding phase has
      // optimistically removed this account's old rows from the list.
      setActive(undefined);
      setActiveThreadSnapshot(undefined);
      setSelected(new Set());
      void loadMessages().finally(() => setSyncStatus(undefined));
    }).then((unlisten) => {
      if (disposed) unlisten();
      else dispose = unlisten;
    });
    return () => {
      disposed = true;
      dispose();
    };
  }, [loadMessages]);
  const loadMoreMessages = useCallback(async () => {
    if (loadingMoreRef.current || !hasMore || !mailFirstPageReadyRef.current)
      return;
    const currentView = currentViewRef.current;
    if (currentView.mailbox === "Outbox" || !currentView.accountIds.length) {
      return;
    }
    const requestId = loadRequestIdRef.current;
    loadingMoreRef.current = true;
    setLoadingMore(true);
    try {
      const specialUnread = currentView.mailbox === "unread";
      const specialFlagged = currentView.mailbox === "starred";
      const actualMailbox =
        ["unread", "starred"].includes(currentView.mailbox) ||
        currentView.mailbox === ""
          ? undefined
          : currentView.mailbox;
      const page = await api.search(
        currentView.query,
        currentView.accountIds,
        actualMailbox,
        specialUnread,
        specialFlagged,
        mailPageSize,
        nextCursorRef.current,
      );
      if (
        requestId !== loadRequestIdRef.current ||
        !sameMailView(currentView, currentViewRef.current)
      )
        return;
      nextCursorRef.current = page.nextCursor;
      mailViewHasAppendedPagesRef.current = true;
      setHasMore(page.nextCursor !== null);
      setThreads((current) =>
        mergeThreads(
          current,
          threadsWithinAccountScope(page.conversations, currentView.accountIds),
        ),
      );
    } catch (error) {
      if (
        requestId === loadRequestIdRef.current &&
        sameMailView(currentView, currentViewRef.current)
      )
        showError(error);
    } finally {
      if (
        requestId === loadRequestIdRef.current &&
        sameMailView(currentView, currentViewRef.current)
      ) {
        loadingMoreRef.current = false;
        setLoadingMore(false);
      }
    }
  }, [hasMore]);
  const loadMoreSmartSection = useCallback(
    async (id: SmartSectionId) => {
      const currentView = currentViewRef.current;
      const section = smartSections[id];
      if (
        smartLoadingMoreRef.current.has(id) ||
        !smartFirstPageReadyRef.current ||
        !section.nextCursor ||
        currentView.view !== "smart" ||
        currentView.mailbox !== "INBOX" ||
        currentView.query.trim() ||
        !currentView.accountIds.length
      ) {
        return;
      }
      const requestId = smartLoadRequestIdRef.current;
      smartLoadingMoreRef.current.add(id);
      setSmartSections((current) => ({
        ...current,
        [id]: { ...current[id], loadingMore: true },
      }));
      try {
        const page = await api.search(
          "",
          currentView.accountIds,
          "INBOX",
          !["starred", "seen"].includes(id),
          id === "starred",
          smartMorePageSize,
          section.nextCursor,
          !["starred", "seen"].includes(id) ? id : undefined,
          id !== "starred",
          id === "seen",
        );
        if (
          requestId !== smartLoadRequestIdRef.current ||
          !sameMailView(currentView, currentViewRef.current)
        )
          return;
        mailViewHasAppendedPagesRef.current = true;
        const authoritativeThreads = threadsWithinAccountScope(
          page.conversations,
          currentView.accountIds,
        );
        setSmartSections((current) => ({
          ...current,
          [id]: {
            ...current[id],
            threads: mergeThreads(current[id].threads, authoritativeThreads),
            nextCursor: page.nextCursor,
            loadingMore: false,
          },
        }));
        setRetainedSmartThreads((current) =>
          discardRetainedSmartThreadsReplacedByAuthoritativeStarred(current, [
            { id, threads: authoritativeThreads },
          ]),
        );
      } catch (error) {
        if (
          requestId === smartLoadRequestIdRef.current &&
          sameMailView(currentView, currentViewRef.current)
        )
          showError(error);
      } finally {
        smartLoadingMoreRef.current.delete(id);
        if (
          requestId === smartLoadRequestIdRef.current &&
          sameMailView(currentView, currentViewRef.current)
        ) {
          setSmartSections((current) => ({
            ...current,
            [id]: { ...current[id], loadingMore: false },
          }));
        }
      }
    },
    [smartSections],
  );
  useEffect(() => {
    let disposed = false;
    let disposeAccount: () => void = () => undefined;
    let disposeSettings: () => void = () => undefined;
    let disposeNotifications: () => void = () => undefined;
    void onAccountConnected(({ account }) => {
      accountStateGenerationRef.current += 1;
      setAccounts((current) => {
        const next = [
          ...current.filter((item) => item.id !== account.id),
          account,
        ];
        accountsRef.current = next;
        return next;
      });
      // The native backend owns rebuild scheduling and execution. This event
      // is best-effort across windows, so it only updates the visible account
      // list; progress and terminal rebuild events drive the mail reload.
      void requestInitialNotificationAccess(notificationSettings)
        .then(() =>
          loadMessages(
            selectedAccountId
              ? [selectedAccountId]
              : [...new Set([...activeAccounts, account.id])],
          ),
        )
        .catch(showError)
        .finally(() => void classifyPending());
    }).then((unlisten) => {
      if (disposed) unlisten();
      else disposeAccount = unlisten;
    });
    void onSettingsChanged(setAiSettings).then((unlisten) => {
      if (disposed) unlisten();
      else disposeSettings = unlisten;
    });
    void onNotificationSettingsChanged(setNotificationSettings).then(
      (unlisten) => {
        if (disposed) unlisten();
        else disposeNotifications = unlisten;
      },
    );
    return () => {
      disposed = true;
      disposeAccount();
      disposeSettings();
      disposeNotifications();
    };
  }, [
    activeAccounts,
    classifyPending,
    loadMessages,
    markSynced,
    notificationSettings,
    selectedAccountId,
  ]);
  useEffect(() => {
    let disposed = false;
    let unlisten: () => void = () => undefined;
    void onAccountUpdated((updated) => {
      if (removedAccountIdsRef.current.has(updated.id)) return;
      accountStateGenerationRef.current += 1;
      const index = accountsRef.current.findIndex(
        (account) => account.id === updated.id,
      );
      const next = [...accountsRef.current];
      if (index === -1) next.push(updated);
      else next[index] = updated;
      accountsRef.current = next;
      setAccounts(next);
    }).then((listener) => {
      if (disposed) listener();
      else unlisten = listener;
    });
    return () => {
      disposed = true;
      unlisten();
    };
  }, []);
  useEffect(() => {
    let disposed = false;
    let unlisten: () => void = () => undefined;
    void onAccountRemoved(({ accountId }) => {
      removeAccountFromMain(accountId);
    }).then((listener) => {
      if (disposed) listener();
      else unlisten = listener;
    });
    return () => {
      disposed = true;
      unlisten();
    };
  }, [removeAccountFromMain]);
  useEffect(() => {
    let disposed = false;
    let dispose: () => void = () => undefined;
    void onMailIndexRebuilt(() => {
      setActive(undefined);
      setActiveThreadSnapshot(undefined);
      setSelected(new Set());
      void loadMessages().finally(() => {
        markSynced();
        setSyncStatus(undefined);
      });
    }).then((unlisten) => {
      if (disposed) unlisten();
      else dispose = unlisten;
    });
    return () => {
      disposed = true;
      dispose();
    };
  }, [loadMessages]);
  useEffect(() => {
    let disposed = false;
    let dispose: () => void = () => undefined;
    const showRebuildProgress = (progress: MailRebuildProgress) => {
      const account = accounts.find((item) => item.id === progress.accountId);
      setSyncStatus({
        phase: progress.phase,
        completed: progress.completed,
        total: progress.total,
        accountEmail: account?.email ?? progress.accountId,
        accountIndex: 1,
        accountCount: 1,
      });
      if (progress.phase === "finding") {
        setThreads((current) =>
          current.filter(
            (thread) => thread.latest.account_id !== progress.accountId,
          ),
        );
        setActive((current) =>
          current?.account_id === progress.accountId ? undefined : current,
        );
        setActiveThreadSnapshot((current) =>
          current?.latest.account_id === progress.accountId
            ? undefined
            : current,
        );
        setSelected(new Set());
      } else if (progress.phase === "saving") {
        void loadMessages();
      }
    };
    void api
      .mailRebuildStatus()
      .then(([progress]) => progress && showRebuildProgress(progress))
      .catch(showError);
    void onMailRebuildProgress(showRebuildProgress).then((unlisten) => {
      if (disposed) unlisten();
      else dispose = unlisten;
    });
    return () => {
      disposed = true;
      dispose();
    };
  }, [accounts, loadMessages]);
  useEffect(() => {
    let disposed = false;
    let unlisten: () => void = () => undefined;
    void onComposeSent(() => {
      showStatus(t("feedback.sent"));
      void loadMessages();
    }).then((dispose) => {
      if (disposed) dispose();
      else unlisten = dispose;
    });
    return () => {
      disposed = true;
      unlisten();
    };
  }, [loadMessages, showStatus, t]);
  useEffect(() => {
    let disposed = false;
    let unlisten: () => void = () => undefined;
    void onOutboxChanged((event) => {
      setOutbox((current) =>
        event.phase === "sending"
          ? [
              ...current.filter((message) => message.id !== event.message.id),
              event.message,
            ]
          : current.filter((message) => message.id !== event.id),
      );
      if (event.phase === "finished")
        void refreshSendRecoveriesRef.current(event.id);
    }).then((dispose) => {
      if (disposed) dispose();
      else unlisten = dispose;
    });
    return () => {
      disposed = true;
      unlisten();
    };
  }, []);
  useEffect(() => {
    if (!activeAccounts.length) return;
    void loadMessages().then(() => {
      if (initialClassificationStartedRef.current) return;
      initialClassificationStartedRef.current = true;
      void classifyPending();
    });
  }, [
    activeAccounts,
    classifyPending,
    debouncedQuery,
    loadMessages,
    mailbox,
    mailListView,
  ]);
  useEffect(() => {
    let disposed = false;
    const unlisteners: Array<() => void> = [];
    const subscriptionAbort = new AbortController();
    void subscribeRealtimeEvents(
      {
        arrived: (arrival) => {
          if (removedAccountIdsRef.current.has(arrival.accountId)) return;
          void (async () => {
            markSynced();
            await loadMessages();
            if (removedAccountIdsRef.current.has(arrival.accountId)) return;
            await requestInitialNotificationAccess(notificationSettings);
            if (removedAccountIdsRef.current.has(arrival.accountId)) return;
            const focused = await getCurrentWindow().isFocused();
            if (removedAccountIdsRef.current.has(arrival.accountId)) return;
            if (!focused) {
              const delivered = await sendNewMailNotification(
                arrival.messages,
                notificationSettings,
                notificationCopy(t),
                () => !removedAccountIdsRef.current.has(arrival.accountId),
              );
              if (
                delivered &&
                !removedAccountIdsRef.current.has(arrival.accountId)
              ) {
                await api.recordNotificationDelivered(
                  arrival.accountId,
                  arrival.eventId,
                  arrival.detectedAt,
                );
              }
            }
          })().catch(showError);
        },
        hydrated: () => {
          void classifyPending();
        },
        changed: () => {
          void loadMessages().catch(showError);
        },
        syncState: () => undefined,
      },
      subscriptionAbort.signal,
    )
      .then(async (unlisten) => {
        if (disposed) {
          unlisten();
          return;
        }
        unlisteners.push(unlisten);
        const firstPageGeneration = mailViewFirstPageGenerationRef.current;
        await api.configureTray(t("tray.open"), t("tray.quit"));
        await api.startRealtimeSync();
        if (
          !disposed &&
          firstPageGeneration === mailViewFirstPageGenerationRef.current
        ) {
          await reconcileCurrentFirstPage(mailViewHasAppendedPagesRef.current);
        }
      })
      .catch((error) => {
        if (!disposed) showError(error);
      });
    return () => {
      disposed = true;
      subscriptionAbort.abort();
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, [
    classifyPending,
    loadMessages,
    markSynced,
    notificationSettings,
    reconcileCurrentFirstPage,
    t,
  ]);

  const smartInboxActive =
    mailListView === "smart" && mailbox === "INBOX" && !query.trim();
  const smartThreads = useMemo(
    () => smartSectionIds.flatMap((id) => smartSections[id].threads),
    [smartSections],
  );
  const displayedThreads =
    mailbox === "Outbox"
      ? groupMessages(visibleOutbox)
      : smartInboxActive
        ? smartThreads
        : threads;

  useEffect(() => {
    const matchingThread = active
      ? displayedThreads.find(
          (thread) =>
            thread.messages.some((message) => message.id === active.id) ||
            thread.id ===
              `${active.account_id}:${active.thread_id || active.id}`,
        )
      : undefined;
    if (matchingThread) setActiveThreadSnapshot(matchingThread);
    setActive((current) => {
      if (!current) return current;
      const thread = displayedThreads.find(
        (item) =>
          item.messages.some((message) => message.id === current.id) ||
          item.id ===
            `${current.account_id}:${current.thread_id || current.id}`,
      );
      if (!thread) return current;
      return (
        thread.messages.find((message) => message.id === current.id) ??
        thread.messages.find(
          (message) =>
            current.message_id &&
            message.message_id?.toLowerCase() ===
              current.message_id.toLowerCase(),
        ) ??
        thread.latest
      );
    });
  }, [displayedThreads]);

  useEffect(() => {
    let disposed = false;
    let registrationFailed = false;
    const listeners = new Set<() => void>();
    const cleanupListeners = () => {
      for (const unlisten of listeners) unlisten();
      listeners.clear();
    };
    const openNotification = async (extra: Record<string, unknown>) => {
      const accountId =
        typeof extra.accountId === "string" ? extra.accountId : undefined;
      const messageId =
        typeof extra.messageId === "string" ? extra.messageId : undefined;
      const rfcMessageId =
        typeof extra.rfcMessageId === "string" ? extra.rfcMessageId : undefined;
      const threadId =
        typeof extra.threadId === "string" ? extra.threadId : undefined;
      const count = typeof extra.count === "number" ? extra.count : 0;
      if (count === 1 && accountId && (messageId || rfcMessageId || threadId)) {
        try {
          await openReaderWindow({
            target: {
              accountId,
              localMessageId: messageId,
              rfcMessageId,
              threadId,
              mailbox: "INBOX",
            },
            focusedMessageId: messageId,
          });
          return;
        } catch (error) {
          showError(error);
        }
      }

      const currentWindow = getCurrentWindow();
      await currentWindow.show();
      await currentWindow.setFocus();
      setMailbox("INBOX");
      setQuery("");
      setSelected(new Set());
      setAiResult(undefined);

      const accountIds = accountId
        ? [accountId]
        : accounts.map((account) => account.id);
      selectedAccountIdRef.current = accountId;
      setSelectedAccountId(accountId);
      if (!accountIds.length) return;
      const inbox = await api.search("", accountIds, "INBOX");
      nextCursorRef.current = inbox.nextCursor;
      setHasMore(inbox.nextCursor !== null);
      setThreads(inbox.conversations);
      setActive(undefined);
      setActiveThreadSnapshot(undefined);
    };
    const register = (
      setup:
        | ReturnType<typeof onNotificationAction>
        | ReturnType<typeof onDesktopNotificationAction>,
    ) => {
      void setup.then(
        (listener) => {
          const unlisten =
            typeof listener === "function"
              ? listener
              : () => listener.unregister();
          if (disposed || registrationFailed) unlisten();
          else listeners.add(unlisten);
        },
        (error) => {
          if (disposed || registrationFailed) return;
          registrationFailed = true;
          cleanupListeners();
          showError(error);
        },
      );
    };

    register(onNotificationAction(openNotification));
    register(onDesktopNotificationAction(openNotification));
    return () => {
      disposed = true;
      cleanupListeners();
    };
  }, [accounts]);

  useEffect(() => {
    let disposed = false;
    let dispose: () => void = () => undefined;
    const subscriptionAbort = new AbortController();
    void subscribeNativeGroup(
      [
        onReaderWindowMutated(() => {
          void loadMessages();
          void refreshStarredCount(activeAccounts);
        }),
        onReaderWindowFailed(async ({ accountId }) => {
          try {
            const currentWindow = getCurrentWindow();
            await currentWindow.show();
            await currentWindow.setFocus();
            setMailbox("INBOX");
            setQuery("");
            selectedAccountIdRef.current = accountId;
            setSelectedAccountId(accountId);
            setActive(undefined);
            setActiveThreadSnapshot(undefined);
            const inbox = await api.search("", [accountId], "INBOX");
            setThreads(inbox.conversations);
            nextCursorRef.current = inbox.nextCursor;
            setHasMore(inbox.nextCursor !== null);
          } catch (error) {
            showError(error);
          }
        }),
      ],
      subscriptionAbort.signal,
    )
      .then((unlisten) => {
        if (disposed) unlisten();
        else dispose = unlisten;
      })
      .catch((error) => {
        if (!disposed) showError(error);
      });
    return () => {
      disposed = true;
      subscriptionAbort.abort();
      dispose();
    };
  }, [activeAccounts, loadMessages, refreshStarredCount]);

  const activeThread = useMemo(
    () =>
      active
        ? (displayedThreads.find(
            (thread) =>
              thread.messages.some((message) => message.id === active.id) ||
              thread.id ===
                `${active.account_id}:${active.thread_id || active.id}`,
          ) ??
          (activeThreadSnapshot?.messages.some(
            (message) => message.id === active.id,
          )
            ? activeThreadSnapshot
            : undefined))
        : undefined,
    [active, activeThreadSnapshot, displayedThreads],
  );
  activeThreadRef.current = activeThread;
  const targetThreads = selected.size
    ? displayedThreads.filter((thread) => selected.has(thread.id))
    : activeThread
      ? [activeThread]
      : [];
  const targets = targetThreads.flatMap((thread) => thread.messages);
  const selectAccount = (id: string) => {
    selectedAccountIdRef.current = id;
    setSelectedAccountId(id);
    setMailbox("INBOX");
    setActive(undefined);
    setActiveThreadSnapshot(undefined);
    setSelected(new Set());
    setAiResult(undefined);
  };
  const selectMailbox = (value: string) => {
    if (value === "INBOX") {
      selectedAccountIdRef.current = undefined;
      setSelectedAccountId(undefined);
    }
    setMailbox(value);
    setSelected(new Set());
    setActive(undefined);
    setActiveThreadSnapshot(undefined);
    setAiResult(undefined);
  };
  const select = (ids: string[], checked: boolean) =>
    setSelected((current) => {
      const next = new Set(current);
      for (const id of ids) checked ? next.add(id) : next.delete(id);
      return next;
    });

  const sync = async () => {
    if (syncStatus) return;
    try {
      const targets = accounts.filter((item) =>
        activeAccounts.includes(item.id),
      );
      await syncAccounts(targets, setSyncStatus, () => void loadMessages());
      markSynced();
      await requestInitialNotificationAccess(notificationSettings);
      await loadMessages();
      await refreshStarredCount(activeAccounts);
      void classifyPending();
    } catch (error) {
      showError(error);
    } finally {
      setSyncStatus(undefined);
    }
  };
  const applyAction = async (
    action: MailAction,
    explicitThreads: MailThread[] = targetThreads,
  ) => {
    const actionThreads = explicitThreads
      .map((thread) => ({
        thread,
        messages: conversationActionMessages(thread, mailbox, action),
      }))
      .filter((target) => target.messages.length > 0);
    if (!actionThreads.length) return;
    const actionView: MailViewRequest = {
      ...currentViewRef.current,
      accountIds: [...currentViewRef.current.accountIds],
    };
    mailboxActionsInFlightRef.current += 1;
    // An open/read transition may already be refreshing Smart sections with
    // the pre-action INBOX row. Make that result stale before moving the row.
    loadRequestIdRef.current += 1;
    smartLoadRequestIdRef.current += 1;
    const actionCatalogueRevision = catalogueRevisionRef.current;
    const originalThreads = [...displayedThreads];
    const originalSmartSections = smartSections;
    const originalRetainedSmartThreads = retainedSmartThreads;
    const actionTargets = actionThreads.flatMap((target) => target.messages);
    const targetIds = new Set(actionTargets.map((message) => message.id));
    const targetThreadIds = new Set(
      actionThreads.map((target) => target.thread.id),
    );
    for (const id of targetThreadIds) {
      const nextCount = (mailboxActionThreadCountsRef.current.get(id) ?? 0) + 1;
      mailboxActionThreadCountsRef.current.set(id, nextCount);
      mailboxActionThreadIdsRef.current.add(id);
    }
    const reducedMotion = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    setPendingActions((current) => {
      const next = { ...current };
      actionTargets.forEach((message, index) => {
        next[message.id] = {
          action,
          phase: "exiting",
          delay: reducedMotion ? 0 : Math.min(index * 24, 144),
        };
      });
      return next;
    });
    setSelected(new Set());
    setAiResult(undefined);
    if (activeThread && targetThreadIds.has(activeThread.id)) {
      const activeIndex = displayedThreads.findIndex(
        (thread) => thread.id === activeThread.id,
      );
      const next =
        displayedThreads
          .slice(activeIndex + 1)
          .find((thread) => !targetThreadIds.has(thread.id)) ??
        displayedThreads
          .slice(0, activeIndex)
          .reverse()
          .find((thread) => !targetThreadIds.has(thread.id));
      setActive(next?.latest);
      setActiveThreadSnapshot(next);
    }

    // Remove the affected conversations in the same render as the action.
    // Network work continues independently so the next visible conversation
    // can be acted on immediately.
    setThreads((current) =>
      current.filter((thread) => !targetThreadIds.has(thread.id)),
    );
    setSmartSections(
      (current) =>
        Object.fromEntries(
          smartSectionIds.map((id) => [
            id,
            {
              ...current[id],
              threads: current[id].threads.filter(
                (thread) => !targetThreadIds.has(thread.id),
              ),
            },
          ]),
        ) as Record<SmartSectionId, SmartSection>,
    );
    setRetainedSmartThreads((current) => {
      const next = new Map(current);
      for (const id of targetThreadIds) next.delete(id);
      return next;
    });

    const resultsPromise = Promise.allSettled(
      actionThreads.map(({ messages }) =>
        Promise.all(
          messages.map((message) =>
            api.action(
              message.account_id,
              message.mailbox,
              message.uid,
              action,
              message.id,
              message.mailbox_uid_validity ?? null,
            ),
          ),
        ),
      ),
    );
    const results = await resultsPromise;
    if (!mountedRef.current) return;
    const failedThreadIds = new Set(
      actionThreads
        .filter((_, index) => results[index].status === "rejected")
        .map((target) => target.thread.id),
    );
    const failedIds = new Set(
      actionThreads
        .filter((target) => failedThreadIds.has(target.thread.id))
        .flatMap((target) => target.messages.map((message) => message.id)),
    );
    const succeeded = actionThreads.length - failedThreadIds.size;

    if (failedThreadIds.size) {
      setPendingActions((current) => {
        const next = { ...current };
        for (const message of actionTargets) {
          if (failedIds.has(message.id)) {
            next[message.id] = { action, phase: "restoring", delay: 0 };
          } else {
            delete next[message.id];
          }
        }
        return next;
      });
      const currentRevision = catalogueRevisionRef.current;
      const unchangedCatalogue = actionCatalogueRevision === currentRevision;
      const observationFor = (threadId: string) => {
        const observation =
          mailboxActionPageObservationsRef.current.get(threadId);
        return observation?.revision === currentRevision
          ? observation
          : undefined;
      };
      const restorableThreadIds = new Set(
        [...failedThreadIds].filter((threadId) => {
          if (unchangedCatalogue) return true;
          const observation = observationFor(threadId);
          if (!observation) return false;
          const present = usesSmartInbox(actionView)
            ? Boolean(
                observation.smartThreads &&
                Object.keys(observation.smartThreads).length,
              )
            : Boolean(observation.listThread);
          return present || !observation.complete;
        }),
      );
      if (
        restorableThreadIds.size > 0 &&
        sameMailView(actionView, currentViewRef.current)
      ) {
        setSelected((current) => {
          const next = new Set(current);
          for (const id of restorableThreadIds) next.add(id);
          return next;
        });
        const latestListThreads = originalThreads.map((thread) => {
          if (!restorableThreadIds.has(thread.id)) return thread;
          return observationFor(thread.id)?.listThread ?? thread;
        });
        setThreads((current) =>
          restoreThreads(current, latestListThreads, restorableThreadIds),
        );
        setSmartSections(
          (current) =>
            Object.fromEntries(
              smartSectionIds.map((id) => {
                const restoreCandidates = originalSmartSections[
                  id
                ].threads.filter(
                  (thread) => !restorableThreadIds.has(thread.id),
                );
                for (const threadId of restorableThreadIds) {
                  const observation = observationFor(threadId);
                  const observedMembership = observation?.smartThreads;
                  const observedThread = observedMembership?.[id];
                  if (observedThread) {
                    restoreCandidates.push(observedThread);
                    continue;
                  }
                  if (
                    observedMembership &&
                    Object.keys(observedMembership).length > 0
                  )
                    continue;
                  const original = originalSmartSections[id].threads.find(
                    (thread) => thread.id === threadId,
                  );
                  if (original) restoreCandidates.push(original);
                }
                return [
                  id,
                  {
                    ...current[id],
                    threads: restoreThreads(
                      current[id].threads,
                      restoreCandidates,
                      restorableThreadIds,
                    ),
                  },
                ];
              }),
            ) as Record<SmartSectionId, SmartSection>,
        );
        setRetainedSmartThreads((current) => {
          const next = new Map(current);
          for (const id of restorableThreadIds) {
            next.delete(id);
            const observed = observationFor(id)?.smartThreads;
            const hasObservedMembership = Boolean(
              observed && Object.keys(observed).length,
            );
            const retained = hasObservedMembership
              ? undefined
              : originalRetainedSmartThreads.get(id);
            if (retained) next.set(id, retained);
          }
          return next;
        });
      }
      showStatus(
        actionOutcome(t, action, succeeded, failedThreadIds.size),
        "error",
      );
      await wait(reducedMotion ? 0 : 260);
    } else {
      showStatus(actionOutcome(t, action, succeeded, 0));
    }

    if (!mountedRef.current) return;

    setPendingActions((current) => {
      const next = { ...current };
      for (const id of targetIds) delete next[id];
      return next;
    });
    mailboxActionsInFlightRef.current -= 1;
    for (const id of targetThreadIds) {
      const nextCount = (mailboxActionThreadCountsRef.current.get(id) ?? 1) - 1;
      if (nextCount > 0) {
        mailboxActionThreadCountsRef.current.set(id, nextCount);
      } else {
        mailboxActionThreadCountsRef.current.delete(id);
        mailboxActionThreadIdsRef.current.delete(id);
        mailboxActionPageObservationsRef.current.delete(id);
      }
    }
    if (mailboxActionsInFlightRef.current === 0) await loadMessages();
  };
  const permanentlyDeleteMessage = async (message: MailSummary) => {
    if (actionBusyRef.current) return;
    actionBusyRef.current = true;
    setPermanentDeleteLoading(true);
    const remainingActiveThread = activeThread
      ? removeConcreteMessage(activeThread, message)
      : undefined;
    const replacementThread =
      activeThread && !remainingActiveThread
        ? (displayedThreads
            .slice(
              displayedThreads.findIndex(
                (thread) => thread.id === activeThread.id,
              ) + 1,
            )
            .find((thread) => thread.id !== activeThread.id) ??
          displayedThreads
            .slice(
              0,
              displayedThreads.findIndex(
                (thread) => thread.id === activeThread.id,
              ),
            )
            .reverse()
            .find((thread) => thread.id !== activeThread.id))
        : undefined;
    try {
      await api.action(
        message.account_id,
        message.mailbox,
        message.uid,
        "delete",
        message.id,
        message.mailbox_uid_validity ?? null,
      );
      // Prevent an in-flight fetch that still contains this locator from
      // restoring the message after the server has deleted it. Do this only
      // after success so a failed delete cannot strand an invalidated load in
      // its loading state.
      loadRequestIdRef.current += 1;
      smartLoadRequestIdRef.current += 1;
      loadingMoreRef.current = false;
      setLoadingMore(false);
      smartLoadingMoreRef.current.clear();
      setThreads((current) =>
        removeConcreteMessageFromThreads(current, message),
      );
      setSmartSections(
        (current) =>
          Object.fromEntries(
            smartSectionIds.map((id) => [
              id,
              {
                ...current[id],
                loadingMore: false,
                threads: removeConcreteMessageFromThreads(
                  current[id].threads,
                  message,
                ),
              },
            ]),
          ) as Record<SmartSectionId, SmartSection>,
      );
      setRetainedSmartThreads((current) => {
        const next = new Map(current);
        for (const [id, retained] of next) {
          const thread = removeConcreteMessage(retained.thread, message);
          if (thread) next.set(id, { ...retained, thread });
          else next.delete(id);
        }
        return next;
      });
      setActive((current) => {
        if (!current || !sameMessageLocator(current, message)) return current;
        if (!remainingActiveThread) return replacementThread?.latest;
        return (
          nextMessageAfterAction(
            activeThread?.messages ?? [],
            current,
            new Set([message.id]),
          ) ?? remainingActiveThread.latest
        );
      });
      setActiveThreadSnapshot((current) => {
        if (!current) return current;
        const thread = removeConcreteMessage(current, message);
        return thread ?? replacementThread;
      });
      if (!remainingActiveThread && activeThread) {
        setSelected((current) => {
          const next = new Set(current);
          next.delete(activeThread.id);
          return next;
        });
      }
      setAiResult(undefined);
      showStatus(t("feedback.permanentDeleteSuccess"));
      await loadMessages();
    } catch {
      showStatus(t("feedback.permanentDeleteFailed"), "error");
    } finally {
      actionBusyRef.current = false;
      setPermanentDeleteLoading(false);
    }
  };
  const runSummary = async () => {
    if (!targets.length) return;
    setAiLoading(true);
    try {
      setAiResult(
        await api.summarize(
          aiSettings,
          targets.map((item) => ({
            messageId: item.id,
            expectedUidValidity: item.mailbox_uid_validity ?? null,
          })),
        ),
      );
    } catch (error) {
      showError(error, t("ai.error"));
    } finally {
      setAiLoading(false);
    }
  };
  const openReplyForMessage = async (
    replyMessage: MailSummary | undefined,
    replyAll = false,
    contextMessages: MailSummary[] = activeThread?.messages ?? [],
  ) => {
    if (!replyMessage) return;
    const account = accounts.find(
      (item) => item.id === replyMessage.account_id,
    );
    // A Reply button on a sent message may need a conversation counterpart for
    // recipient calculation, but the clicked message remains the provenance of
    // the subject, quoted body, and threading headers.
    const recipientMessage =
      account &&
      replyMessage.from_address.toLowerCase() === account.email.toLowerCase()
        ? ([...contextMessages]
            .reverse()
            .find(
              (item) =>
                item.from_address.toLowerCase() !== account.email.toLowerCase(),
            ) ?? replyMessage)
        : replyMessage;
    const recipients = replyRecipients(
      recipientMessage,
      account?.email,
      replyAll,
    );
    if (!recipients) return;
    try {
      const content = await api.content(
        replyMessage.id,
        replyMessage.mailbox_uid_validity ?? null,
      );
      const replyHistory = formatReplyHistory({
        message: replyMessage,
        bodyText: content.body_text,
        bodyHtml: content.body_html,
        formatCitation: ({ date, sender }) =>
          t("reader.replyCitation", { date, sender }),
      });
      const prefix = t("reader.replyPrefix");
      openComposeWindow({
        accountId: replyMessage.account_id,
        to: recipients.to,
        ...(recipients.cc ? { cc: recipients.cc } : {}),
        subject: replyMessage.subject
          .toLowerCase()
          .startsWith(prefix.toLowerCase())
          ? replyMessage.subject
          : `${prefix} ${replyMessage.subject}`,
        body: replyHistory.body,
        bodyHtml: replyHistory.bodyHtml,
        inReplyTo: replyMessage.message_id ?? undefined,
        references: [replyMessage.reference_ids, replyMessage.message_id]
          .filter(Boolean)
          .join(" "),
        contextMessageIds: (contextMessages.length
          ? contextMessages
          : [replyMessage]
        ).map((message) => message.id),
        contextMessageTargets: (contextMessages.length
          ? contextMessages
          : [replyMessage]
        ).map((message) => ({
          messageId: message.id,
          expectedUidValidity: message.mailbox_uid_validity ?? null,
        })),
      });
    } catch (error) {
      await showNativeMessage(
        t("reader.replyErrorTitle"),
        error instanceof Error ? error.message : String(error),
        "error",
      );
    }
  };
  const openThreadReply = async (
    thread: MailThread | undefined = activeThread,
    replyAll = false,
  ) => {
    if (!thread) return;
    const account = accounts.find(
      (item) => item.id === thread.latest.account_id,
    );
    const replyMessage =
      [...thread.messages]
        .reverse()
        .find(
          (item) =>
            !account ||
            item.from_address.toLowerCase() !== account.email.toLowerCase(),
        ) ?? thread.latest;
    await openReplyForMessage(replyMessage, replyAll, thread.messages);
  };
  const openForwardForMessage = async (message: MailSummary | undefined) => {
    if (!message) return;
    try {
      const content = await api.content(
        message.id,
        message.mailbox_uid_validity ?? null,
      );
      const history = formatForwardHistory(message, content, {
        originalMessage: t("reader.originalMessage"),
        from: t("composer.from"),
        date: t("reader.date"),
        subject: t("composer.subject"),
        to: t("composer.to"),
      });
      openComposeWindow({
        accountId: message.account_id,
        subject: forwardSubject(message.subject, t("reader.forwardPrefix")),
        body: history.body,
        bodyHtml: history.bodyHtml,
        forwardMessageId: content.attachments.some(
          (attachment) => attachment.presentation !== "embedded",
        )
          ? message.id
          : undefined,
        forwardMessageUidValidity: content.attachments.some(
          (attachment) => attachment.presentation !== "embedded",
        )
          ? (message.mailbox_uid_validity ?? null)
          : undefined,
        contextMessageIds: [message.id],
        contextMessageTargets: [
          {
            messageId: message.id,
            expectedUidValidity: message.mailbox_uid_validity ?? null,
          },
        ],
      });
    } catch (error) {
      await showNativeMessage(
        t("reader.forwardErrorTitle"),
        error instanceof Error ? error.message : String(error),
        "error",
      );
    }
  };
  const openThreadForward = async (
    thread: MailThread | undefined = activeThread,
  ) => openForwardForMessage(thread?.latest);
  const openSendAgainForMessage = async (message: MailSummary) => {
    try {
      const content = await api.content(
        message.id,
        message.mailbox_uid_validity ?? null,
      );
      openComposeWindow({
        accountId: message.account_id,
        to: message.to_addresses,
        ...(message.cc_addresses ? { cc: message.cc_addresses } : {}),
        ...(message.bcc_addresses ? { bcc: message.bcc_addresses } : {}),
        subject: message.subject,
        body: content.body_text,
        ...(content.body_html ? { bodyHtml: content.body_html } : {}),
        forwardMessageId: content.attachments.some(
          (attachment) => attachment.presentation !== "embedded",
        )
          ? message.id
          : undefined,
        forwardMessageUidValidity: content.attachments.some(
          (attachment) => attachment.presentation !== "embedded",
        )
          ? (message.mailbox_uid_validity ?? null)
          : undefined,
      });
    } catch (error) {
      await showNativeMessage(
        t("reader.sendAgainErrorTitle"),
        error instanceof Error ? error.message : String(error),
        "error",
      );
    }
  };
  const toggleThreadStar = async (thread: MailThread, flagged: boolean) => {
    if (actionBusyRef.current) return;
    actionBusyRef.current = true;
    const sourceMessages = concreteThreadMessages(thread);
    const previous = sourceMessages.map((message) => ({
      id: message.id,
      is_flagged: message.is_flagged,
    }));
    const ids = new Set(previous.map((message) => message.id));
    const mutationGeneration = ++starMutationGenerationRef.current;
    for (const id of ids)
      starMutationByMessageRef.current.set(id, mutationGeneration);
    const updateFlag = (message: MailSummary, value: boolean) =>
      ids.has(message.id) ? { ...message, is_flagged: value } : message;
    setThreads((current) =>
      current.map((item) => ({
        ...item,
        messages: item.messages.map((message) => updateFlag(message, flagged)),
        sourceMessages: item.sourceMessages?.map((message) =>
          updateFlag(message, flagged),
        ),
        latest: updateFlag(item.latest, flagged),
      })),
    );
    setSmartSections(
      (current) =>
        Object.fromEntries(
          smartSectionIds.map((id) => {
            const sectionThreads = current[id].threads.map((item) => ({
              ...item,
              messages: item.messages.map((message) =>
                updateFlag(message, flagged),
              ),
              sourceMessages: item.sourceMessages?.map((message) =>
                updateFlag(message, flagged),
              ),
              latest: updateFlag(item.latest, flagged),
            }));
            return [
              id,
              {
                ...current[id],
                threads:
                  id === "starred" || !flagged
                    ? sectionThreads
                    : sectionThreads.filter(
                        (item) =>
                          !concreteThreadMessages(item).some(
                            (message) => message.is_flagged,
                          ),
                      ),
              },
            ];
          }),
        ) as Record<SmartSectionId, SmartSection>,
    );
    setActive((current) => (current ? updateFlag(current, flagged) : current));
    setActiveThreadSnapshot((current) =>
      current
        ? {
            ...current,
            messages: current.messages.map((message) =>
              updateFlag(message, flagged),
            ),
            sourceMessages: current.sourceMessages?.map((message) =>
              updateFlag(message, flagged),
            ),
            latest: updateFlag(current.latest, flagged),
          }
        : current,
    );
    const results = await Promise.allSettled(
      sourceMessages.map((message) =>
        api.setStarred(
          message.id,
          flagged,
          message.mailbox_uid_validity ?? null,
        ),
      ),
    );
    const failed = new Set(
      results.flatMap((result, index) =>
        result.status === "rejected" &&
        starMutationByMessageRef.current.get(previous[index].id) ===
          mutationGeneration
          ? [previous[index].id]
          : [],
      ),
    );
    if (failed.size) {
      const prior = new Map(previous.map((item) => [item.id, item.is_flagged]));
      const restoreFlag = (message: MailSummary) =>
        failed.has(message.id)
          ? { ...message, is_flagged: prior.get(message.id) ?? false }
          : message;
      setThreads((current) =>
        current.map((item) => ({
          ...item,
          messages: item.messages.map(restoreFlag),
          sourceMessages: item.sourceMessages?.map(restoreFlag),
          latest: restoreFlag(item.latest),
        })),
      );
      setActive((current) => (current ? restoreFlag(current) : current));
      setActiveThreadSnapshot((current) =>
        current
          ? {
              ...current,
              messages: current.messages.map(restoreFlag),
              sourceMessages: current.sourceMessages?.map(restoreFlag),
              latest: restoreFlag(current.latest),
            }
          : current,
      );
      showStatus(t("feedback.starFailed", { count: failed.size }), "error");
    } else {
      showStatus(
        t(flagged ? "feedback.starSuccess" : "feedback.unstarSuccess"),
      );
    }
    await refreshStarredCount(activeAccounts);
    for (const id of ids) {
      if (starMutationByMessageRef.current.get(id) === mutationGeneration)
        starMutationByMessageRef.current.delete(id);
    }
    actionBusyRef.current = false;
    await loadMessages();
  };
  const setThreadReadState = async (
    thread: MailThread,
    read: boolean,
    options?: { silent?: boolean },
  ) => {
    const targets = concreteThreadMessages(thread).filter(
      (message) => message.is_read !== read,
    );
    if (!targets.length) return;
    const mutationView: MailViewRequest = {
      ...currentViewRef.current,
      accountIds: [...currentViewRef.current.accountIds],
    };
    const mutationCatalogueRevision = catalogueRevisionRef.current;
    const silent = Boolean(options?.silent);
    const previous = targets.map((message) => ({
      id: message.id,
      is_read: message.is_read,
    }));
    const ids = new Set(previous.map((message) => message.id));
    const mutationGeneration = ++readMutationGenerationRef.current;
    for (const id of ids)
      readMutationByMessageRef.current.set(id, mutationGeneration);
    const previousThreads = threads;
    const previousSmartSections = smartSections;
    setThreads((current) => {
      const updated = current.map((item) =>
        updateThreadReadState(item, ids, read),
      );
      return read && mailbox === "unread"
        ? updated.filter((item) => item.id !== thread.id)
        : updated;
    });
    setSmartSections(
      (current) =>
        Object.fromEntries(
          smartSectionIds.map((id) => {
            const sectionThreads = current[id].threads.map((item) =>
              updateThreadReadState(item, ids, read),
            );
            return [
              id,
              {
                ...current[id],
                threads:
                  read && !["starred", "seen"].includes(id)
                    ? sectionThreads.filter(
                        (item) => item.unread || item.id === thread.id,
                      )
                    : !read && id === "seen"
                      ? sectionThreads.filter((item) => item.unread)
                      : sectionThreads,
              },
            ];
          }),
        ) as Record<SmartSectionId, SmartSection>,
    );
    setRetainedSmartThreads((current) => {
      const next = new Map(current);
      const retained = next.get(thread.id);
      if (retained) {
        next.set(thread.id, {
          ...retained,
          thread: updateThreadReadState(retained.thread, ids, read),
        });
      }
      return next;
    });
    setActive((current) =>
      current ? updateMessageReadState(current, ids, read) : current,
    );
    setActiveThreadSnapshot((current) =>
      current ? updateThreadReadState(current, ids, read) : current,
    );
    const results = await Promise.allSettled(
      targets.map((message) =>
        api.setRead(message.id, read, message.mailbox_uid_validity ?? null),
      ),
    );
    const failed = new Set(
      results.flatMap((result, index) =>
        result.status === "rejected" &&
        readMutationByMessageRef.current.get(previous[index].id) ===
          mutationGeneration
          ? [previous[index].id]
          : [],
      ),
    );
    if (failed.size) {
      const prior = new Map(previous.map((item) => [item.id, item.is_read]));
      const failedThreadIds = new Set(
        Object.values(previousSmartSections)
          .flatMap((section) => section.threads)
          .filter((item) =>
            concreteThreadMessages(item).some((message) =>
              failed.has(message.id),
            ),
          )
          .map((item) => item.id)
          .filter((id) => !mailboxActionThreadIdsRef.current.has(id)),
      );
      const failedRegularThreadIds = new Set(
        previousThreads
          .filter((item) =>
            concreteThreadMessages(item).some((message) =>
              failed.has(message.id),
            ),
          )
          .map((item) => item.id)
          .filter((id) => !mailboxActionThreadIdsRef.current.has(id)),
      );
      if (
        mutationCatalogueRevision === catalogueRevisionRef.current &&
        sameMailView(mutationView, currentViewRef.current)
      ) {
        setThreads((current) => {
          const currentIds = new Set(current.map((item) => item.id));
          const missingFailedThreadIds = new Set(
            [...failedRegularThreadIds].filter((id) => !currentIds.has(id)),
          );
          return restoreThreads(
            current,
            previousThreads,
            missingFailedThreadIds,
          ).map((item) => restoreThreadReadState(item, failed, prior));
        });
        setActive((current) =>
          current ? restoreMessageReadState(current, failed, prior) : current,
        );
        setActiveThreadSnapshot((current) =>
          current ? restoreThreadReadState(current, failed, prior) : current,
        );
        setSmartSections(
          (current) =>
            Object.fromEntries(
              smartSectionIds.map((id) => [
                id,
                {
                  ...current[id],
                  threads: (() => {
                    const currentIds = new Set(
                      current[id].threads.map((item) => item.id),
                    );
                    const missingFailedThreadIds = new Set(
                      [...failedThreadIds].filter(
                        (threadId) => !currentIds.has(threadId),
                      ),
                    );
                    return restoreThreads(
                      current[id].threads,
                      previousSmartSections[id].threads,
                      missingFailedThreadIds,
                    ).map((item) =>
                      restoreThreadReadState(item, failed, prior),
                    );
                  })(),
                },
              ]),
            ) as Record<SmartSectionId, SmartSection>,
        );
      }
      if (!silent) {
        showStatus(
          t(read ? "feedback.readFailed" : "feedback.unreadFailed", {
            count: failed.size,
          }),
          "error",
        );
      }
      for (const id of ids) {
        if (readMutationByMessageRef.current.get(id) === mutationGeneration)
          readMutationByMessageRef.current.delete(id);
      }
      return;
    }
    if (!silent) {
      showStatus(t(read ? "feedback.readSuccess" : "feedback.unreadSuccess"));
    }
    for (const id of ids) {
      if (readMutationByMessageRef.current.get(id) === mutationGeneration)
        readMutationByMessageRef.current.delete(id);
    }
    if (smartInboxActive) await loadMessages();
  };
  const trashMessagesFromSender = async (target: SenderCleanupTarget) => {
    if (senderCleanupInFlightRef.current) return;
    senderCleanupInFlightRef.current = true;
    senderCleanupTargetRef.current = target;
    const originalThreads = threadsRef.current;
    const originalSmartSections = smartSectionsRef.current;
    const originalRetainedSmartThreads = retainedSmartThreadsRef.current;
    const originalActive = activeRef.current;
    const originalActiveThreadSnapshot = activeThreadSnapshotRef.current;
    const originalSelected = new Set(selectedRef.current);
    const originalVisibleThreads = [
      ...originalThreads,
      ...smartSectionIds.flatMap((id) => originalSmartSections[id].threads),
    ];
    const optimisticSelected = new Set(originalSelected);
    for (const id of originalSelected) {
      const matchingThreads = originalVisibleThreads.filter(
        (thread) => thread.id === id,
      );
      if (
        matchingThreads.length > 0 &&
        matchingThreads.every(
          (thread) =>
            removeSenderCleanupMessages([thread], target).length === 0,
        )
      )
        optimisticSelected.delete(id);
    }
    const originalView = {
      ...currentViewRef.current,
      accountIds: [...currentViewRef.current.accountIds],
    };
    const restoreOptimisticMessages = () => {
      if (!sameMailView(originalView, currentViewRef.current)) return;
      setSelected((current) =>
        sameStringSet(current, optimisticSelected) ? originalSelected : current,
      );
      setThreads((current) =>
        restoreSenderCleanupMessages(current, originalThreads, target),
      );
      setSmartSections(
        (current) =>
          Object.fromEntries(
            smartSectionIds.map((id) => [
              id,
              {
                ...current[id],
                threads: restoreSenderCleanupMessages(
                  current[id].threads,
                  originalSmartSections[id].threads,
                  target,
                ),
              },
            ]),
          ) as Record<SmartSectionId, SmartSection>,
      );
      setRetainedSmartThreads((current) => {
        const next = new Map(current);
        for (const [id, retained] of originalRetainedSmartThreads) {
          if (
            concreteThreadMessages(retained.thread).some((message) =>
              isSenderCleanupMessage(message, target),
            )
          ) {
            const currentRetained = next.get(id);
            next.set(
              id,
              currentRetained
                ? {
                    ...currentRetained,
                    thread: restoreSenderCleanupMessages(
                      [currentRetained.thread],
                      [retained.thread],
                      target,
                    )[0],
                  }
                : retained,
            );
          }
        }
        return next;
      });
      setActive((current) => {
        if (current) return current;
        return originalActive && isSenderCleanupMessage(originalActive, target)
          ? originalActive
          : current;
      });
      setActiveThreadSnapshot((current) => {
        if (!originalActiveThreadSnapshot) return current;
        if (current && current.id !== originalActiveThreadSnapshot.id)
          return current;
        return restoreSenderCleanupMessages(
          current ? [current] : [],
          [originalActiveThreadSnapshot],
          target,
        )[0];
      });
    };
    setActionStatus(undefined);
    setSenderCleanupLoading(true);
    loadRequestIdRef.current += 1;
    smartLoadRequestIdRef.current += 1;
    setSelected(optimisticSelected);
    setThreads((current) => removeSenderCleanupMessages(current, target));
    setSmartSections(
      (current) =>
        Object.fromEntries(
          smartSectionIds.map((id) => [
            id,
            {
              ...current[id],
              threads: removeSenderCleanupMessages(current[id].threads, target),
            },
          ]),
        ) as Record<SmartSectionId, SmartSection>,
    );
    setRetainedSmartThreads((current) => {
      const next = new Map(current);
      for (const [id, retained] of next) {
        const thread = removeSenderCleanupMessages(
          [retained.thread],
          target,
        )[0];
        if (thread) next.set(id, { ...retained, thread });
        else next.delete(id);
      }
      return next;
    });
    const currentActiveThread = activeThreadRef.current;
    const remainingActiveThread = currentActiveThread
      ? removeSenderCleanupMessages([currentActiveThread], target)[0]
      : undefined;
    setActive((current) =>
      current && isSenderCleanupMessage(current, target)
        ? remainingActiveThread?.latest
        : current,
    );
    setActiveThreadSnapshot((current) =>
      current ? removeSenderCleanupMessages([current], target)[0] : current,
    );
    setAiResult(undefined);
    try {
      let result: TrashMessagesFromSenderResult;
      try {
        result = await api.trashMessagesFromSender(
          target.accountId,
          target.senderAddress,
        );
      } catch {
        senderCleanupTargetRef.current = undefined;
        restoreOptimisticMessages();
        try {
          await loadMessages();
        } catch (error) {
          console.warn("Could not reconcile sender cleanup", error);
        }
        showStatus(t("feedback.unsubscribeCleanupFailed"), "error");
        return;
      }
      senderCleanupTargetRef.current = undefined;
      if (
        result.failed > 0 ||
        result.moved === 0 ||
        result.moved !== result.matched
      )
        restoreOptimisticMessages();
      try {
        await loadMessages();
      } catch (error) {
        console.warn("Could not reconcile sender cleanup", error);
      }
      showStatus(
        senderCleanupOutcome(t, result),
        result.failed || result.moved !== result.matched ? "error" : "success",
      );
    } finally {
      senderCleanupTargetRef.current = undefined;
      senderCleanupInFlightRef.current = false;
      setSenderCleanupLoading(false);
    }
  };
  const unsubscribe = async (message: MailSummary) => {
    if (unsubscribeLoading) return;
    setUnsubscribeLoading(true);
    try {
      const result = await api.unsubscribe(
        message.id,
        message.mailbox_uid_validity ?? null,
      );
      showStatus(
        t(
          result.kind === "opened_web"
            ? "feedback.unsubscribeWeb"
            : "feedback.unsubscribeSuccess",
        ),
        "success",
        result.cleanupTarget
          ? {
              label: t("feedback.unsubscribeCleanupAction"),
              onAction: () =>
                void trashMessagesFromSender(result.cleanupTarget!),
              disabled: senderCleanupLoading,
            }
          : undefined,
      );
    } catch (error) {
      showStatus(
        error instanceof Error
          ? error.message
          : typeof error === "string"
            ? error
            : t("feedback.unsubscribeFailed"),
        "error",
      );
    } finally {
      setUnsubscribeLoading(false);
    }
  };
  const openCompose = () => {
    if (!accounts.length) {
      void showNativeMessage(
        t("composer.title"),
        t("composer.noAccount"),
        "warning",
      );
      void openAccountWindow();
      return;
    }
    openComposeWindow({ accountId: activeAccounts[0] ?? accounts[0].id });
  };
  const openFeedback = async () => {
    if (!accounts.length) {
      await showNativeMessage(
        t("composer.title"),
        t("composer.noAccount"),
        "warning",
      );
      await openAccountWindow();
      return;
    }
    openComposeWindow(
      await createFeedbackComposeSeed(
        activeAccounts[0] ?? accounts[0].id,
        i18n.resolvedLanguage ?? i18n.language,
      ),
    );
  };

  const configureTerminalCommand = async () => {
    try {
      const status = await api.terminalCommandStatus();
      if (status === "conflict") {
        await showNativeMessage(
          t("terminalCommand.conflictTitle"),
          t("terminalCommand.conflictBody"),
          "warning",
        );
        return;
      }
      if (status === "available") {
        const confirmed = await confirmNativeAction(
          t("terminalCommand.removeTitle"),
          t("terminalCommand.removeBody"),
          t("terminalCommand.removeAction"),
        );
        if (!confirmed) return;
        await api.removeTerminalCommand();
        await showNativeMessage(
          t("terminalCommand.removedTitle"),
          t("terminalCommand.removedBody"),
        );
        return;
      }
      const confirmed = await confirmNativeAction(
        t("terminalCommand.installTitle"),
        t("terminalCommand.installBody"),
        t("terminalCommand.installAction"),
      );
      if (!confirmed) return;
      await api.installTerminalCommand();
      await showNativeMessage(
        t("terminalCommand.installedTitle"),
        t("terminalCommand.installedBody"),
      );
    } catch (error) {
      await showNativeMessage(
        t("terminalCommand.errorTitle"),
        String(error),
        "error",
      );
    }
  };

  useHotkeys([
    ["mod+F", () => searchRef.current?.focus()],
    ["mod+K", () => searchRef.current?.focus()],
    ["mod+N", openCompose],
    ["mod+,", () => void openSettingsWindow()],
    ["mod+R", () => void openThreadReply()],
    ["mod+shift+F", () => void openThreadForward()],
    ["/", () => searchRef.current?.focus()],
    ["c", openCompose],
    ["e", () => void applyAction("archive")],
    ["shift+1", () => void applyAction("spam")],
  ]);
  useEffect(() => {
    let disposed = false;
    let dispose: () => void = () => undefined;
    void onNativeMenuAction((action) => {
      switch (action) {
        case "new-message":
          openCompose();
          break;
        case "add-account":
          void openAccountWindow();
          break;
        case "settings":
          void openSettingsWindow();
          break;
        case "check-for-updates":
          void runManualUpdateCheck();
          break;
        case "search":
          searchRef.current?.focus();
          break;
        case "sync":
          void sync();
          break;
        case "reply":
          void openThreadReply();
          break;
        case "forward":
          void openThreadForward();
          break;
        case "archive":
          void applyAction("archive");
          break;
        case "spam":
          void applyAction("spam");
          break;
        case "keyboard-shortcuts":
          void showNativeMessage(
            t("shortcuts.title"),
            [
              `⌘N  ${t("shortcuts.compose")}`,
              `⌘F  ${t("shortcuts.search")}`,
              `⌘R  ${t("actions.reply")}`,
              `⇧⌘F  ${t("actions.forward")}`,
              `⇧⌘A  ${t("shortcuts.archive")}`,
              `⇧⌘J  ${t("shortcuts.spam")}`,
              `⇧⌫  ${t("shortcuts.permanentlyDelete")}`,
              `⌘,  ${t("settings.title")}`,
            ].join("\n"),
          );
          break;
        case "terminal-command":
          void configureTerminalCommand();
          break;
        case "copy-email-address-failed":
          void showNativeMessage(
            t("errors.generic"),
            t("errors.copyFailed"),
            "error",
          );
          break;
        default:
          {
            const addressAction = parseEmailAddressMenuAction(action);
            if (
              addressAction?.kind === "compose" &&
              accounts.some((account) => account.id === addressAction.accountId)
            ) {
              openComposeWindow({
                accountId: addressAction.accountId,
                to: addressAction.address,
              });
              break;
            }
          }
          if (action.startsWith("rename-account:")) {
            const accountId = action.slice("rename-account:".length);
            if (accounts.some((account) => account.id === accountId)) {
              void openSettingsWindowForAccount(accountId);
            }
          }
      }
    }).then((unlisten) => {
      if (disposed) {
        unlisten();
      } else {
        dispose = unlisten;
      }
    });
    return () => {
      disposed = true;
      dispose();
    };
  }, [
    accounts,
    applyAction,
    configureTerminalCommand,
    openCompose,
    openThreadReply,
    openThreadForward,
    runManualUpdateCheck,
    sync,
    t,
  ]);

  const selectedAccount = accounts.find(
    (account) => account.id === selectedAccountId,
  );
  const toggleStar = async (message: MailSummary, starred: boolean) => {
    try {
      const updated = await api.setStarred(
        message.id,
        starred,
        message.mailbox_uid_validity ?? null,
      );
      const update = (item: MailSummary) =>
        item.id === message.id
          ? { ...item, ...updated, is_flagged: starred }
          : item;
      setThreads((current) =>
        current.map((thread) => ({
          ...thread,
          messages: thread.messages.map(update),
          latest: update(thread.latest),
        })),
      );
      setActive((current) => (current ? update(current) : current));
      setActiveThreadSnapshot((current) =>
        current
          ? {
              ...current,
              messages: current.messages.map(update),
              latest: update(current.latest),
            }
          : current,
      );
      await refreshStarredCount(activeAccounts);
      await loadMessages();
    } catch (error) {
      showError(error);
    }
  };
  const mailboxTitle =
    mailbox === "INBOX"
      ? selectedAccount?.account_name ||
        selectedAccount?.email ||
        t("nav.inbox")
      : mailbox === ""
        ? t("nav.allMail")
        : mailbox === "unread"
          ? t("nav.unread")
          : mailbox === "starred"
            ? t("nav.starred")
            : mailbox;
  const analyticsConsentDialog = (
    <AnalyticsConsentDialog
      accounts={accounts}
      opened={accountsLoaded && analytics.consent === "unknown"}
      onChoose={(enabled) => setAnalytics(setAnalyticsConsent(enabled))}
    />
  );
  if (!loading && accounts.length === 0)
    return (
      <div className="app-shell">
        <WindowDragRegion />
        {updateState ? (
          <UpdateBanner
            state={updateState}
            onDownload={() => void startUpdateDownload()}
            onInstall={() => void installUpdate()}
            onDismiss={() => setUpdateState(undefined)}
          />
        ) : null}
        <MailboxNav
          accounts={accounts}
          selectedAccountId={selectedAccountId}
          mailbox={mailbox}
          onSelectAccount={selectAccount}
          onAccountContextMenu={(account) =>
            void api
              .showAccountContextMenu(account.id, t("actions.renameAccount"))
              .catch((error) =>
                showNativeMessage(t("errors.generic"), String(error), "error"),
              )
          }
          onAddAccount={() => void openAccountWindow()}
          onMailbox={selectMailbox}
          onFeedback={() => void openFeedback()}
          feedbackDisabled={loading}
          outboxSendingCount={visibleOutbox.length}
          outboxRecoveryCount={recoveryCount}
          starredCount={starredCount}
        />
        <section className="mail-list-panel" />
        <main className="reader">
          <EmptyState
            title={t("empty.title")}
            body={t("empty.body")}
            action={t("empty.cta")}
            onAction={() => void openAccountWindow()}
          />
        </main>
        {analyticsConsentDialog}
      </div>
    );

  return (
    <div className="app-shell">
      <WindowDragRegion />
      {updateState ? (
        <UpdateBanner
          state={updateState}
          onDownload={() => void startUpdateDownload()}
          onInstall={() => void installUpdate()}
          onDismiss={() => setUpdateState(undefined)}
        />
      ) : null}
      <MailboxNav
        accounts={accounts}
        selectedAccountId={selectedAccountId}
        mailbox={mailbox}
        onSelectAccount={selectAccount}
        onAccountContextMenu={(account) =>
          void api
            .showAccountContextMenu(account.id, t("actions.renameAccount"))
            .catch((error) =>
              showNativeMessage(t("errors.generic"), String(error), "error"),
            )
        }
        onAddAccount={() => void openAccountWindow()}
        onMailbox={selectMailbox}
        onFeedback={() => void openFeedback()}
        feedbackDisabled={loading}
        outboxSendingCount={visibleOutbox.length}
        outboxRecoveryCount={recoveryCount}
        starredCount={starredCount}
      />
      <MailList
        threads={displayedThreads}
        supplementaryContent={
          mailbox === "Outbox" ? (
            <SendRecoveryList
              items={visibleRecoveries}
              accountNames={Object.fromEntries(
                accounts.map((account) => [
                  account.id,
                  account.account_name || account.email,
                ]),
              )}
              loading={recoveries.loading}
              failed={recoveries.failed}
              hasMore={Boolean(recoveries.nextCursor)}
              onOpen={(item) => {
                void api
                  .openSendRecoveryWindow(item.account_id, item.attempt_id)
                  .catch(showError);
              }}
              onRetry={() => void refreshSendRecoveries()}
              onLoadMore={recoveries.loadMore}
            />
          ) : undefined
        }
        suppressEmptyState={
          mailbox === "Outbox" &&
          (recoveries.items.length > 0 ||
            recoveries.total > 0 ||
            Boolean(recoveries.nextCursor) ||
            recoveries.loading ||
            recoveries.failed)
        }
        activeThreadId={activeThread?.id}
        selected={selected}
        query={query}
        loading={loading}
        loadingMore={loadingMore}
        hasMore={hasMore}
        remoteSearchUnavailable={remoteSearchUnavailable}
        remoteSearchLimitReached={remoteSearchLimitReached}
        syncStatus={syncStatus}
        classifying={classifying}
        lastSyncAt={lastSyncAt}
        aiConnected={aiConnected}
        mailboxTitle={mailboxTitle}
        view={mailListView}
        smartInbox={smartInboxActive}
        smartSections={smartSectionIds.map((id) => {
          const section = smartSections[id];
          const retainedEntries = [...retainedSmartThreads.values()];
          const retained = retainedEntries
            .filter((item) => item.sectionId === id)
            .map((item) => item.thread);
          const retainedInOtherSections = new Set(
            retainedEntries
              .filter((item) => item.sectionId !== id)
              .map((item) => item.thread.id),
          );
          const authoritativeThreads =
            id === "starred"
              ? section.threads
              : section.threads.filter(
                  (thread) => !retainedInOtherSections.has(thread.id),
                );
          return {
            ...section,
            threads: retained.length
              ? mergeThreads(authoritativeThreads, retained)
              : authoritativeThreads,
          };
        })}
        exitingThreadIds={exitingSmartThreadIds}
        onViewChange={changeMailListView}
        onCategorize={(message, category) => {
          void api
            .setCategory(
              message.id,
              category,
              message.mailbox_uid_validity ?? null,
            )
            .then(() => {
              const updateCategory = (item: MailSummary) =>
                item.id === message.id
                  ? {
                      ...item,
                      category,
                      classification_confidence: 1,
                      classification_source: "user" as const,
                    }
                  : item;
              setThreads((current) =>
                current.map((thread) => ({
                  ...thread,
                  messages: thread.messages.map(updateCategory),
                  latest: updateCategory(thread.latest),
                })),
              );
              setActive((current) =>
                current ? updateCategory(current) : current,
              );
              setActiveThreadSnapshot((current) =>
                current
                  ? {
                      ...current,
                      messages: current.messages.map(updateCategory),
                      latest: updateCategory(current.latest),
                    }
                  : current,
              );
              showStatus(t("feedback.categorySaved"));
              void loadMessages();
            })
            .catch(showError);
        }}
        onToggleStar={(message, starred) => void toggleStar(message, starred)}
        onQuery={setQuery}
        onOpen={(thread) => {
          const previousThread = activeThread;
          if (smartInboxActive) {
            const sectionId = smartSectionIds.find((id) =>
              smartSections[id].threads.some((item) => item.id === thread.id),
            );
            if (sectionId && !["starred", "seen"].includes(sectionId)) {
              setRetainedSmartThreads((current) => {
                const next = new Map(current);
                next.set(thread.id, { sectionId, thread });
                return next;
              });
            }
          }
          if (
            smartInboxActive &&
            previousThread &&
            previousThread.id !== thread.id &&
            !previousThread.unread
          ) {
            const previousId = previousThread.id;
            setExitingSmartThreadIds((current) =>
              new Set(current).add(previousId),
            );
            const existingTimer = smartExitTimersRef.current.get(previousId);
            if (existingTimer) window.clearTimeout(existingTimer);
            smartExitTimersRef.current.set(
              previousId,
              window.setTimeout(() => {
                setSmartSections(
                  (current) =>
                    Object.fromEntries(
                      smartSectionIds.map((id) => [
                        id,
                        {
                          ...current[id],
                          threads: ["starred", "seen"].includes(id)
                            ? current[id].threads
                            : current[id].threads.filter(
                                (item) => item.id !== previousId,
                              ),
                        },
                      ]),
                    ) as Record<SmartSectionId, SmartSection>,
                );
                setExitingSmartThreadIds((current) => {
                  const next = new Set(current);
                  next.delete(previousId);
                  return next;
                });
                setRetainedSmartThreads((current) => {
                  const next = new Map(current);
                  next.delete(previousId);
                  return next;
                });
                smartExitTimersRef.current.delete(previousId);
              }, 180),
            );
          }
          setActive(thread.latest);
          setActiveThreadSnapshot(thread);
          setAiResult(undefined);
          void setThreadReadState(thread, true, { silent: true });
        }}
        onDoubleOpen={(thread) => {
          const focused = thread.latest;
          const seed: ReaderWindowSeed = {
            target: {
              accountId: focused.account_id,
              localMessageId: focused.id,
              rfcMessageId: focused.message_id ?? undefined,
              threadId: thread.threadId ?? focused.thread_id,
              mailbox,
            },
            focusedMessageId: focused.id,
          };
          void openReaderWindow(seed).catch(showError);
        }}
        onSelect={select}
        onSync={() => void sync()}
        onCompose={openCompose}
        onArchive={() => void applyAction("archive")}
        onSpam={() => void applyAction("spam")}
        onReplyThread={(thread) => void openThreadReply(thread)}
        onForwardThread={(thread) => void openThreadForward(thread)}
        onActionThread={(thread, action) => void applyAction(action, [thread])}
        onToggleReadThread={(thread, read) =>
          void setThreadReadState(thread, read)
        }
        onToggleStarThread={(thread, flagged) =>
          void toggleThreadStar(thread, flagged)
        }
        onSummarize={() => void runSummary()}
        onLoadMore={() => void loadMoreMessages()}
        onLoadMoreSmart={(id) => void loadMoreSmartSection(id)}
        pendingActions={pendingActions}
        actionsDisabled={permanentDeleteLoading}
        searchRef={searchRef}
      />
      <Reader
        message={active}
        messages={activeThread?.messages}
        accountEmail={
          accounts.find((account) => account.id === active?.account_id)?.email
        }
        aiResult={aiResult}
        aiLoading={aiLoading}
        aiConnected={aiConnected}
        actionsDisabled={permanentDeleteLoading}
        onArchive={() => void applyAction("archive")}
        onSpam={() =>
          void applyAction(
            active?.mailbox.split("::", 1)[0] === "Spam" ? "not_spam" : "spam",
          )
        }
        onTrash={() => void applyAction("trash")}
        onPermanentDelete={permanentlyDeleteMessage}
        onReply={(message) => void openReplyForMessage(message)}
        onReplyAll={(message) => void openReplyForMessage(message, true)}
        onForward={(message) => void openForwardForMessage(message)}
        onSendAgain={(message) => void openSendAgainForMessage(message)}
        onToggleRead={(read) =>
          activeThread ? void setThreadReadState(activeThread, read) : undefined
        }
        onSummarize={() => void runSummary()}
        onCopyAi={() =>
          aiResult &&
          navigator.clipboard
            .writeText(aiResult)
            .catch(() =>
              showNativeMessage(
                t("errors.generic"),
                t("errors.copyFailed"),
                "error",
              ),
            )
        }
        onComposeTo={(message, address) =>
          openComposeWindow({ accountId: message.account_id, to: address })
        }
        onComposeLink={(message, seed) =>
          openComposeWindow({ ...seed, accountId: message.account_id })
        }
        onAddressContextMenu={(message, address) =>
          void api
            .showEmailAddressContextMenu(
              message.account_id,
              address,
              t("actions.copy"),
              t("reader.newMessageTo", { address }),
            )
            .catch((error) =>
              showNativeMessage(t("errors.generic"), String(error), "error"),
            )
        }
        unsubscribeLoading={unsubscribeLoading}
        onUnsubscribe={(message) => void unsubscribe(message)}
        onToggleStar={(message, starred) => void toggleStar(message, starred)}
      />
      {actionStatus ? (
        <ActionStatus
          key={actionStatus.id}
          message={actionStatus.message}
          tone={actionStatus.tone}
          action={actionStatus.action}
          dismiss={{
            label: t("actions.close"),
            onDismiss: () => setActionStatus(undefined),
          }}
        />
      ) : null}
      {analyticsConsentDialog}
    </div>
  );

  function showError(error: unknown, title = t("errors.generic")) {
    void showNativeMessage(
      title,
      error instanceof Error ? error.message : String(error),
      "error",
    );
  }
}

function actionOutcome(
  t: ReturnType<typeof useTranslation>["t"],
  action: MailAction,
  succeeded: number,
  failed: number,
) {
  const key = {
    archive: "archive",
    spam: "spam",
    not_spam: "notSpam",
    trash: "trash",
  }[action];
  if (failed && succeeded) {
    return t(`feedback.${key}Partial`, { succeeded, failed });
  }
  if (failed) {
    return t(`feedback.${key}Failed`, { count: failed });
  }
  return t(`feedback.${key}Success`, { count: succeeded });
}

function mergeThreads(current: MailThread[], incoming: MailThread[]) {
  const merged = new Map(current.map((thread) => [thread.id, thread]));
  for (const thread of incoming) merged.set(thread.id, thread);
  return [...merged.values()].sort(
    (left, right) =>
      new Date(right.latest.received_at).getTime() -
      new Date(left.latest.received_at).getTime(),
  );
}

function mergeFirstPageThreads(
  current: MailThread[],
  incoming: MailThread[],
  pendingReadMutations: Map<string, number>,
  pendingStarMutations: Map<string, number>,
) {
  const merged = new Map(current.map((thread) => [thread.id, thread]));
  for (const incomingThread of incoming) {
    const currentThread = merged.get(incomingThread.id);
    if (!currentThread) {
      merged.set(incomingThread.id, incomingThread);
      continue;
    }

    const currentMessages = concreteThreadMessages(currentThread);
    const currentById = new Map(
      currentMessages.map((message) => [message.id, message]),
    );
    const messagesByLocator = new Map(
      currentMessages.map((message) => [messageLocator(message), message]),
    );
    for (const incomingMessage of concreteThreadMessages(incomingThread)) {
      const currentWithId = currentById.get(incomingMessage.id);
      if (
        currentWithId &&
        messageLocator(currentWithId) !== messageLocator(incomingMessage)
      ) {
        // A local ID must never be rebound to a different remote action target.
        continue;
      }
      const currentMessage =
        currentWithId ?? messagesByLocator.get(messageLocator(incomingMessage));
      if (!currentMessage) {
        messagesByLocator.set(messageLocator(incomingMessage), incomingMessage);
        continue;
      }
      messagesByLocator.set(messageLocator(currentMessage), {
        ...currentMessage,
        ...incomingMessage,
        id: currentMessage.id,
        account_id: currentMessage.account_id,
        mailbox: currentMessage.mailbox,
        uid: currentMessage.uid,
        mailbox_uid_validity: currentMessage.mailbox_uid_validity,
        is_read: pendingReadMutations.has(currentMessage.id)
          ? currentMessage.is_read
          : incomingMessage.is_read,
        is_flagged: pendingStarMutations.has(currentMessage.id)
          ? currentMessage.is_flagged
          : incomingMessage.is_flagged,
      });
    }

    const rebuilt = groupMessages([...messagesByLocator.values()]).find(
      (thread) => thread.id === incomingThread.id,
    );
    if (!rebuilt) continue;
    const knownCount = Math.max(
      currentThread.messageCount ?? currentMessages.length,
      incomingThread.messageCount ??
        concreteThreadMessages(incomingThread).length,
      concreteThreadMessages(rebuilt).length,
    );
    merged.set(incomingThread.id, {
      ...currentThread,
      ...incomingThread,
      ...rebuilt,
      messageCount: knownCount,
    });
  }
  return [...merged.values()].sort(
    (left, right) =>
      new Date(right.latest.received_at).getTime() -
      new Date(left.latest.received_at).getTime(),
  );
}

function messageLocator(message: MailSummary) {
  return JSON.stringify([
    message.account_id,
    message.mailbox,
    message.mailbox_uid_validity ?? null,
    message.uid,
  ]);
}

function excludeThreads(threads: MailThread[], excludedIds: Set<string>) {
  return threads.filter((thread) => !excludedIds.has(thread.id));
}

function updateMessageReadState(
  message: MailSummary,
  ids: Set<string>,
  read: boolean,
) {
  return ids.has(message.id) ? { ...message, is_read: read } : message;
}

function updateThreadReadState(
  thread: MailThread,
  ids: Set<string>,
  read: boolean,
) {
  const messages = thread.messages.map((message) =>
    updateMessageReadState(message, ids, read),
  );
  const sourceMessages = thread.sourceMessages?.map((message) =>
    updateMessageReadState(message, ids, read),
  );
  const latest =
    messages.find((message) => message.id === thread.latest.id) ??
    thread.latest;
  return {
    ...thread,
    messages,
    sourceMessages,
    latest,
    unread: (sourceMessages ?? messages).some((message) => !message.is_read),
  };
}

function restoreMessageReadState(
  message: MailSummary,
  failed: Set<string>,
  previous: Map<string, boolean>,
) {
  return failed.has(message.id)
    ? { ...message, is_read: previous.get(message.id) ?? message.is_read }
    : message;
}

function restoreThreadReadState(
  thread: MailThread,
  failed: Set<string>,
  previous: Map<string, boolean>,
) {
  const messages = thread.messages.map((message) =>
    restoreMessageReadState(message, failed, previous),
  );
  const sourceMessages = thread.sourceMessages?.map((message) =>
    restoreMessageReadState(message, failed, previous),
  );
  const latest =
    messages.find((message) => message.id === thread.latest.id) ??
    thread.latest;
  return {
    ...thread,
    messages,
    sourceMessages,
    latest,
    unread: (sourceMessages ?? messages).some((message) => !message.is_read),
  };
}

const wait = (milliseconds: number) =>
  new Promise<void>((resolve) => window.setTimeout(resolve, milliseconds));

function WindowDragRegion() {
  const startDragging = (event: React.MouseEvent<HTMLDivElement>) => {
    const tauriWindow = window as Window & { __TAURI_INTERNALS__?: unknown };
    if (event.button !== 0 || !tauriWindow.__TAURI_INTERNALS__) return;
    void getCurrentWindow().startDragging();
  };
  return (
    <div
      className="window-drag-region"
      data-tauri-drag-region
      onMouseDown={startDragging}
      aria-hidden="true"
    />
  );
}

async function syncAccounts(
  accounts: Account[],
  onProgress: (status: SyncStatus) => void,
  onBatchPublished?: () => void,
  full = false,
) {
  const result: SyncResult = { syncedCount: 0, newMessages: [] };
  for (const [index, account] of accounts.entries()) {
    onProgress({
      phase: "connecting",
      completed: 0,
      total: null,
      accountEmail: account.email,
      accountIndex: index + 1,
      accountCount: accounts.length,
    });
    const accountResult = await api.sync(
      account.id,
      (progress) => {
        onProgress({
          ...progress,
          accountEmail: account.email,
          accountIndex: index + 1,
          accountCount: accounts.length,
        });
        if (progress.phase === "saving") onBatchPublished?.();
      },
      full,
    );
    result.syncedCount += accountResult.syncedCount;
    result.newMessages.push(...accountResult.newMessages);
  }
  return result;
}

function notificationCopy(t: TFunction) {
  return {
    newMail: t("notifications.newMail"),
    oneGeneric: t("notifications.oneGeneric"),
    many: (count: number) => t("notifications.many", { count }),
    manyBody: (count: number) => t("notifications.manyBody", { count }),
  };
}

function readJson<T>(key: string, fallback: T): T {
  try {
    const value = localStorage.getItem(key);
    return value ? { ...fallback, ...JSON.parse(value) } : fallback;
  } catch {
    return fallback;
  }
}
