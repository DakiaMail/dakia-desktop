import { useDebouncedValue, useHotkeys } from "@mantine/hooks";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { api } from "./api";
import { AI_FEATURES_VISIBLE } from "./features";
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
  restoreThreads,
  type MailAction,
  type PendingMailActions,
} from "./mailActions";
import { confirmNativeAction, showNativeMessage } from "./nativeFeedback";
import { groupMessages } from "./threads";
import { forwardBody, forwardSubject } from "./forward";
import {
  onNotificationAction,
  readNotificationSettings,
  requestInitialNotificationAccess,
  sendNewMailNotification,
} from "./notifications";
import {
  onAccountConnected,
  onAccountsChanged,
  onMailArrived,
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
import { runUpdaterAcceptanceIfConfigured } from "./updaterAcceptance";

const defaultAi: AiSettings = {
  provider: "ollama",
  baseUrl: "http://127.0.0.1:11434/",
  model: "qwen2.5:1.5b",
  apiKey: "",
  executable: "",
  modelPath: "",
};

const mailPageSize = 100;

export default function App() {
  const { t } = useTranslation();
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [threads, setThreads] = useState<MailThread[]>([]);
  const [active, setActive] = useState<MailSummary>();
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
  const [pendingActions, setPendingActions] = useState<PendingMailActions>({});
  const [outbox, setOutbox] = useState<MailSummary[]>([]);
  const [starredCount, setStarredCount] = useState(0);
  const [actionStatus, setActionStatus] = useState<{
    id: number;
    message: string;
    tone: "success" | "error";
  }>();
  const [updateState, setUpdateState] = useState<UpdateBannerState>();
  const searchRef = useRef<HTMLInputElement>(null);
  const statusId = useRef(0);
  const actionBusyRef = useRef(false);
  const loadRequestIdRef = useRef(0);
  const nextCursorRef = useRef<MailCursor | null>(null);
  const loadingMoreRef = useRef(false);
  const initialClassificationStartedRef = useRef(false);
  const classifyingRef = useRef(false);
  const manualUpdateCheckInFlightRef = useRef(false);

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
    void runUpdaterAcceptanceIfConfigured()
      .then(async (acceptanceActive) => {
        if (acceptanceActive || !current) return;
        const update = await checkForUpdate();
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
    api
      .accounts()
      .then((accountData) => {
        setAccounts(accountData);
        if (accountData.length === 0) {
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
  const currentViewRef = useRef({
    accountIds: activeAccounts,
    query: debouncedQuery,
    mailbox,
  });
  currentViewRef.current = {
    accountIds: activeAccounts,
    query: debouncedQuery,
    mailbox,
  };
  const refreshStarredCount = useCallback(async (accountIds: string[]) => {
    try {
      setStarredCount(await api.starredCount(accountIds));
    } catch {
      // The message list remains usable if a count refresh fails offline.
    }
  }, []);
  useEffect(() => {
    void refreshStarredCount(activeAccounts);
  }, [activeAccounts, refreshStarredCount]);

  const loadMessages = useCallback(async (requestedAccountIds?: string[]) => {
    const requestId = ++loadRequestIdRef.current;
    const currentView = currentViewRef.current;
    const accountIds = requestedAccountIds ?? currentView.accountIds;
    if (currentView.mailbox === "Outbox") {
      if (requestId === loadRequestIdRef.current) {
        setThreads([]);
        setLoading(false);
        setHasMore(false);
      }
      return;
    }
    if (!accountIds.length) {
      if (requestId === loadRequestIdRef.current) {
        setThreads([]);
        setHasMore(false);
      }
      return;
    }
    setLoading(true);
    setRemoteSearchUnavailable(false);
    try {
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
      if (requestId === loadRequestIdRef.current) {
        nextCursorRef.current = page.nextCursor;
        setThreads(page.conversations);
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
          if (requestId === loadRequestIdRef.current) {
            const merged = new Map(
              page.conversations.map((thread) => [thread.id, thread]),
            );
            for (const thread of groupMessages(remote)) {
              if (!merged.has(thread.id)) merged.set(thread.id, thread);
            }
            setThreads(
              [...merged.values()].sort(
                (left, right) =>
                  new Date(right.latest.received_at).getTime() -
                  new Date(left.latest.received_at).getTime(),
              ),
            );
          }
        } catch {
          // Local catalogue results remain useful when remote search is
          // unavailable or the device goes offline mid-query.
          if (requestId === loadRequestIdRef.current) {
            setRemoteSearchUnavailable(true);
          }
        }
      }
    } catch (error) {
      if (requestId === loadRequestIdRef.current) showError(error);
    } finally {
      if (requestId === loadRequestIdRef.current) setLoading(false);
    }
  }, []);
  const classifyPending = useCallback(async () => {
    if (classifyingRef.current) return;
    classifyingRef.current = true;
    setClassifying(true);
    try {
      const classified = await api.classifyPending();
      if (classified > 0) await loadMessages();
    } catch (error) {
      showError(error);
    } finally {
      classifyingRef.current = false;
      setClassifying(false);
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
      if (requestId !== loadRequestIdRef.current) return;
      nextCursorRef.current = page.nextCursor;
      setHasMore(page.nextCursor !== null);
      setThreads((current) => mergeThreads(current, page.conversations));
    } catch (error) {
      if (requestId === loadRequestIdRef.current) showError(error);
    } finally {
      loadingMoreRef.current = false;
      if (requestId === loadRequestIdRef.current) setLoadingMore(false);
    }
  }, [hasMore]);
  useEffect(() => {
    let disposeAccount: () => void = () => undefined;
    let disposeSettings: () => void = () => undefined;
    let disposeAccounts: () => void = () => undefined;
    let disposeNotifications: () => void = () => undefined;
    void onAccountConnected((account) => {
      setAccounts((current) => [
        ...current.filter((item) => item.id !== account.id),
        account,
      ]);
      void syncAccounts(
        [account],
        setSyncStatus,
        () => void loadMessages(),
        true,
      )
        .then(async () => {
          markSynced();
          await requestInitialNotificationAccess(notificationSettings);
          return loadMessages(
            selectedAccountId
              ? [selectedAccountId]
              : [...new Set([...activeAccounts, account.id])],
          );
        })
        .catch(showError)
        .finally(() => setSyncStatus(undefined));
    }).then((unlisten) => (disposeAccount = unlisten));
    void onSettingsChanged(setAiSettings).then(
      (unlisten) => (disposeSettings = unlisten),
    );
    void onNotificationSettingsChanged(setNotificationSettings).then(
      (unlisten) => (disposeNotifications = unlisten),
    );
    void onAccountsChanged((next) => {
      setAccounts(next);
      setSelectedAccountId((current) =>
        current && next.some((account) => account.id === current)
          ? current
          : undefined,
      );
    }).then((unlisten) => (disposeAccounts = unlisten));
    return () => {
      disposeAccount();
      disposeSettings();
      disposeAccounts();
      disposeNotifications();
    };
  }, [
    activeAccounts,
    loadMessages,
    markSynced,
    notificationSettings,
    selectedAccountId,
  ]);
  useEffect(() => {
    let dispose: () => void = () => undefined;
    void onMailIndexRebuilt(() => {
      setActive(undefined);
      setSelected(new Set());
      void loadMessages().finally(() => {
        markSynced();
        setSyncStatus(undefined);
      });
    }).then((unlisten) => (dispose = unlisten));
    return () => dispose();
  }, [loadMessages]);
  useEffect(() => {
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
        setSelected(new Set());
      } else if (progress.phase === "saving") {
        void loadMessages();
      }
    };
    void api
      .mailRebuildStatus()
      .then(([progress]) => progress && showRebuildProgress(progress))
      .catch(showError);
    void onMailRebuildProgress(showRebuildProgress).then(
      (unlisten) => (dispose = unlisten),
    );
    return () => dispose();
  }, [accounts, loadMessages]);
  useEffect(() => {
    let unlisten: () => void = () => undefined;
    void onComposeSent(() => {
      showStatus(t("feedback.sent"));
      void loadMessages();
    }).then((dispose) => {
      unlisten = dispose;
    });
    return () => unlisten();
  }, [loadMessages, showStatus, t]);
  useEffect(() => {
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
      unlisten = dispose;
    });
    return () => unlisten();
  }, []);
  useEffect(() => {
    if (!activeAccounts.length) return;
    void loadMessages().then(() => {
      if (initialClassificationStartedRef.current) return;
      initialClassificationStartedRef.current = true;
      void classifyPending();
    });
  }, [activeAccounts, classifyPending, debouncedQuery, loadMessages, mailbox]);
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

  useEffect(() => {
    setActive((current) => {
      if (!current) return current;
      return (
        threads
          .flatMap((thread) => thread.messages)
          .find((message) => message.id === current.id) ?? current
      );
    });
  }, [threads]);

  useEffect(() => {
    let dispose: () => void = () => undefined;
    const openNotification = async (extra: Record<string, unknown>) => {
      const currentWindow = getCurrentWindow();
      await currentWindow.show();
      await currentWindow.setFocus();
      setMailbox("INBOX");
      setQuery("");
      setSelected(new Set());
      setAiResult(undefined);

      const accountId =
        typeof extra.accountId === "string" ? extra.accountId : undefined;
      const messageId =
        typeof extra.messageId === "string" ? extra.messageId : undefined;
      const accountIds = accountId
        ? [accountId]
        : accounts.map((account) => account.id);
      setSelectedAccountId(accountId);
      if (!accountIds.length) return;
      const inbox = await api.search("", accountIds, "INBOX");
      nextCursorRef.current = inbox.nextCursor;
      setHasMore(inbox.nextCursor !== null);
      setThreads(inbox.conversations);
      const clickedThread = inbox.conversations.find((thread) =>
        thread.messages.some((message) => message.id === messageId),
      );
      const clickedMessage = clickedThread?.messages.find(
        (message) => message.id === messageId,
      );
      setActive(clickedMessage);
      if (clickedThread) {
        void setThreadReadState(clickedThread, true, { silent: true });
      }
    };
    void Promise.all([
      onNotificationAction(openNotification).then((listener) =>
        typeof listener === "function" ? listener : () => listener.unregister(),
      ),
      onDesktopNotificationAction(openNotification),
    ]).then((listeners) => {
      dispose = () => listeners.forEach((unlisten) => unlisten());
    });
    return () => dispose();
  }, [accounts]);

  const activeThread = useMemo(
    () =>
      active
        ? threads.find((thread) =>
            thread.messages.some((message) => message.id === active.id),
          )
        : undefined,
    [active, threads],
  );
  const targetThreads = selected.size
    ? threads.filter((thread) => selected.has(thread.id))
    : activeThread
      ? [activeThread]
      : [];
  const targets = targetThreads.flatMap((thread) => thread.messages);
  const selectAccount = (id: string) => {
    setSelectedAccountId(id);
    setMailbox("INBOX");
    setActive(undefined);
    setSelected(new Set());
    setAiResult(undefined);
  };
  const selectMailbox = (value: string) => {
    if (value === "INBOX") setSelectedAccountId(undefined);
    setMailbox(value);
    setSelected(new Set());
    setActive(undefined);
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
    } catch (error) {
      showError(error);
    } finally {
      setSyncStatus(undefined);
    }
  };
  const actionBusy = Object.keys(pendingActions).length > 0;
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
    if (!actionThreads.length || actionBusyRef.current) return;
    actionBusyRef.current = true;
    const originalThreads = [...threads];
    const actionTargets = actionThreads.flatMap((target) => target.messages);
    const targetIds = new Set(actionTargets.map((message) => message.id));
    const targetThreadIds = new Set(
      actionThreads.map((target) => target.thread.id),
    );
    const reducedMotion = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    const maxDelay = reducedMotion
      ? 0
      : Math.min((actionTargets.length - 1) * 24, 144);

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
      const activeIndex = threads.findIndex(
        (thread) => thread.id === activeThread.id,
      );
      const next =
        threads
          .slice(activeIndex + 1)
          .find((thread) => !targetThreadIds.has(thread.id)) ??
        threads
          .slice(0, activeIndex)
          .reverse()
          .find((thread) => !targetThreadIds.has(thread.id));
      setActive(next?.latest);
    }

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
    await wait(reducedMotion ? 0 : 210 + maxDelay);
    setThreads((current) =>
      current.filter((thread) => !targetThreadIds.has(thread.id)),
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
    actionBusyRef.current = false;
    await loadMessages();
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
  const openReply = (thread: MailThread | undefined = activeThread) => {
    if (!thread) return;
    const account = accounts.find(
      (item) => item.id === thread.latest.account_id,
    );
    const replyMessage =
      [...thread.messages]
        .reverse()
        .find(
          (message) =>
            !account ||
            message.from_address.toLowerCase() !== account.email.toLowerCase(),
        ) ?? thread.latest;
    const prefix = t("reader.replyPrefix");
    openComposeWindow({
      accountId: replyMessage.account_id,
      to: replyMessage.from_address,
      subject: replyMessage.subject
        .toLowerCase()
        .startsWith(prefix.toLowerCase())
        ? replyMessage.subject
        : `${prefix} ${replyMessage.subject}`,
      inReplyTo: replyMessage.message_id,
      references: [replyMessage.reference_ids, replyMessage.message_id]
        .filter(Boolean)
        .join(" "),
      contextMessageIds: thread.messages.map((message) => message.id),
    });
  };
  const openForward = async (thread: MailThread | undefined = activeThread) => {
    const message = thread?.latest;
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
        forwardMessageId: message.has_attachments ? message.id : undefined,
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
  const toggleThreadStar = async (thread: MailThread, flagged: boolean) => {
    if (actionBusyRef.current) return;
    actionBusyRef.current = true;
    const previous = thread.messages.map((message) => ({
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
        latest: updateFlag(item.latest, flagged),
      })),
    );
    setActive((current) => (current ? updateFlag(current, flagged) : current));
    const results = await Promise.allSettled(
      thread.messages.map((message) => api.setStarred(message.id, flagged)),
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
          latest: restoreFlag(item.latest),
        })),
      );
      setActive((current) => (current ? restoreFlag(current) : current));
      showStatus(t("feedback.starFailed", { count: failed.size }), "error");
    } else {
      showStatus(
        t(flagged ? "feedback.starSuccess" : "feedback.unstarSuccess"),
      );
    }
    await refreshStarredCount(activeAccounts);
    actionBusyRef.current = false;
  };
  const setThreadReadState = async (
    thread: MailThread,
    read: boolean,
    options?: { silent?: boolean },
  ) => {
    const targets = thread.messages.filter(
      (message) => message.is_read !== read,
    );
    if (!targets.length) return;
    const silent = Boolean(options?.silent);
    const previous = targets.map((message) => ({
      id: message.id,
      is_read: message.is_read,
    }));
    const ids = new Set(previous.map((message) => message.id));
    setThreads((current) =>
      current.map((item) => updateThreadReadState(item, ids, read)),
    );
    setActive((current) =>
      current ? updateMessageReadState(current, ids, read) : current,
    );
    const results = await Promise.allSettled(
      targets.map((message) => api.setRead(message.id, read)),
    );
    const failed = new Set(
      results.flatMap((result, index) =>
        result.status === "rejected" ? [previous[index].id] : [],
      ),
    );
    if (failed.size) {
      const prior = new Map(previous.map((item) => [item.id, item.is_read]));
      setThreads((current) =>
        current.map((item) => restoreThreadReadState(item, failed, prior)),
      );
      setActive((current) =>
        current ? restoreMessageReadState(current, failed, prior) : current,
      );
      if (!silent) {
        showStatus(
          t(read ? "feedback.readFailed" : "feedback.unreadFailed", {
            count: failed.size,
          }),
          "error",
        );
      }
      return;
    }
    if (!silent) {
      showStatus(t(read ? "feedback.readSuccess" : "feedback.unreadSuccess"));
    }
  };
  const unsubscribe = async () => {
    if (!active || unsubscribeLoading) return;
    setUnsubscribeLoading(true);
    try {
      const result = await api.unsubscribe(active.id);
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
    ["mod+R", () => openReply()],
    ["mod+shift+F", () => void openForward()],
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
          openReply();
          break;
        case "forward":
          void openForward();
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
              `⌘,  ${t("settings.title")}`,
            ].join("\n"),
          );
          break;
        case "terminal-command":
          void configureTerminalCommand();
          break;
        default:
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
    openReply,
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
      await refreshStarredCount(activeAccounts);
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
        outboxCount={outbox.length}
        starredCount={starredCount}
      />
      <MailList
        threads={mailbox === "Outbox" ? groupMessages(outbox) : threads}
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
        smartInbox={mailbox === "INBOX" && !query.trim()}
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
              showStatus(t("feedback.categorySaved"));
            })
            .catch(showError);
        }}
        onToggleStar={(message, starred) => void toggleStar(message, starred)}
        onQuery={setQuery}
        onOpen={(thread) => {
          setActive(thread.latest);
          setAiResult(undefined);
          void setThreadReadState(thread, true, { silent: true });
        }}
        onSelect={select}
        onSync={() => void sync()}
        onCompose={openCompose}
        onArchive={() => void applyAction("archive")}
        onSpam={() => void applyAction("spam")}
        onReplyThread={(thread) => openReply(thread)}
        onForwardThread={(thread) => void openForward(thread)}
        onActionThread={(thread, action) => void applyAction(action, [thread])}
        onToggleReadThread={(thread, read) =>
          void setThreadReadState(thread, read)
        }
        onToggleStarThread={(thread, flagged) =>
          void toggleThreadStar(thread, flagged)
        }
        onSummarize={() => void runSummary()}
        onLoadMore={() => void loadMoreMessages()}
        pendingActions={pendingActions}
        actionsDisabled={actionBusy}
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
        actionsDisabled={actionBusy}
        onArchive={() => void applyAction("archive")}
        onSpam={() =>
          void applyAction(
            active?.mailbox.split("::", 1)[0] === "Spam" ? "not_spam" : "spam",
          )
        }
        onTrash={() => void applyAction("trash")}
        onReply={openReply}
        onForward={() => void openForward()}
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
        unsubscribeLoading={unsubscribeLoading}
        onUnsubscribe={() => void unsubscribe()}
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
  const latest =
    messages.find((message) => message.id === thread.latest.id) ??
    thread.latest;
  return {
    ...thread,
    messages,
    latest,
    unread: messages.some((message) => !message.is_read),
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
  const latest =
    messages.find((message) => message.id === thread.latest.id) ??
    thread.latest;
  return {
    ...thread,
    messages,
    latest,
    unread: messages.some((message) => !message.is_read),
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
