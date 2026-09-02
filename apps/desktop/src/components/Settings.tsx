import {
  Button,
  Divider,
  Group,
  Loader,
  PasswordInput,
  SegmentedControl,
  Select,
  Stack,
  Switch,
  Tabs,
  Text,
  TextInput,
  useMantineColorScheme,
} from "@mantine/core";
import {
  IconBrain,
  IconAt,
  IconBell,
  IconLanguage,
  IconPalette,
  IconShieldLock,
} from "@tabler/icons-react";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { ANALYTICS_PRIVACY_URL, type AnalyticsSettings } from "../analytics";
import { api } from "../api";
import { AI_FEATURES_VISIBLE } from "../features";
import { confirmNativeAction } from "../nativeFeedback";
import { resetOfflineTranslator } from "../offlineTranslation";
import type {
  Account,
  AiSettings,
  NotificationSettings,
  RealtimeSyncStatus,
  SyncProgress,
  TranslationModelStatus,
} from "../types";
import { AccountsSettings } from "./AccountsSettings";
import { AnalyticsDataPreview } from "./AnalyticsDataPreview";

type Props = {
  ai: AiSettings;
  accounts: Account[];
  accountsLoading: boolean;
  accountSaving: boolean;
  accountRemoving: boolean;
  accountFullSyncing: boolean;
  accountFullSyncProgress?: SyncProgress;
  notifications: NotificationSettings;
  notificationPermission: boolean | null;
  launchAtLogin: boolean;
  analytics: AnalyticsSettings;
  realtimeStatuses: RealtimeSyncStatus[];
  onAiChange: (settings: AiSettings) => void;
  onAddAccount: () => void;
  selectedAccountId?: string;
  onSaveAccount: (input: Record<string, unknown>) => void;
  onRemoveAccount: (account: Account) => void;
  onFullSyncAccount: (account: Account) => void;
  onNotificationsChange: (settings: NotificationSettings) => void;
  onTestNotification: () => void;
  onLaunchAtLoginChange: (enabled: boolean) => void;
  onAnalyticsChange: (enabled: boolean) => void;
};

const DEFAULT_TAB = "accounts";

export function Settings({
  ai,
  accounts,
  accountsLoading,
  accountSaving,
  accountRemoving,
  accountFullSyncing,
  accountFullSyncProgress,
  notifications,
  notificationPermission,
  launchAtLogin,
  analytics,
  realtimeStatuses,
  onAiChange,
  onAddAccount,
  selectedAccountId,
  onSaveAccount,
  onRemoveAccount,
  onFullSyncAccount,
  onNotificationsChange,
  onTestNotification,
  onLaunchAtLoginChange,
  onAnalyticsChange,
}: Props) {
  const { t } = useTranslation();
  const { colorScheme, setColorScheme } = useMantineColorScheme();
  const [activeTab, setActiveTab] = useState(selectedAccountId ?? DEFAULT_TAB);
  const [translationModels, setTranslationModels] = useState<
    TranslationModelStatus[]
  >([]);
  const [translationModelsLoading, setTranslationModelsLoading] =
    useState(false);
  const [translationModelRemoving, setTranslationModelRemoving] =
    useState<string>();
  const [translationModelError, setTranslationModelError] = useState<string>();
  useEffect(() => {
    if (selectedAccountId) setActiveTab("accounts");
  }, [selectedAccountId]);
  useEffect(() => {
    if (activeTab !== "translation") return;
    setTranslationModelsLoading(true);
    setTranslationModelError(undefined);
    void api
      .translationModels()
      .then(setTranslationModels)
      .catch((error) =>
        setTranslationModelError(
          error instanceof Error ? error.message : String(error),
        ),
      )
      .finally(() => setTranslationModelsLoading(false));
  }, [activeTab]);
  const removeTranslationModel = async (model: TranslationModelStatus) => {
    const language = model.sourceName;
    const confirmed = await confirmNativeAction(
      t("translation.remove", { language }),
      t("translation.removeConfirm", { language }),
      t("translation.remove", { language }),
    );
    if (!confirmed) return;
    setTranslationModelRemoving(model.source);
    setTranslationModelError(undefined);
    try {
      await resetOfflineTranslator();
      await api.removeTranslationModel(model.source);
      setTranslationModels((current) =>
        current.map((item) =>
          item.source === model.source ? { ...item, installed: false } : item,
        ),
      );
    } catch (error) {
      setTranslationModelError(
        error instanceof Error ? error.message : String(error),
      );
    } finally {
      setTranslationModelRemoving(undefined);
    }
  };
  const update = <K extends keyof AiSettings>(key: K, value: AiSettings[K]) =>
    onAiChange({ ...ai, [key]: value });
  return (
    <main className="utility-window settings-window">
      <header className="utility-header" data-tauri-drag-region>
        <h1>{t("settings.title")}</h1>
      </header>
      <Tabs
        className="settings-tabs"
        value={activeTab}
        onChange={(value) => setActiveTab(value ?? DEFAULT_TAB)}
        orientation="vertical"
      >
        <Tabs.List>
          <Tabs.Tab value="accounts" leftSection={<IconAt size={16} />}>
            {t("settings.accounts")}
          </Tabs.Tab>
          <Tabs.Tab value="appearance" leftSection={<IconPalette size={16} />}>
            {t("settings.appearance")}
          </Tabs.Tab>
          <Tabs.Tab value="notifications" leftSection={<IconBell size={16} />}>
            {t("settings.notifications")}
          </Tabs.Tab>
          {AI_FEATURES_VISIBLE ? (
            <Tabs.Tab value="ai" leftSection={<IconBrain size={16} />}>
              {t("settings.ai")}
            </Tabs.Tab>
          ) : null}
          <Tabs.Tab
            value="translation"
            leftSection={<IconLanguage size={16} />}
          >
            {t("settings.translation")}
          </Tabs.Tab>
          <Tabs.Tab value="privacy" leftSection={<IconShieldLock size={16} />}>
            {t("settings.privacy")}
          </Tabs.Tab>
        </Tabs.List>
        <Tabs.Panel
          value="accounts"
          className="settings-pane settings-accounts-pane"
        >
          {accountsLoading ? (
            <div
              className="accounts-settings-loading"
              aria-label={t("settings.loadingAccounts")}
            >
              <span />
              <span />
              <span />
            </div>
          ) : (
            <AccountsSettings
              accounts={accounts}
              saving={accountSaving}
              removing={accountRemoving}
              fullSyncing={accountFullSyncing}
              fullSyncProgress={accountFullSyncProgress}
              onAdd={onAddAccount}
              selectedAccountId={selectedAccountId}
              onSave={onSaveAccount}
              onRemove={onRemoveAccount}
              onFullSync={onFullSyncAccount}
            />
          )}
        </Tabs.Panel>
        <Tabs.Panel value="appearance" className="settings-pane">
          <Stack>
            <Text fw={650}>{t("settings.appearance")}</Text>
            <SegmentedControl
              value={colorScheme}
              onChange={(value) =>
                setColorScheme(value as "auto" | "light" | "dark")
              }
              data={[
                { value: "auto", label: t("settings.system") },
                { value: "light", label: t("settings.light") },
                { value: "dark", label: t("settings.dark") },
              ]}
            />
          </Stack>
        </Tabs.Panel>
        <Tabs.Panel value="notifications" className="settings-pane">
          <Stack>
            <div>
              <Text fw={650}>{t("settings.notifications")}</Text>
              <Text size="sm" c="dimmed">
                {t("settings.notificationsBody")}
              </Text>
            </div>
            <Switch
              label={t("settings.enableNotifications")}
              checked={notifications.enabled}
              onChange={(event) =>
                onNotificationsChange({
                  ...notifications,
                  enabled: event.currentTarget.checked,
                })
              }
            />
            <Switch
              label={t("settings.notificationSound")}
              checked={notifications.soundEnabled}
              disabled={!notifications.enabled}
              onChange={(event) =>
                onNotificationsChange({
                  ...notifications,
                  soundEnabled: event.currentTarget.checked,
                })
              }
            />
            <Switch
              label={t("settings.notificationPreview")}
              description={t("settings.notificationPreviewBody")}
              checked={notifications.showPreview}
              disabled={!notifications.enabled}
              onChange={(event) =>
                onNotificationsChange({
                  ...notifications,
                  showPreview: event.currentTarget.checked,
                })
              }
            />
            <Divider />
            <Switch
              label={t("settings.launchAtLogin")}
              description={t("settings.launchAtLoginBody")}
              checked={launchAtLogin}
              onChange={(event) =>
                onLaunchAtLoginChange(event.currentTarget.checked)
              }
            />
            <Divider />
            {realtimeStatuses.map((status) => {
              const account = accounts.find(
                (item) => item.id === status.accountId,
              );
              return (
                <Text
                  key={status.accountId}
                  size="sm"
                  c={status.state === "paused" ? "red" : "dimmed"}
                >
                  {t(`settings.realtime.${status.state}`, {
                    account: account?.email ?? status.accountId,
                  })}
                </Text>
              );
            })}
            <Divider />
            <Text
              size="sm"
              c={notificationPermission === false ? "red" : "dimmed"}
            >
              {notificationPermission === null
                ? t("settings.notificationPermissionChecking")
                : notificationPermission
                  ? t("settings.notificationPermissionGranted")
                  : t("settings.notificationPermissionBlocked")}
            </Text>
            <Button
              variant="light"
              disabled={!notifications.enabled}
              onClick={onTestNotification}
            >
              {t("settings.testNotification")}
            </Button>
          </Stack>
        </Tabs.Panel>
        {AI_FEATURES_VISIBLE ? (
          <Tabs.Panel value="ai" className="settings-pane">
            <Stack>
              <Text fw={650}>{t("settings.ai")}</Text>
              <Text size="sm" c="dimmed">
                {t("ai.privacy")}
              </Text>
              <Select
                label={t("ai.provider")}
                value={ai.provider}
                onChange={(value) =>
                  update(
                    "provider",
                    (value ?? "ollama") as AiSettings["provider"],
                  )
                }
                data={[
                  { value: "ollama", label: t("ai.providerOllama") },
                  { value: "openai", label: t("ai.providerOpenAi") },
                  { value: "local", label: t("ai.providerLocal") },
                ]}
              />
              {ai.provider !== "local" ? (
                <TextInput
                  label={t("ai.baseUrl")}
                  value={ai.baseUrl}
                  onChange={(event) =>
                    update("baseUrl", event.currentTarget.value)
                  }
                />
              ) : null}
              <TextInput
                label={t("ai.model")}
                value={ai.model}
                onChange={(event) => update("model", event.currentTarget.value)}
              />
              {ai.provider === "openai" ? (
                <PasswordInput
                  label={t("ai.apiKey")}
                  value={ai.apiKey}
                  onChange={(event) =>
                    update("apiKey", event.currentTarget.value)
                  }
                />
              ) : null}
              {ai.provider === "local" ? (
                <>
                  <TextInput
                    label={t("ai.executable")}
                    value={ai.executable}
                    onChange={(event) =>
                      update("executable", event.currentTarget.value)
                    }
                  />
                  <TextInput
                    label={t("ai.modelPath")}
                    value={ai.modelPath}
                    onChange={(event) =>
                      update("modelPath", event.currentTarget.value)
                    }
                  />
                </>
              ) : null}
            </Stack>
          </Tabs.Panel>
        ) : null}
        <Tabs.Panel value="translation" className="settings-pane">
          <Stack>
            <div>
              <Text fw={650}>{t("translation.settingsTitle")}</Text>
              <Text size="sm" c="dimmed">
                {t("translation.settingsBody")}
              </Text>
            </div>
            <Divider />
            {translationModelsLoading ? <Loader size="sm" /> : null}
            {translationModelError ? (
              <Text size="sm" c="red">
                {translationModelError}
              </Text>
            ) : null}
            {translationModels.map((model) => {
              const language = model.sourceName;
              return (
                <Group key={model.source} justify="space-between" wrap="nowrap">
                  <div>
                    <Text size="sm" fw={600}>
                      {language} → English
                    </Text>
                    <Text size="xs" c="dimmed">
                      {t(
                        model.installed
                          ? "translation.installed"
                          : "translation.available",
                        { size: formatBytes(model.downloadBytes) },
                      )}
                    </Text>
                  </div>
                  {model.installed ? (
                    <Button
                      size="compact-xs"
                      variant="subtle"
                      color="red"
                      loading={translationModelRemoving === model.source}
                      onClick={() => void removeTranslationModel(model)}
                    >
                      {t("translation.remove", { language })}
                    </Button>
                  ) : null}
                </Group>
              );
            })}
          </Stack>
        </Tabs.Panel>
        <Tabs.Panel value="privacy" className="settings-pane">
          <Stack gap="sm">
            <div>
              <Text fw={650}>{t("settings.analyticsTitle")}</Text>
              <Text size="sm">{t("settings.analyticsBody")}</Text>
            </div>
            <Switch
              label={t("settings.analyticsEnable")}
              description={t("settings.analyticsDisclosure")}
              checked={analytics.consent === "enabled"}
              onChange={(event) =>
                onAnalyticsChange(event.currentTarget.checked)
              }
            />
            <AnalyticsDataPreview
              accounts={accounts}
              enabled={analytics.consent === "enabled"}
            />
            <Text size="sm" c="dimmed">
              {t("settings.analyticsProcessingDisclosure")}{" "}
              <Text
                component="button"
                td="underline"
                c="blue"
                inherit
                onClick={() => void api.openExternal(ANALYTICS_PRIVACY_URL)}
              >
                {t("settings.analyticsPrivacyDetails")}
              </Text>
            </Text>
            <Divider my="xs" />
            <Text fw={650}>{t("settings.privacy")}</Text>
            <Text size="sm">{t("settings.privacyBody")}</Text>
            {AI_FEATURES_VISIBLE ? (
              <>
                <Divider my="xs" />
                <Text fw={600} size="sm">
                  {t("settings.aiDisclosureTitle")}
                </Text>
                <Text size="sm" c="dimmed">
                  {t("settings.aiDisclosureBody")}
                </Text>
              </>
            ) : null}
          </Stack>
        </Tabs.Panel>
      </Tabs>
    </main>
  );
}

function formatBytes(bytes: number) {
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
