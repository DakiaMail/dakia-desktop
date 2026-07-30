import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  disable as disableAutostart,
  enable as enableAutostart,
  isEnabled as isAutostartEnabled,
} from "@tauri-apps/plugin-autostart";
import { api } from "./api";
import { AccountSetup } from "./components/AccountSetup";
import { Settings } from "./components/Settings";
import {
  closeNativeWindow,
  notifyAccountConnected,
  notifyNotificationSettingsChanged,
  notifySettingsChanged,
  onNativeMenuAction,
  onAccountConnected,
  onSettingsAccountSelected,
  notifyAccountUpdated,
  onMailSyncState,
  onMailIndexRebuilt,
  onMailRebuildProgress,
  openAccountWindow,
} from "./nativeWindows";
import { confirmNativeAction, showNativeMessage } from "./nativeFeedback";
import {
  notificationPermissionGranted,
  readNotificationSettings,
  saveNotificationSettings,
  sendTestNotification,
} from "./notifications";
import type {
  Account,
  AiSettings,
  NotificationSettings,
  Provider,
  RealtimeSyncStatus,
  SyncProgress,
} from "./types";

const defaultAi: AiSettings = {
  provider: "ollama",
  baseUrl: "http://127.0.0.1:11434/",
  model: "qwen2.5:1.5b",
  apiKey: "",
  executable: "",
  modelPath: "",
};

export function AccountWindowApp() {
  const { t } = useTranslation();
  const [providers, setProviders] = useState<Provider[]>([]);
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    document.title = t("account.title");
    void api.providers().then(setProviders).catch(showError);
  }, [t]);

  const save = async (draft: Record<string, unknown>, password?: string) => {
    setSaving(true);
    try {
      const account =
        password === undefined
          ? await api.addOAuthAccount(draft)
          : await api.addAccount(draft, password);
      await notifyAccountConnected(account);
      await closeNativeWindow();
    } catch (error) {
      showError(error, t("account.setupError"));
      setSaving(false);
    }
  };

  return (
    <AccountSetup
      providers={providers}
      saving={saving}
      onSave={(draft, password) => void save(draft, password)}
      onOAuth={(draft) => void save(draft)}
    />
  );

  function showError(error: unknown, title = t("errors.generic")) {
    void showNativeMessage(
      title,
      error instanceof Error ? error.message : String(error),
      "error",
    );
  }
}

export function SettingsWindowApp() {
  const { t } = useTranslation();
  const [ai, setAi] = useState<AiSettings>(() =>
    readJson("dakia.ai", defaultAi),
  );
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [accountsLoading, setAccountsLoading] = useState(true);
  const [accountSaving, setAccountSaving] = useState(false);
  const [accountRemoving, setAccountRemoving] = useState(false);
  const [accountFullSyncing, setAccountFullSyncing] = useState(false);
  const [accountFullSyncProgress, setAccountFullSyncProgress] =
    useState<SyncProgress>();
  const [selectedAccountId, setSelectedAccountId] = useState<
    string | undefined
  >(
    () =>
      new URLSearchParams(window.location.search).get("accountId") ?? undefined,
  );
  const [notifications, setNotifications] = useState<NotificationSettings>(
    readNotificationSettings,
  );
  const [notificationPermission, setNotificationPermission] = useState<
    boolean | null
  >(null);
  const [launchAtLogin, setLaunchAtLogin] = useState(false);
  const [realtimeStatuses, setRealtimeStatuses] = useState<
    RealtimeSyncStatus[]
  >([]);
  const aiPersistenceQueueRef = useRef(Promise.resolve());
  const aiSettingsGenerationRef = useRef(0);

  useEffect(() => {
    document.title = t("settings.title");
    void notificationPermissionGranted().then(setNotificationPermission);
    if ("__TAURI_INTERNALS__" in window) {
      void isAutostartEnabled().then(setLaunchAtLogin).catch(showError);
    }
    void api
      .accounts()
      .then(setAccounts)
      .catch(showError)
      .finally(() => setAccountsLoading(false));
    void api.realtimeSyncStatus().then(setRealtimeStatuses).catch(showError);
    void api
      .mailRebuildStatus()
      .then(([progress]) => {
        if (!progress) return;
        setAccountFullSyncing(true);
        setAccountFullSyncProgress(progress);
      })
      .catch(showError);
    let dispose: () => void = () => undefined;
    let disposeAccount: () => void = () => undefined;
    let disposeSelectedAccount: () => void = () => undefined;
    let disposeSyncState: () => void = () => undefined;
    let disposeRebuildProgress: () => void = () => undefined;
    let disposeRebuilt: () => void = () => undefined;
    void onNativeMenuAction((action) => {
      if (action === "close-window") void closeNativeWindow();
    }).then((unlisten) => (dispose = unlisten));
    void onAccountConnected((account) => {
      setAccounts((current) => [
        ...current.filter((item) => item.id !== account.id),
        account,
      ]);
    }).then((unlisten) => (disposeAccount = unlisten));
    void onSettingsAccountSelected(setSelectedAccountId).then(
      (unlisten) => (disposeSelectedAccount = unlisten),
    );
    void onMailSyncState((status) => {
      setRealtimeStatuses((current) => [
        ...current.filter((item) => item.accountId !== status.accountId),
        status,
      ]);
    }).then((unlisten) => (disposeSyncState = unlisten));
    void onMailRebuildProgress((progress) => {
      setAccountFullSyncing(true);
      setAccountFullSyncProgress(progress);
    }).then((unlisten) => (disposeRebuildProgress = unlisten));
    void onMailIndexRebuilt(() => {
      setAccountFullSyncing(false);
      setAccountFullSyncProgress(undefined);
    }).then((unlisten) => (disposeRebuilt = unlisten));
    return () => {
      dispose();
      disposeAccount();
      disposeSelectedAccount();
      disposeSyncState();
      disposeRebuildProgress();
      disposeRebuilt();
    };
  }, [t]);

  const updateAi = (value: AiSettings) => {
    const generation = ++aiSettingsGenerationRef.current;
    const apiKeyChanged = value.apiKey !== ai.apiKey;
    setAi(value);
    localStorage.setItem("dakia.ai", JSON.stringify({ ...value, apiKey: "" }));
    aiPersistenceQueueRef.current = aiPersistenceQueueRef.current
      .catch(() => undefined)
      .then(async () => {
        if (apiKeyChanged) await api.saveAiApiKey(value.apiKey);
        if (generation === aiSettingsGenerationRef.current) {
          await notifySettingsChanged({ ...value, apiKey: "" });
        }
      });
    void aiPersistenceQueueRef.current.catch(showError);
  };

  const updateNotifications = (value: NotificationSettings) => {
    setNotifications(value);
    saveNotificationSettings(value);
    void notifyNotificationSettingsChanged(value);
  };

  const testNotification = async () => {
    try {
      const granted = await sendTestNotification(
        notifications,
        t("notifications.testTitle"),
        t("notifications.testBody"),
      );
      setNotificationPermission(granted);
    } catch (error) {
      showError(error, t("settings.notificationError"));
    }
  };

  const updateLaunchAtLogin = async (enabled: boolean) => {
    try {
      if (enabled) await enableAutostart();
      else await disableAutostart();
      setLaunchAtLogin(enabled);
    } catch (error) {
      showError(error, t("settings.launchAtLoginError"));
    }
  };

  const saveAccount = async (input: Record<string, unknown>) => {
    setAccountSaving(true);
    try {
      const updated = await api.updateAccount(input);
      const next = accounts.map((account) =>
        account.id === updated.id ? updated : account,
      );
      setAccounts(next);
      await notifyAccountUpdated(updated);
      await showNativeMessage(
        t("settings.accountSaved"),
        t("settings.accountSavedBody"),
      );
    } catch (error) {
      showError(error, t("settings.accountSaveError"));
    } finally {
      setAccountSaving(false);
    }
  };

  const removeAccount = async (account: Account) => {
    const confirmed = await confirmNativeAction(
      t("settings.removeAccount"),
      t("settings.removeAccountConfirm", { email: account.email }),
      t("settings.removeAccount"),
    );
    if (!confirmed) return;
    setAccountRemoving(true);
    try {
      await api.removeAccount(account.id);
      const next = accounts.filter((item) => item.id !== account.id);
      setAccounts(next);
    } catch (error) {
      showError(error, t("settings.accountRemoveError"));
    } finally {
      setAccountRemoving(false);
    }
  };

  const fullSyncAccount = async (account: Account) => {
    const confirmed = await confirmNativeAction(
      t("settings.fullSync"),
      t("settings.fullSyncConfirm", { email: account.email }),
      t("settings.fullSync"),
    );
    if (!confirmed) return;
    setAccountFullSyncing(true);
    setAccountFullSyncProgress({
      phase: "connecting",
      completed: 0,
      total: null,
    });
    try {
      await api.sync(account.id, setAccountFullSyncProgress, true);
      await showNativeMessage(
        t("settings.fullSyncComplete"),
        t("settings.fullSyncCompleteBody", { email: account.email }),
      );
    } catch (error) {
      showError(error, t("settings.fullSyncError"));
    } finally {
      setAccountFullSyncing(false);
      setAccountFullSyncProgress(undefined);
    }
  };

  return (
    <Settings
      ai={ai}
      accounts={accounts}
      accountsLoading={accountsLoading}
      accountSaving={accountSaving}
      accountRemoving={accountRemoving}
      accountFullSyncing={accountFullSyncing}
      accountFullSyncProgress={accountFullSyncProgress}
      notifications={notifications}
      notificationPermission={notificationPermission}
      launchAtLogin={launchAtLogin}
      realtimeStatuses={realtimeStatuses}
      onAiChange={updateAi}
      onAddAccount={() => void openAccountWindow()}
      selectedAccountId={selectedAccountId}
      onSaveAccount={(input) => void saveAccount(input)}
      onRemoveAccount={(account) => void removeAccount(account)}
      onFullSyncAccount={(account) => void fullSyncAccount(account)}
      onNotificationsChange={updateNotifications}
      onTestNotification={() => void testNotification()}
      onLaunchAtLoginChange={(enabled) => void updateLaunchAtLogin(enabled)}
    />
  );

  function showError(error: unknown, title = t("errors.generic")) {
    void showNativeMessage(
      title,
      error instanceof Error ? error.message : String(error),
      "error",
    );
  }
}

function readJson<T>(key: string, fallback: T): T {
  try {
    return { ...fallback, ...JSON.parse(localStorage.getItem(key) ?? "{}") };
  } catch {
    return fallback;
  }
}
