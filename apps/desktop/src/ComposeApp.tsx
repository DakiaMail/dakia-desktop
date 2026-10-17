import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "./api";
import {
  closeComposeWindow,
  notifyOutbox,
  readDatabaseComposeSeed,
  readComposeSeed,
} from "./composeWindow";
import type { ComposeSeed } from "./composeWindow";
import { Composer } from "./components/Composer";
import { AI_FEATURES_VISIBLE } from "./features";
import { showNativeMessage } from "./nativeFeedback";
import type { Account, AiSettings, LocalDraft } from "./types";

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
  const [localDraft, setLocalDraft] = useState<LocalDraft>();
  const [savedDrafts, setSavedDrafts] = useState<LocalDraft[]>([]);
  const [draftAccountId, setDraftAccountId] = useState<string>();
  const draftTimer = useRef<number | undefined>(undefined);
  const draftRef = useRef<LocalDraft | undefined>(undefined);
  const pendingDraft = useRef<Record<string, unknown> | undefined>(undefined);
  const draftSaveInFlight = useRef(false);
  const draftSavePromise = useRef<Promise<void> | undefined>(undefined);
  draftRef.current = localDraft;

  useEffect(() => {
    document.title = t("composer.title");
    readDatabaseComposeSeed()
      .then((databaseSeed) => {
        const nextSeed = databaseSeed ?? seed;
        if (databaseSeed) setSeed(databaseSeed);
        return Promise.all([
          api.accounts(),
          api.localDrafts(),
          nextSeed.draftId ? api.localDraft(nextSeed.draftId) : Promise.resolve(null),
          nextSeed.forwardMessageId
            ? api
                .forwardAttachments(nextSeed.forwardMessageId)
                .catch((error) => {
                  showError(error, t("attachments.loadError"));
                  return [];
                })
            : Promise.resolve([]),
        ]);
      })
      .then(([nextAccounts, drafts, storedDraft, attachments]) => {
        setAccounts(nextAccounts);
        setSavedDrafts(drafts.filter((draft) => draft.status !== "sent"));
        if (storedDraft) {
          draftRef.current = storedDraft;
          setLocalDraft(storedDraft);
          setDraftAccountId(storedDraft.accountId);
          setSeed((current) => ({ ...current, ...seedFromLocalDraft(storedDraft) }));
        }
        if (attachments.length) {
          setSeed((current) => ({ ...current, attachments }));
        }
      })
      .catch((error) => showError(error))
      .finally(() => setLoading(false));
  }, []);

  useEffect(
    () => () => {
      if (draftTimer.current) window.clearTimeout(draftTimer.current);
    },
    [],
  );

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

  const saveDraftNow = useCallback(async (draft: Record<string, unknown>) => {
    const current = draftRef.current;
    const next = current
      ? await api.updateLocalDraft(current.id, current.revision, draft)
      : await api.createLocalDraft(draft);
    draftRef.current = next;
    setLocalDraft(next);
    setSavedDrafts((currentDrafts) => [
      next,
      ...currentDrafts.filter((draft) => draft.id !== next.id),
    ]);
    return next;
  }, []);

  const flushDraftSave = useCallback(() => {
    if (draftSaveInFlight.current)
      return draftSavePromise.current ?? Promise.resolve();
    draftSaveInFlight.current = true;
    const save = (async () => {
      try {
        while (pendingDraft.current) {
          const nextDraft = pendingDraft.current;
          pendingDraft.current = undefined;
          try {
            await saveDraftNow(nextDraft);
          } catch (error) {
            const current = draftRef.current;
            if (current && isDraftConflict(error)) {
              const latest = await api.localDraft(current.id).catch(() => null);
              if (latest) {
                draftRef.current = latest;
                setLocalDraft(latest);
                setSavedDrafts((drafts) => [
                  latest,
                  ...drafts.filter((draft) => draft.id !== latest.id),
                ]);
              }
            }
            showError(error, t("composer.saveError"));
            return;
          }
        }
      } finally {
        draftSaveInFlight.current = false;
        draftSavePromise.current = undefined;
        if (pendingDraft.current) void flushDraftSave();
      }
    })();
    draftSavePromise.current = save;
    return save;
  }, [saveDraftNow, t]);

  const scheduleDraftSave = useCallback((draft: Record<string, unknown>) => {
    const accountId = typeof draft.account_id === "string" ? draft.account_id : undefined;
    if (accountId) setDraftAccountId((current) => current ?? accountId);
    if (draftTimer.current) window.clearTimeout(draftTimer.current);
    draftTimer.current = window.setTimeout(() => {
      pendingDraft.current = draft;
      void flushDraftSave();
    }, 550);
  }, [flushDraftSave]);

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
      if (draftTimer.current) window.clearTimeout(draftTimer.current);
      pendingDraft.current = undefined;
      await flushDraftSave();
      const stored = await saveDraftNow(draft);
      await api.sendLocalDraft(stored.id, stored.revision);
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

  const openDraft = useCallback((draft: LocalDraft) => {
    if (draftTimer.current) window.clearTimeout(draftTimer.current);
    draftRef.current = draft;
    setLocalDraft(draft);
    setDraftAccountId(draft.accountId);
    setSeed(seedFromLocalDraft(draft));
  }, []);

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
      key={localDraft?.id ?? "new"}
      accounts={accounts}
      seed={seed}
      sendState={sendState}
      aiConnected={aiConnected}
      accountLocked={Boolean(localDraft || draftAccountId)}
      savedDrafts={savedDrafts}
      activeDraftId={localDraft?.id}
      onSend={(draft) => void send(draft)}
      onDraftChange={scheduleDraftSave}
      onOpenDraft={openDraft}
      onAiDraft={(instruction) =>
        api.draft(aiSettings, seed.contextMessageIds ?? [], instruction)
      }
    />
  );
}

function isDraftConflict(error: unknown) {
  return String(error).toLowerCase().includes("revision conflict");
}

function readJson<T>(key: string, fallback: T): T {
  try {
    return { ...fallback, ...JSON.parse(localStorage.getItem(key) ?? "{}") };
  } catch {
    return fallback;
  }
}

function seedFromLocalDraft(draft: LocalDraft): ComposeSeed {
  const compose = draft.compose;
  const list = (name: string) =>
    Array.isArray(compose[name])
      ? compose[name].filter((value): value is string => typeof value === "string").join(", ")
      : "";
  const attachments = Array.isArray(compose.attachments)
    ? compose.attachments.flatMap((value) => {
        if (!value || typeof value !== "object") return [];
        const attachment = value as Record<string, unknown>;
        if (
          typeof attachment.filename !== "string" ||
          typeof attachment.mime_type !== "string" ||
          typeof attachment.content_base64 !== "string"
        )
          return [];
        return [{
          filename: attachment.filename,
          mime_type: attachment.mime_type,
          content_base64: attachment.content_base64,
          size_bytes: Math.floor((attachment.content_base64.length * 3) / 4),
        }];
      })
    : [];
  return {
    draftId: draft.id,
    accountId: draft.accountId,
    to: list("to"),
    cc: list("cc"),
    bcc: list("bcc"),
    subject: typeof compose.subject === "string" ? compose.subject : "",
    body: typeof compose.body_text === "string" ? compose.body_text : "",
    bodyHtml: typeof compose.body_html === "string" ? compose.body_html : undefined,
    inReplyTo: typeof compose.in_reply_to === "string" ? compose.in_reply_to : undefined,
    references: typeof compose.references === "string" ? compose.references : undefined,
    attachments,
  };
}
