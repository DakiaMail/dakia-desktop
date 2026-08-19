import {
  Accordion,
  Button,
  Group,
  PasswordInput,
  Select,
  Stack,
  Text,
  TextInput,
} from "@mantine/core";
import { openUrl } from "@tauri-apps/plugin-opener";
import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import type { Provider } from "../types";

type Props = {
  providers: Provider[];
  saving: boolean;
  onSave: (draft: Record<string, unknown>, password: string) => void;
  onOAuth: (draft: Record<string, unknown>) => void;
};

export function AccountSetup({ providers, saving, onSave, onOAuth }: Props) {
  const { t } = useTranslation();
  const [email, setEmail] = useState("");
  const [name, setName] = useState("");
  const [password, setPassword] = useState("");
  const [provider, setProvider] = useState<string | null>(null);
  const [username, setUsername] = useState("");
  const [imapHost, setImapHost] = useState("");
  const [imapPort, setImapPort] = useState("");
  const [imapSecurity, setImapSecurity] = useState<"tls" | "start_tls">("tls");
  const [smtpHost, setSmtpHost] = useState("");
  const [smtpPort, setSmtpPort] = useState("");
  const [smtpSecurity, setSmtpSecurity] = useState<"tls" | "start_tls">("tls");
  const [authMode, setAuthMode] = useState<"oauth" | "password">("oauth");
  const detected = useMemo(
    () =>
      providers.find((item) =>
        item.domains.some((domain) =>
          email.toLowerCase().endsWith(`@${domain}`),
        ),
      ),
    [email, providers],
  );
  useEffect(() => {
    if (!provider && detected) setProvider(detected.id);
  }, [detected, provider]);
  const chosen = providers.find((item) => item.id === provider);
  const usesOAuth = Boolean(chosen?.oauth && authMode === "oauth");
  useEffect(() => {
    setAuthMode(chosen?.oauth ? "oauth" : "password");
    if (chosen) {
      setImapSecurity(chosen.imap_security);
      setSmtpSecurity(chosen.smtp_security);
    }
  }, [chosen?.id, chosen?.oauth, chosen?.imap_security, chosen?.smtp_security]);
  const valid =
    email.includes("@") &&
    name.trim() &&
    provider &&
    (usesOAuth || password) &&
    (provider !== "custom" || (imapHost && smtpHost));
  const draft = () => ({
    email,
    display_name: name,
    provider_id: provider,
    username: username || null,
    imap_host: imapHost || null,
    imap_port: imapPort ? Number(imapPort) : null,
    imap_security: imapSecurity,
    smtp_host: smtpHost || null,
    smtp_port: smtpPort ? Number(smtpPort) : null,
    smtp_security: smtpSecurity,
    archive_mailbox: null,
    spam_mailbox: null,
  });
  const submit = () =>
    usesOAuth ? onOAuth(draft()) : onSave(draft(), password);
  return (
    <main className="utility-window account-window">
      <header className="utility-header" data-tauri-drag-region>
        <h1>{t("account.title")}</h1>
        <p>{t("account.intro")}</p>
      </header>
      <div className="utility-scroll">
        <Stack className="account-form">
          <TextInput
            label={t("account.email")}
            value={email}
            onChange={(event) => setEmail(event.currentTarget.value)}
            required
            autoFocus
          />
          <TextInput
            label={t("account.name")}
            value={name}
            onChange={(event) => setName(event.currentTarget.value)}
            required
          />
          <Select
            label={t("account.provider")}
            value={provider}
            onChange={setProvider}
            data={providers.map((item) => ({
              value: item.id,
              label: item.name,
            }))}
            required
          />
          {chosen?.id === "gmail" ? (
            <Text role="status" size="xs" className="gmail-verification-notice">
              {t("account.gmailVerificationNotice")}
            </Text>
          ) : null}
          {usesOAuth ? (
            <Text size="sm" c="dimmed">
              {t("account.browserHint")}
            </Text>
          ) : (
            <>
              <PasswordInput
                label={t("account.password")}
                value={password}
                onChange={(event) => setPassword(event.currentTarget.value)}
                required
              />
              {chosen?.app_password_help ? (
                <button
                  type="button"
                  className="native-link"
                  onClick={() => void openUrl(chosen.app_password_help!)}
                >
                  {t("account.appPasswordHelp", { provider: chosen.name })}
                </button>
              ) : null}
            </>
          )}
          <Accordion variant="contained">
            <Accordion.Item value="advanced">
              <Accordion.Control>{t("account.advanced")}</Accordion.Control>
              <Accordion.Panel>
                <Stack>
                  <TextInput
                    label={t("account.username")}
                    value={username}
                    onChange={(event) => setUsername(event.currentTarget.value)}
                    placeholder={email}
                  />
                  <TextInput
                    label={t("account.imapHost")}
                    value={imapHost}
                    onChange={(event) => setImapHost(event.currentTarget.value)}
                    placeholder={chosen?.imap_host}
                    required={provider === "custom"}
                  />
                  <Group grow>
                    <TextInput
                      label={t("account.imapPort")}
                      value={imapPort}
                      onChange={(event) =>
                        setImapPort(event.currentTarget.value)
                      }
                      placeholder={String(chosen?.imap_port ?? 993)}
                      inputMode="numeric"
                    />
                    <Select
                      label={t("account.imapSecurity")}
                      value={imapSecurity}
                      onChange={(value) =>
                        setImapSecurity((value ?? "tls") as "tls" | "start_tls")
                      }
                      data={[
                        { value: "tls", label: t("account.tls") },
                        { value: "start_tls", label: t("account.startTls") },
                      ]}
                    />
                  </Group>
                  <TextInput
                    label={t("account.smtpHost")}
                    value={smtpHost}
                    onChange={(event) => setSmtpHost(event.currentTarget.value)}
                    placeholder={chosen?.smtp_host}
                    required={provider === "custom"}
                  />
                  <Group grow>
                    <TextInput
                      label={t("account.smtpPort")}
                      value={smtpPort}
                      onChange={(event) =>
                        setSmtpPort(event.currentTarget.value)
                      }
                      placeholder={String(chosen?.smtp_port ?? 465)}
                      inputMode="numeric"
                    />
                    <Select
                      label={t("account.smtpSecurity")}
                      value={smtpSecurity}
                      onChange={(value) =>
                        setSmtpSecurity((value ?? "tls") as "tls" | "start_tls")
                      }
                      data={[
                        { value: "tls", label: t("account.tls") },
                        { value: "start_tls", label: t("account.startTls") },
                      ]}
                    />
                  </Group>
                </Stack>
              </Accordion.Panel>
            </Accordion.Item>
          </Accordion>
          <Button onClick={submit} disabled={!valid} loading={saving}>
            {usesOAuth
              ? t("account.continueWith", { provider: chosen?.name })
              : t("actions.addAccount")}
          </Button>
          {chosen?.oauth ? (
            <Button
              variant="subtle"
              color="gray"
              onClick={() => setAuthMode(usesOAuth ? "password" : "oauth")}
            >
              {usesOAuth
                ? t("account.useAppPassword")
                : t("account.useSecureSignIn")}
            </Button>
          ) : null}
        </Stack>
      </div>
    </main>
  );
}
