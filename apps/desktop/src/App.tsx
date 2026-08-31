import { useDebouncedValue, useHotkeys } from "@mantine/hooks";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api } from "./api";
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
import { forwardBody, forwardSubject } from "./forward";
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
  onMailArrived,
  onMailChanged,
  onMailHydrated,
  onMailIndexRebuilt,
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

function emptySmartSections(): Record<SmartSectionId, SmartSection> {
  return smartSectionIds.reduce<Record<SmartSectionId, SmartSection>>(
    (sections, id) => {
      sections[id] = { id, threads: [], nextCursor: null, loadingMore: false };
      return sections;
    },
    {} as Record<SmartSectionId, SmartSection>,
  );
}

export default function App() {
  const { t, i18n } = useTranslation();
  const [accounts, setAccounts] = useState<Account[]>([]);
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
  const [actionStatus, setActionStatus] = useState<{
    id: number;
    message: string;
    tone: "success" | "error";
  }>();
  const [updateState, setUpdateState] = useState<UpdateBannerState>();
  const searchRef = useRef<HTMLInputElement>(null);
  const statusId = useRef(0);
  const actionBusyRef = useRef(false);
  const mailboxActionsInFlightRef = useRef(0);
  const mailboxActionThreadIdsRef = useRef(new Set<string>());
  const readMutationGenerationRef = useRef(0);
  const readMutationByMessageRef = useRef(new Map<string, number>());
  const loadRequestIdRef = useRef(0);
  const smartLoadRequestIdRef = useRef(0);
  const starredCountRequestIdRef = useRef(0);
  const nextCursorRef = useRef<MailCursor | null>(null);
  const loadingMoreRef = useRef(false);
  const smartLoadingMoreRef = useRef(new Set<SmartSectionId>());
  const initialClassificationStartedRef = useRef(false);
  const classifyingRef = useRef(false);
  const classificationRequestedRef = useRef(false);
  const manualUpdateCheckInFlightRef = useRef(false);
  const smartExitTimersRef = useRef(new Map<string, number>());
  const accountsRef = useRef<Account[]>([]);
  const selectedAccountIdRef = useRef<string | undefined>(undefined);
  const removedAccountIdsRef = useRef(new Set<string>());
  const accountStateGenerationRef = useRef(0);

  accountsRef.current = accounts;
  selectedAccountIdRef.current = selectedAccountId;

  useEffect(
    () => () => {
      smartExitTimersRef.current.forEach((timer) => window.clearTimeout(timer));
    },
    [],
  );

  const showStatus = useCallback(
    (message: string, tone: "success" | "error" = "success") => {
      statusId.current += 1;
      setActionStatus({ id: statusId.current, message, tone });
    },
    [],
  );

  useEffect(() => {
    if (!actionStatus) return;
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
  const activeAccounts = useMemo(
    () =>
      selectedAccountId
        ? [selectedAccountId]
        : accounts.map((account) => account.id),
    [accounts, selectedAccountId],
  );
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
    const requestId = ++loadRequestIdRef.current;
    const smartRequestId = ++smartLoadRequestIdRef.current;
    const currentView = {
      ...currentViewRef.current,
      accountIds: requestedAccountIds ?? currentViewRef.current.accountIds,
    };
    const accountIds = currentView.accountIds;
    const smartInbox =
      currentView.view === "smart" &&
      currentView.mailbox === "INBOX" &&
      !currentView.query.trim();
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
          setSmartSections(() => {
            const next = emptySmartSections();
            for (const section of page.sections) {
              next[section.id] = {
                id: section.id,
                threads: excludeThreads(
                  section.conversations,
                  mailboxActionThreadIdsRef.current,
                ),
                nextCursor: section.nextCursor,
                loadingMore: false,
              };
            }
            return next;
          });
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
        requestId === loadRequestIdRef.current &&
        sameMailView(currentView, currentViewRef.current)
      ) {
        nextCursorRef.current = page.nextCursor;
        setThreads(
          excludeThreads(page.conversations, mailboxActionThreadIdsRef.current),
        );
        setHasMore(page.nextCursor !== null);
      }
      if (currentView.query.trim()) {
        try {
          const remote = await api.searchRemote(
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
            const merged = new Map(
              page.conversations.map((thread) => [thread.id, thread]),
            );
            for (const thread of groupMessages(remote)) {
              if (!merged.has(thread.id)) merged.set(thread.id, thread);
            }
            setThreads(
              excludeThreads(
                [...merged.values()].sort(
                  (left, right) =>
                    new Date(right.latest.received_at).getTime() -
                    new Date(left.latest.received_at).getTime(),
                ),
                mailboxActionThreadIdsRef.current,
              ),
            );
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
  const loadMoreMessages = useCallback(async () => {
    if (loadingMoreRef.current || !hasMore) return;
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
      setHasMore(page.nextCursor !== null);
      setThreads((current) => mergeThreads(current, page.conversations));
    } catch (error) {
      if (
        requestId === loadRequestIdRef.current &&
        sameMailView(currentView, currentViewRef.current)
      )
        showError(error);
    } finally {
      loadingMoreRef.current = false;
      if (
        requestId === loadRequestIdRef.current &&
        sameMailView(currentView, currentViewRef.current)
      )
        setLoadingMore(false);
    }
  }, [hasMore]);
  const loadMoreSmartSection = useCallback(
    async (id: SmartSectionId) => {
      const currentView = currentViewRef.current;
      const section = smartSections[id];
      if (
        smartLoadingMoreRef.current.has(id) ||
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
          !["starred", "seen"].includes(id),
          id === "seen",
        );
        if (
          requestId !== smartLoadRequestIdRef.current ||
          !sameMailView(currentView, currentViewRef.current)
        )
          return;
        setSmartSections((current) => ({
          ...current,
          [id]: {
            ...current[id],
            threads: mergeThreads(current[id].threads, page.conversations),
            nextCursor: page.nextCursor,
            loadingMore: false,
          },
        }));
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
    void onAccountConnected((account) => {
      accountStateGenerationRef.current += 1;
      setAccounts((current) => {
        const next = [
          ...current.filter((item) => item.id !== account.id),
          account,
        ];
        accountsRef.current = next;
        return next;
      });
      void syncAccounts(
        [account],
        setSyncStatus,
        () => void loadMessages(),
        true,
      )
        .then(async () => {
          markSynced();
          await requestInitialNotificationAccess(notificationSettings);
          await loadMessages(
            selectedAccountId
              ? [selectedAccountId]
              : [...new Set([...activeAccounts, account.id])],
          );
          void classifyPending();
        })
        .catch(showError)
        .finally(() => setSyncStatus(undefined));
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
    void Promise.all([
      onMailArrived((arrival) => {
        void (async () => {
          markSynced();
          await loadMessages();
          await requestInitialNotificationAccess(notificationSettings);
          if (!(await getCurrentWindow().isFocused())) {
            const delivered = await sendNewMailNotification(
              arrival.messages,
              notificationSettings,
              notificationCopy(t),
            );
            if (delivered) {
              await api.recordNotificationDelivered(
                arrival.accountId,
                arrival.eventId,
                arrival.detectedAt,
              );
            }
          }
        })().catch(showError);
      }),
      onMailHydrated(() => {
        void classifyPending();
      }),
      onMailChanged(() => {
        void loadMessages().catch(showError);
      }),
      onMailSyncState(() => undefined),
    ])
      .then(async (listeners) => {
        if (disposed) {
          listeners.forEach((unlisten) => unlisten());
          return;
        }
        unlisteners.push(...listeners);
        await api.configureTray(t("tray.open"), t("tray.quit"));
        await api.startRealtimeSync();
      })
      .catch(showError);
    return () => {
      disposed = true;
      unlisteners.forEach((unlisten) => unlisten());
    };
  }, [classifyPending, loadMessages, markSynced, notificationSettings, t]);

  const smartInboxActive =
    mailListView === "smart" && mailbox === "INBOX" && !query.trim();
  const smartThreads = useMemo(
    () => smartSectionIds.flatMap((id) => smartSections[id].threads),
    [smartSections],
  );
  const displayedThreads =
    mailbox === "Outbox"
      ? groupMessages(outbox)
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
    let dispose: () => void = () => undefined;
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
    void Promise.all([
      onNotificationAction(openNotification).then((listener) =>
        typeof listener === "function" ? listener : () => listener.unregister(),
      ),
      onDesktopNotificationAction(openNotification),
    ]).then((listeners) => {
      const unlistenAll = () => listeners.forEach((unlisten) => unlisten());
      if (disposed) unlistenAll();
      else dispose = unlistenAll;
    });
    return () => {
      disposed = true;
      dispose();
    };
  }, [accounts]);

  useEffect(() => {
    let dispose: () => void = () => undefined;
    let disposed = false;
    void Promise.all([
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
    ]).then((unlisteners) => {
      const unlistenAll = () => unlisteners.forEach((unlisten) => unlisten());
      if (disposed) unlistenAll();
      else dispose = unlistenAll;
    });
    return () => {
      disposed = true;
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
    mailboxActionsInFlightRef.current += 1;
    // An open/read transition may already be refreshing Smart sections with
    // the pre-action INBOX row. Make that result stale before moving the row.
    loadRequestIdRef.current += 1;
    smartLoadRequestIdRef.current += 1;
    const originalThreads = [...displayedThreads];
    const originalSmartSections = smartSections;
    const originalRetainedSmartThreads = retainedSmartThreads;
    const actionTargets = actionThreads.flatMap((target) => target.messages);
    const targetIds = new Set(actionTargets.map((message) => message.id));
    const targetThreadIds = new Set(
      actionThreads.map((target) => target.thread.id),
    );
    for (const id of targetThreadIds) mailboxActionThreadIdsRef.current.add(id);
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
            ),
          ),
        ),
      ),
    );
    const results = await resultsPromise;
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
      setSelected(new Set(failedThreadIds));
      setThreads((current) =>
        restoreThreads(current, originalThreads, failedThreadIds),
      );
      setSmartSections(
        (current) =>
          Object.fromEntries(
            smartSectionIds.map((id) => [
              id,
              {
                ...current[id],
                threads: restoreThreads(
                  current[id].threads,
                  originalSmartSections[id].threads,
                  failedThreadIds,
                ),
              },
            ]),
          ) as Record<SmartSectionId, SmartSection>,
      );
      setRetainedSmartThreads((current) => {
        const next = new Map(current);
        for (const id of failedThreadIds) {
          const retained = originalRetainedSmartThreads.get(id);
          if (retained) next.set(id, retained);
        }
        return next;
      });
      showStatus(
        actionOutcome(t, action, succeeded, failedThreadIds.size),
        "error",
      );
      await wait(reducedMotion ? 0 : 260);
    } else {
      showStatus(actionOutcome(t, action, succeeded, 0));
    }

    setPendingActions((current) => {
      const next = { ...current };
      for (const id of targetIds) delete next[id];
      return next;
    });
    mailboxActionsInFlightRef.current -= 1;
    for (const id of targetThreadIds)
      mailboxActionThreadIdsRef.current.delete(id);
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
          targets.map((item) => item.id),
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
      const content = await api.content(replyMessage.id);
      const replyHistory = formatReplyHistory({
        message: replyMessage,
        bodyText: content.body_text,
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
      const content = await api.content(message.id);
      openComposeWindow({
        accountId: message.account_id,
        subject: forwardSubject(message.subject, t("reader.forwardPrefix")),
        body: forwardBody(message, content, {
          originalMessage: t("reader.originalMessage"),
          from: t("composer.from"),
          date: t("reader.date"),
          subject: t("composer.subject"),
          to: t("composer.to"),
        }),
        forwardMessageId: content.attachments.some(
          (attachment) => attachment.presentation !== "embedded",
        )
          ? message.id
          : undefined,
        contextMessageIds: [message.id],
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
  const toggleThreadStar = async (thread: MailThread, flagged: boolean) => {
    if (actionBusyRef.current) return;
    actionBusyRef.current = true;
    const sourceMessages = concreteThreadMessages(thread);
    const previous = sourceMessages.map((message) => ({
      id: message.id,
      is_flagged: message.is_flagged,
    }));
    const ids = new Set(previous.map((message) => message.id));
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
      sourceMessages.map((message) => api.setStarred(message.id, flagged)),
    );
    const failed = new Set(
      results.flatMap((result, index) =>
        result.status === "rejected" ? [previous[index].id] : [],
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
      targets.map((message) => api.setRead(message.id, read)),
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
      setThreads((current) =>
        restoreThreads(current, previousThreads, failedRegularThreadIds).map(
          (item) => restoreThreadReadState(item, failed, prior),
        ),
      );
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
                threads: restoreThreads(
                  current[id].threads,
                  previousSmartSections[id].threads,
                  failedThreadIds,
                ).map((item) => restoreThreadReadState(item, failed, prior)),
              },
            ]),
          ) as Record<SmartSectionId, SmartSection>,
      );
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
  const unsubscribe = async (message: MailSummary) => {
    if (unsubscribeLoading) return;
    setUnsubscribeLoading(true);
    try {
      const result = await api.unsubscribe(message.id);
      if (result.kind === "opened_web") {
        showStatus(t("feedback.unsubscribeWeb"));
      } else {
        showStatus(t("feedback.unsubscribeSuccess"));
      }
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
      const updated = await api.setStarred(message.id, starred);
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
          outboxCount={outbox.length}
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
        outboxCount={outbox.length}
        starredCount={starredCount}
      />
      <MailList
        threads={displayedThreads}
        activeThreadId={activeThread?.id}
        selected={selected}
        query={query}
        loading={loading}
        loadingMore={loadingMore}
        hasMore={hasMore}
        remoteSearchUnavailable={remoteSearchUnavailable}
        syncStatus={syncStatus}
        classifying={classifying}
        lastSyncAt={lastSyncAt}
        aiConnected={aiConnected}
        mailboxTitle={mailboxTitle}
        view={mailListView}
        smartInbox={smartInboxActive}
        smartSections={smartSectionIds.map((id) => {
          const section = smartSections[id];
          const retained = [...retainedSmartThreads.values()]
            .filter((item) => item.sectionId === id)
            .map((item) => item.thread)
            .filter(
              (thread) =>
                !section.threads.some((item) => item.id === thread.id),
            );
          return retained.length
            ? { ...section, threads: mergeThreads(section.threads, retained) }
            : section;
        })}
        exitingThreadIds={exitingSmartThreadIds}
        onViewChange={changeMailListView}
        onCategorize={(message, category) => {
          void api
            .setCategory(message.id, category)
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
        />
      ) : null}
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
