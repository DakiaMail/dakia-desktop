import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "./api";
import {
  closeComposeWindow,
  notifyOutbox,
  readDatabaseComposeSeed,
  readComposeSeed,
} from "./composeWindow";
import { Composer } from "./components/Composer";
import { AI_FEATURES_VISIBLE } from "./features";
import { showNativeMessage } from "./nativeFeedback";
import type { Account, AiSettings } from "./types";

const defaultAi: AiSettings = {
  provider: "ollama",
  baseUrl: "http://127.0.0.1:11434/",
  model: "qwen2.5:1.5b",
  apiKey: "",
  executable: "",
  modelPath: "",
};

export function ComposeApp() {
  const { t } = useTranslation();
  const [accounts, setAccounts] = useState<Account[]>([]);
  const [loading, setLoading] = useState(true);
  const [sendState, setSendState] = useState<"idle" | "sending" | "sent">(
    "idle",
  );
  const [seed, setSeed] = useState(readComposeSeed);
  const [aiSettings] = useState<AiSettings>(() =>
    readJson("dakia.ai", defaultAi),
  );
  const [aiConnected, setAiConnected] = useState(false);

  useEffect(() => {
    document.title = t("composer.title");
    readDatabaseComposeSeed()
      .then((databaseSeed) => {
        const nextSeed = databaseSeed ?? seed;
        if (databaseSeed) setSeed(databaseSeed);
        return Promise.all([
          api.accounts(),
          nextSeed.forwardMessageId
            ? api.forwardAttachments(nextSeed.forwardMessageId)
            : Promise.resolve([]),
        ]);
      })
      .then(([nextAccounts, attachments]) => {
        setAccounts(nextAccounts);
        if (attachments.length) {
          setSeed((current) => ({ ...current, attachments }));
        }
      })
      .catch((error) => showError(error, t("reader.forwardErrorTitle")))
      .finally(() => setLoading(false));
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

  const send = async (draft: Record<string, unknown>) => {
    if (sendState !== "idle") return;
    const outboxId = `outbox-${crypto.randomUUID()}`;
    const account = accounts.find((item) => item.id === draft.account_id);
    const recipients = Array.isArray(draft.to)
      ? draft.to.filter((value): value is string => typeof value === "string")
      : [];
    setSendState("sending");
    await notifyOutbox({
      phase: "sending",
      message: {
        id: outboxId,
        account_id: String(draft.account_id),
        mailbox: "Outbox",
        uid: 0,
        thread_id: outboxId,
        subject: String(draft.subject || t("composer.noSubject")),
        from_name: account?.display_name,
        from_address: account?.email ?? "",
        to_addresses: recipients.join(", "),
        received_at: new Date().toISOString(),
        snippet: String(draft.body_text || "").slice(0, 240),
        body_text: String(draft.body_text || ""),
        body_html:
          typeof draft.body_html === "string" ? draft.body_html : undefined,
        content_state: "complete",
        is_read: true,
        is_flagged: false,
        has_attachments:
          Array.isArray(draft.attachments) && draft.attachments.length > 0,
      },
    });
    try {
      await api.send(draft);
    } catch (error) {
      await notifyOutbox({ phase: "finished", id: outboxId });
      showError(error, t("composer.sendError"));
      setSendState("idle");
      return;
    }
    await notifyOutbox({ phase: "finished", id: outboxId });
    setSendState("sent");
    const reducedMotion = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    await new Promise<void>((resolve) =>
      window.setTimeout(resolve, reducedMotion ? 0 : 320),
    );
    await closeComposeWindow(true);
  };

  const showError = (error: unknown, title = t("errors.generic")) => {
    void showNativeMessage(
      title,
      error instanceof Error ? error.message : String(error),
      "error",
    );
  };

  if (loading)
    return (
      <main className="compose-window compose-window-loading">
        <span>{t("composer.loading")}</span>
      </main>
    );

  if (!accounts.length)
    return (
      <main className="compose-window compose-window-loading">
        <span>{t("composer.noAccount")}</span>
      </main>
    );

  return (
    <Composer
      accounts={accounts}
      seed={seed}
      sendState={sendState}
      aiConnected={aiConnected}
      onSend={(draft) => void send(draft)}
      onAiDraft={(instruction) =>
        api.draft(aiSettings, seed.contextMessageIds ?? [], instruction)
      }
    />
  );
}

function readJson<T>(key: string, fallback: T): T {
  try {
    return { ...fallback, ...JSON.parse(localStorage.getItem(key) ?? "{}") };
  } catch {
    return fallback;
  }
}
