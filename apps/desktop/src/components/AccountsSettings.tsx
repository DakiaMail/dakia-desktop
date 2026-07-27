import {
  Button,
  Group,
  PasswordInput,
  Progress,
  Select,
  Stack,
  Text,
  TextInput,
} from "@mantine/core";
import {
  IconCirclePlus,
  IconMail,
  IconRefresh,
  IconServer,
  IconTrash,
} from "@tabler/icons-react";
import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import type { Account, Security, SyncProgress } from "../types";

type Props = {
  accounts: Account[];
  saving: boolean;
  removing: boolean;
  fullSyncing: boolean;
  fullSyncProgress?: SyncProgress;
  onAdd: () => void;
  selectedAccountId?: string;
  onSave: (input: Record<string, unknown>) => void;
  onRemove: (account: Account) => void;
  onFullSync: (account: Account) => void;
};

type Draft = {
  accountName: string;
  displayName: string;
  imapHost: string;
  imapPort: string;
  imapSecurity: Security;
  smtpHost: string;
  smtpPort: string;
  smtpSecurity: Security;
  archiveMailbox: string;
  spamMailbox: string;
  password: string;
};

const securityOptions = [
  { value: "tls", label: "TLS" },
  { value: "start_tls", label: "STARTTLS" },
];

export function AccountsSettings({
  accounts,
  saving,
  removing,
  fullSyncing,
  fullSyncProgress,
  onAdd,
  selectedAccountId,
  onSave,
  onRemove,
  onFullSync,
}: Props) {
  const { t } = useTranslation();
  const [selectedId, setSelectedId] = useState<string | undefined>(
    accounts[0]?.id,
  );
  const selected = useMemo(
    () => accounts.find((account) => account.id === selectedId) ?? accounts[0],
    [accounts, selectedId],
  );
  const [draft, setDraft] = useState<Draft>();

  useEffect(() => {
    if (
      selectedAccountId &&
      accounts.some((account) => account.id === selectedAccountId)
    ) {
      setSelectedId(selectedAccountId);
    }
  }, [accounts, selectedAccountId]);

  useEffect(() => {
    if (!selected) {
      setSelectedId(undefined);
      setDraft(undefined);
      return;
    }
    if (selected.id !== selectedId) setSelectedId(selected.id);
    setDraft(toDraft(selected));
  }, [selected?.id]);

  const update = <K extends keyof Draft>(key: K, value: Draft[K]) =>
    setDraft((current) => (current ? { ...current, [key]: value } : current));
  const valid = Boolean(
    selected &&
    draft?.accountName.trim() &&
    draft?.displayName.trim() &&
    draft.imapHost.trim() &&
    Number(draft.imapPort) > 0 &&
    draft.smtpHost.trim() &&
    Number(draft.smtpPort) > 0,
  );

  return (
    <div className="accounts-settings">
      <aside
        className="accounts-settings-list"
        aria-label={t("settings.accounts")}
      >
        <div className="accounts-settings-list-heading">
          <Text size="xs" fw={650} c="dimmed">
            {t("settings.mailAccounts")}
          </Text>
          <button
            type="button"
            className="account-list-add"
            onClick={onAdd}
            aria-label={t("actions.addAccount")}
            title={t("actions.addAccount")}
          >
            <IconCirclePlus size={17} stroke={1.7} />
          </button>
        </div>
        {accounts.map((account) => (
          <button
            type="button"
            key={account.id}
            className="accounts-settings-account"
            data-active={account.id === selected?.id}
            onClick={() => setSelectedId(account.id)}
          >
            <span className="account-provider-icon">
              <IconMail size={17} stroke={1.7} />
            </span>
            <span>
              <strong>{account.account_name}</strong>
              <small>{account.email}</small>
            </span>
          </button>
        ))}
      </aside>

      <section className="account-settings-detail">
        {!selected || !draft ? (
          <div className="account-settings-empty">
            <span>
              <IconMail size={24} stroke={1.5} />
            </span>
            <Text fw={650}>{t("settings.noAccountsTitle")}</Text>
            <Text size="sm" c="dimmed">
              {t("settings.noAccountsBody")}
            </Text>
            <Button leftSection={<IconCirclePlus size={15} />} onClick={onAdd}>
              {t("actions.addAccount")}
            </Button>
          </div>
        ) : (
          <Stack gap="lg" className="account-settings-form">
            <div className="account-detail-heading">
              <span className="account-detail-icon">
                <IconMail size={20} />
              </span>
              <div>
                <Text fw={680}>{selected.email}</Text>
                <Text size="xs" c="dimmed">
                  {t("settings.connectedWith", {
                    provider: selected.provider_id,
                  })}
                </Text>
              </div>
            </div>

            <section className="account-config-section">
              <Text className="account-config-label">
                {t("settings.identity")}
              </Text>
              <TextInput
                label={t("settings.accountName")}
                description={t("settings.accountNameHint")}
                value={draft.accountName}
                onChange={(event) =>
                  update("accountName", event.currentTarget.value)
                }
              />
              <TextInput
                label={t("account.name")}
                value={draft.displayName}
                onChange={(event) =>
                  update("displayName", event.currentTarget.value)
                }
              />
              <TextInput
                label={t("account.email")}
                value={selected.email}
                disabled
              />
            </section>

            <section className="account-config-section">
              <Text className="account-config-label">
                <IconServer size={14} stroke={1.7} />
                {t("settings.serverSettings")}
              </Text>
              <div className="account-server-grid">
                <TextInput
                  label={t("account.imapHost")}
                  value={draft.imapHost}
                  onChange={(event) =>
                    update("imapHost", event.currentTarget.value)
                  }
                />
                <TextInput
                  label={t("account.imapPort")}
                  value={draft.imapPort}
                  inputMode="numeric"
                  onChange={(event) =>
                    update("imapPort", event.currentTarget.value)
                  }
                />
                <Select
                  label={t("account.imapSecurity")}
                  value={draft.imapSecurity}
                  data={securityOptions}
                  allowDeselect={false}
                  onChange={(value) =>
                    update("imapSecurity", (value ?? "tls") as Security)
                  }
                />
                <TextInput
                  label={t("account.smtpHost")}
                  value={draft.smtpHost}
                  onChange={(event) =>
                    update("smtpHost", event.currentTarget.value)
                  }
                />
                <TextInput
                  label={t("account.smtpPort")}
                  value={draft.smtpPort}
                  inputMode="numeric"
                  onChange={(event) =>
                    update("smtpPort", event.currentTarget.value)
                  }
                />
                <Select
                  label={t("account.smtpSecurity")}
                  value={draft.smtpSecurity}
                  data={securityOptions}
                  allowDeselect={false}
                  onChange={(value) =>
                    update("smtpSecurity", (value ?? "tls") as Security)
                  }
                />
              </div>
            </section>

            <section className="account-config-section account-folder-grid">
              <Text className="account-config-label">
                {t("settings.folders")}
              </Text>
              <TextInput
                label={t("settings.archiveFolder")}
                value={draft.archiveMailbox}
                onChange={(event) =>
                  update("archiveMailbox", event.currentTarget.value)
                }
              />
              <TextInput
                label={t("settings.spamFolder")}
                value={draft.spamMailbox}
                onChange={(event) =>
                  update("spamMailbox", event.currentTarget.value)
                }
              />
            </section>

            {selected.auth.type === "password" ? (
              <section className="account-config-section">
                <Text className="account-config-label">
                  {t("settings.credentials")}
                </Text>
                <PasswordInput
                  label={t("settings.newPassword")}
                  description={t("settings.passwordHint")}
                  value={draft.password}
                  onChange={(event) =>
                    update("password", event.currentTarget.value)
                  }
                />
              </section>
            ) : null}

            <section className="account-config-section">
              <Text className="account-config-label">
                {t("settings.mailIndex")}
              </Text>
              <Text size="sm" c="dimmed">
                {t("settings.fullSyncBody")}
              </Text>
              <Button
                variant="light"
                color="orange"
                leftSection={<IconRefresh size={14} />}
                loading={fullSyncing}
                onClick={() => onFullSync(selected)}
              >
                {t("settings.fullSync")}
              </Button>
              {fullSyncProgress ? (
                <div
                  className="sync-indicator"
                  role="status"
                  aria-live="polite"
                >
                  <div className="sync-indicator-copy">
                    <span>
                      {fullSyncDetail(fullSyncProgress, selected.email, t)}
                    </span>
                  </div>
                  <Progress
                    value={
                      fullSyncProgress.total
                        ? Math.min(
                            100,
                            (fullSyncProgress.completed /
                              fullSyncProgress.total) *
                              100,
                          )
                        : 100
                    }
                    animated={
                      fullSyncProgress.total === null ||
                      fullSyncProgress.phase !== "complete"
                    }
                    size="xs"
                    radius="xl"
                    aria-label={fullSyncDetail(
                      fullSyncProgress,
                      selected.email,
                      t,
                    )}
                  />
                </div>
              ) : null}
            </section>

            <Group justify="space-between" className="account-settings-actions">
              <Button
                variant="subtle"
                color="red"
                leftSection={<IconTrash size={14} />}
                loading={removing}
                onClick={() => onRemove(selected)}
              >
                {t("settings.removeAccount")}
              </Button>
              <Button
                disabled={!valid}
                loading={saving}
                onClick={() =>
                  onSave({
                    id: selected.id,
                    ...draft,
                    imapPort: Number(draft.imapPort),
                    smtpPort: Number(draft.smtpPort),
                  })
                }
              >
                {t("actions.save")}
              </Button>
            </Group>
          </Stack>
        )}
      </section>
    </div>
  );
}

function fullSyncDetail(
  progress: SyncProgress,
  email: string,
  t: ReturnType<typeof useTranslation>["t"],
) {
  switch (progress.phase) {
    case "connecting":
      return t("inbox.syncConnecting", { account: email });
    case "authenticating":
      return t("inbox.syncAuthenticating", { account: email });
    case "finding":
      return t("inbox.syncFinding");
    case "threading":
      return t("inbox.syncThreading", {
        completed: progress.completed,
        total: progress.total ?? progress.completed,
      });
    case "downloading":
      return progress.total
        ? t("inbox.syncDownloading", {
            completed: progress.completed,
            total: progress.total,
          })
        : t("inbox.syncPreparing");
    case "saving":
      return t("inbox.syncSaving");
    case "complete":
      return t("inbox.syncFinishing");
  }
}

function toDraft(account: Account): Draft {
  return {
    accountName: account.account_name,
    displayName: account.display_name,
    imapHost: account.imap_host,
    imapPort: String(account.imap_port),
    imapSecurity: account.imap_security,
    smtpHost: account.smtp_host,
    smtpPort: String(account.smtp_port),
    smtpSecurity: account.smtp_security,
    archiveMailbox: account.archive_mailbox,
    spamMailbox: account.spam_mailbox,
    password: "",
  };
}
