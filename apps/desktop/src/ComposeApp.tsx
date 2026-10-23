import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api, SendMessageError } from "./api";
import {
  closeComposeWindow,
  composeSeedFromSendRecovery,
  discardComposeSeed,
  hasNativeComposeRecovery,
  hasNativeComposeSeedHandoff,
  notifyOutbox,
  notifyComposeWindowReady,
  onComposeTarget,
  persistComposeSeed,
  readDatabaseComposeSeed,
  readComposeSeed,
  readNativeComposeRecoveryKey,
  rememberComposeSeed,
  type ComposeSeed,
} from "./composeWindow";
import { Composer, type ComposeSendState } from "./components/Composer";
import { AI_FEATURES_VISIBLE } from "./features";
import { showNativeMessage } from "./nativeFeedback";
import { outboxItemId } from "./outboxIdentity";
import type {
  Account,
  AiMessageTarget,
  AiSettings,
  ComposeAttachment,
  SendFailureKind,
} from "./types";

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
  const [seed, setSeed] = useState(readComposeSeed);
  const [nativeHandoffPending, setNativeHandoffPending] = useState(
    hasNativeComposeSeedHandoff,
  );
  const [nativeRecoveryPending, setNativeRecoveryPending] = useState(
    hasNativeComposeRecovery,
  );
  const [sendState, setSendState] = useState<ComposeSendState>(() =>
    restoredSendState(seed),
  );
  const sendInFlight = useRef(false);
  const submitted = useRef(false);
  const mounted = useRef(true);
  const draftPersistenceFailureReported = useRef(false);
  const sendAttemptId = useRef(seed.sendAttemptId ?? crypto.randomUUID());
  const seedRef = useRef(seed);
  const nativeHandoffAccepted = useRef(!hasNativeComposeSeedHandoff());
  const nativeHandoffReceived = useRef(!hasNativeComposeSeedHandoff());
  const nativeHandoffPersistence = useRef<Promise<ComposeSeed> | undefined>(
    undefined,
  );
  const nativeHandoffBootstrap = useRef<
    Promise<ComposeSeed | undefined> | undefined
  >(undefined);
  const nativeHandoffFailed = useRef(false);
  const nativeRecoveryBootstrap = useRef<Promise<ComposeSeed> | undefined>(
    undefined,
  );
  const nativeRecoveryFailed = useRef(false);
  const [aiSettings] = useState<AiSettings>(() =>
    readJson("dakia.ai", defaultAi),
  );
  const [aiConnected, setAiConnected] = useState(false);

  const failNativeHandoff = () => {
    if (nativeHandoffFailed.current) return;
    nativeHandoffFailed.current = true;
    void (async () => {
      try {
        await showNativeMessage(
          "Could not open a new message",
          "Try again.",
          "error",
        );
      } catch {
        // Feedback must not keep a blank native Compose window alive.
      } finally {
        try {
          await closeComposeWindow();
        } catch {
          // A closing window needs no second error surface.
        }
      }
    })();
  };

  const failNativeRecovery = () => {
    if (nativeRecoveryFailed.current) return;
    nativeRecoveryFailed.current = true;
    void (async () => {
      try {
        await showNativeMessage(
          t("sendRecovery.openErrorTitle"),
          t("sendRecovery.openError"),
          "error",
        );
      } catch {
        // Feedback must not keep an unusable recovery window alive.
      } finally {
        try {
          await closeComposeWindow();
        } catch {
          // A closing window needs no second error surface.
        }
      }
    })();
  };

  const acceptNativeSeed = (acceptedSeed: ComposeSeed) => {
    nativeHandoffAccepted.current = true;
    seedRef.current = acceptedSeed;
    setSeed(acceptedSeed);
    setSendState(restoredSendState(acceptedSeed));
    if (acceptedSeed.sendAttemptId)
      sendAttemptId.current = acceptedSeed.sendAttemptId;
    setNativeHandoffPending(false);
    setNativeRecoveryPending(false);
  };

  useEffect(() => {
    document.title = t("composer.title");
    if (nativeHandoffPending || nativeRecoveryPending) return;
    let current = true;
    readDatabaseComposeSeed()
      .then((databaseSeed) => {
        const nextSeed = databaseSeed ?? seedRef.current;
        if (databaseSeed && current) {
          setSeed(databaseSeed);
          seedRef.current = databaseSeed;
          setSendState(restoredSendState(databaseSeed));
          if (databaseSeed.sendAttemptId)
            sendAttemptId.current = databaseSeed.sendAttemptId;
        }
        return Promise.all([
          api.accounts(),
          nextSeed.forwardMessageId
            ? api
                .forwardAttachments(
                  nextSeed.forwardMessageId,
                  nextSeed.forwardMessageUidValidity ?? null,
                )
                .catch((error) => {
                  showError(error, t("attachments.loadError"));
                  return [];
                })
            : Promise.resolve([]),
        ]);
      })
      .then(([nextAccounts, attachments]) => {
        if (!current) return;
        setAccounts(nextAccounts);
        if (attachments.length) {
          setSeed((current) => ({ ...current, attachments }));
        }
      })
      .catch((error) => current && showError(error))
      .finally(() => current && setLoading(false));
    return () => {
      current = false;
    };
  }, [nativeHandoffPending, nativeRecoveryPending]);

  useEffect(() => {
    if (!nativeRecoveryPending) return;
    let disposed = false;
    const bootstrap =
      nativeRecoveryBootstrap.current ??
      (nativeRecoveryBootstrap.current = (async () => {
        const recoveryKey = readNativeComposeRecoveryKey();
        // The child database is a recovery cache, not authority for a native
        // grant. A stale or forged entry must never replace the exact
        // account, attempt, and message bound to this recovery key.
        const recoveredSeed = composeSeedFromSendRecovery(
          await api.readSendRecoverySeed(recoveryKey),
        );
        await persistComposeSeed(recoveredSeed);
        if (!rememberComposeSeed(recoveredSeed)) {
          throw new Error("Could not preserve the recovered message.");
        }
        await api.acknowledgeSendRecoverySeed(recoveryKey);
        return recoveredSeed;
      })());
    void bootstrap
      .then((recoveredSeed) => {
        if (!disposed) acceptNativeSeed(recoveredSeed);
      })
      .catch(() => !disposed && failNativeRecovery());
    return () => {
      disposed = true;
    };
  }, [nativeRecoveryPending]);

  useEffect(() => {
    let dispose: () => void = () => undefined;
    let disposed = false;
    void onComposeTarget((nextSeed) => {
      if (nativeHandoffAccepted.current || nativeHandoffPersistence.current) {
        return;
      }
      // Receipt is synchronous: a valid large draft may take longer than the
      // five-second arrival deadline to persist in the child WebView.
      nativeHandoffReceived.current = true;
      const persisted = persistComposeSeed(nextSeed).then(() => {
        rememberComposeSeed(nextSeed);
        return nextSeed;
      });
      nativeHandoffPersistence.current = persisted;
      void persisted
        .then((acceptedSeed) => {
          acceptNativeSeed(acceptedSeed);
        })
        .catch(failNativeHandoff);
    })
      .then((unlisten) => {
        if (disposed) {
          unlisten();
          return;
        }
        dispose = unlisten;
        if (!nativeHandoffPending) return;
        const bootstrap =
          nativeHandoffBootstrap.current ??
          (nativeHandoffBootstrap.current = readDatabaseComposeSeed());
        void bootstrap
          .then(async (storedSeed) => {
            if (disposed || nativeHandoffReceived.current) return;
            if (storedSeed) {
              nativeHandoffReceived.current = true;
              rememberComposeSeed(storedSeed);
              acceptNativeSeed(storedSeed);
              return;
            }
            await notifyComposeWindowReady();
          })
          .catch(failNativeHandoff);
      })
      .catch(() => !disposed && failNativeHandoff());
    return () => {
      disposed = true;
      dispose();
    };
  }, []);

  useEffect(() => {
    if (!nativeHandoffPending) return;
    const timeout = window.setTimeout(() => {
      if (!nativeHandoffReceived.current) failNativeHandoff();
    }, 5_000);
    return () => window.clearTimeout(timeout);
  }, [nativeHandoffPending]);

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
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);

  const persistDraft = useCallback(
    async (
      draft: Record<string, unknown>,
      sendState?: "sending" | "delivery_uncertain" | "sent_copy_failed",
    ) => {
      try {
        const attachments = composeAttachments(draft.attachments);
        const recoveredMessage = seedRef.current.recoveryMessage
          ? {
              to: recipientValues(draft.to),
              cc: recipientValues(draft.cc),
              bcc: recipientValues(draft.bcc),
              subject: stringValue(draft.subject),
              bodyText: stringValue(draft.body_text),
              bodyHtml: stringOrNull(draft.body_html),
              inReplyTo: stringOrNull(draft.in_reply_to),
              references: stringOrNull(draft.references),
              attachments: attachments.map((attachment) => ({ ...attachment })),
            }
          : undefined;
        const nextSeed: ComposeSeed = {
          ...seedRef.current,
          sendAttemptId: sendAttemptId.current,
          accountId: stringValue(draft.account_id),
          to: recipientsValue(draft.to),
          cc: recipientsValue(draft.cc),
          bcc: recipientsValue(draft.bcc),
          subject: stringValue(draft.subject),
          body: stringValue(draft.body_text),
          // `null` is part of the canonical ComposeMessage and means plain
          // text only. Turning it into `undefined` here changes the durable
          // recovery payload before the user has edited it.
          bodyHtml:
            draft.body_html === null
              ? null
              : typeof draft.body_html === "string"
                ? draft.body_html
                : undefined,
          inReplyTo: stringOrUndefined(draft.in_reply_to),
          references: stringOrUndefined(draft.references),
          attachments,
          ...(recoveredMessage ? { recoveryMessage: recoveredMessage } : {}),
          sendState,
        };
        // Keep the child-local seed current after a deliberate edit so a
        // reload preserves that edit. For an untouched recovered message the
        // Composer supplies the original bridge values above.
        seedRef.current = nextSeed;
        await persistComposeSeed(nextSeed);
        draftPersistenceFailureReported.current = false;
        return true;
      } catch {
        if (mounted.current && !draftPersistenceFailureReported.current) {
          draftPersistenceFailureReported.current = true;
          void showNativeMessage(
            t("errors.generic"),
            t("composer.draftSaveError"),
            "error",
          );
        }
        return false;
      }
    },
    [t],
  );

  const send = async (draft: Record<string, unknown>) => {
    if (sendInFlight.current) return;
    sendInFlight.current = true;
    // Main and the backend can both clear this optimistic row even after this
    // Compose WebView closes. The durable attempt ID is its stable identity.
    const outboxId = outboxItemId(
      typeof draft.account_id === "string" ? draft.account_id : "",
      sendAttemptId.current,
    );
    const account = accounts.find((item) => item.id === draft.account_id);
    const recipients = Array.isArray(draft.to)
      ? draft.to.filter((value): value is string => typeof value === "string")
      : [];
    setSendState("sending");
    void notifyOutbox({
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
    // The send journal can prevent a duplicate only when this exact draft,
    // attachment set, and attempt ID are durable before SMTP starts. The
    // Outbox remains optimistic while IndexedDB commits this recovery point.
    if (!(await persistDraft(draft, "sending"))) {
      void notifyOutbox({ phase: "finished", id: outboxId });
      if (mounted.current) {
        setSendState("idle");
        sendInFlight.current = false;
      }
      return;
    }
    try {
      await api.send({ ...draft, send_attempt_id: sendAttemptId.current });
    } catch (error) {
      void notifyOutbox({ phase: "finished", id: outboxId });
      const kind =
        error instanceof SendMessageError ? error.kind : "delivery_uncertain";
      showError(
        new Error(sendFailureDetail(kind, t, error)),
        sendFailureTitle(kind, t),
      );
      if (kind === "not_submitted") {
        const persisted = await persistDraft(draft);
        if (!mounted.current) return;
        if (persisted) {
          sendInFlight.current = false;
          setSendState("idle");
        } else {
          // The previous durable record remains `sending`; after a failed
          // terminal write it would restore as uncertain, so do not invite a
          // second submission from this window either.
          setSendState("delivery_uncertain");
        }
      } else {
        const persisted = await persistDraft(draft, kind);
        if (!mounted.current) return;
        setSendState(persisted ? kind : "delivery_uncertain");
      }
      return;
    }
    void notifyOutbox({ phase: "finished", id: outboxId });
    submitted.current = true;
    try {
      await discardComposeSeed();
    } catch {
      if (mounted.current) {
        setSendState("sent");
        void showNativeMessage(
          t("errors.generic"),
          t("composer.deliveredDraftClearError"),
          "error",
        );
      }
      return;
    }
    if (!mounted.current) return;
    setSendState("sent");
    const reducedMotion = window.matchMedia(
      "(prefers-reduced-motion: reduce)",
    ).matches;
    await new Promise<void>((resolve) =>
      window.setTimeout(resolve, reducedMotion ? 0 : 320),
    );
    if (!mounted.current) return;
    try {
      await closeComposeWindow(true);
    } catch (error) {
      showError(error, t("composer.sent"));
    }
  };

  const showError = (error: unknown, title = t("errors.generic")) => {
    void showNativeMessage(
      title,
      error instanceof Error ? error.message : String(error),
      "error",
    );
  };

  if (loading || nativeHandoffPending || nativeRecoveryPending)
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
      onDraftChange={(draft) => {
        if (!submitted.current) {
          void persistDraft(
            draft,
            sendState === "sending" ||
              sendState === "delivery_uncertain" ||
              sendState === "sent_copy_failed"
              ? sendState
              : undefined,
          );
        }
      }}
      onAiDraft={(instruction) =>
        requestAiDraft(seed, aiSettings, instruction, t, showError)
      }
    />
  );
}

async function requestAiDraft(
  seed: {
    contextMessageIds?: string[];
    contextMessageTargets?: AiMessageTarget[];
  },
  settings: AiSettings,
  instruction: string,
  t: (key: string) => string,
  showError: (error: unknown, title?: string) => void,
) {
  const targets = aiTargetsForSeed(seed);
  if (!targets) {
    const failure = new Error(t("composer.aiContextUnavailable"));
    showError(failure, t("composer.aiContextUnavailableTitle"));
    throw failure;
  }
  return api.draft(settings, targets, instruction);
}

function aiTargetsForSeed(seed: {
  contextMessageIds?: string[];
  contextMessageTargets?: AiMessageTarget[];
}): AiMessageTarget[] | undefined {
  const ids = seed.contextMessageIds ?? [];
  if (!ids.length) return [];
  if (
    !seed.contextMessageTargets ||
    seed.contextMessageTargets.length !== ids.length
  ) {
    return undefined;
  }
  const targets = new Map(
    seed.contextMessageTargets.map((target) => [target.messageId, target]),
  );
  if (targets.size !== ids.length) return undefined;
  const ordered = ids.map((id) => targets.get(id));
  if (
    ordered.some(
      (target) =>
        !target ||
        typeof target.expectedUidValidity !== "number" ||
        !Number.isSafeInteger(target.expectedUidValidity) ||
        target.expectedUidValidity <= 0,
    )
  ) {
    return undefined;
  }
  return ordered as AiMessageTarget[];
}

function restoredSendState(seed: { sendState?: string }): ComposeSendState {
  return seed.sendState === "sending"
    ? "delivery_uncertain"
    : seed.sendState === "delivery_uncertain" ||
        seed.sendState === "sent_copy_failed"
      ? seed.sendState
      : "idle";
}

function stringValue(value: unknown) {
  return typeof value === "string" ? value : "";
}

function stringOrUndefined(value: unknown) {
  return typeof value === "string" ? value : undefined;
}

function stringOrNull(value: unknown) {
  return typeof value === "string" ? value : null;
}

function recipientValues(value: unknown) {
  return Array.isArray(value)
    ? value.filter((item): item is string => typeof item === "string")
    : [];
}

function recipientsValue(value: unknown) {
  return Array.isArray(value)
    ? value
        .filter((item): item is string => typeof item === "string")
        .join(", ")
    : "";
}

function composeAttachments(value: unknown): ComposeAttachment[] {
  return Array.isArray(value)
    ? value.filter(
        (attachment): attachment is ComposeAttachment =>
          Boolean(attachment) &&
          typeof attachment === "object" &&
          "filename" in attachment &&
          "mime_type" in attachment &&
          "content_base64" in attachment &&
          "size_bytes" in attachment &&
          typeof attachment.filename === "string" &&
          typeof attachment.mime_type === "string" &&
          typeof attachment.content_base64 === "string" &&
          typeof attachment.size_bytes === "number",
      )
    : [];
}

function sendFailureTitle(kind: SendFailureKind, t: (key: string) => string) {
  switch (kind) {
    case "delivery_uncertain":
      return t("composer.deliveryUncertain");
    case "sent_copy_failed":
      return t("composer.sentCopyFailed");
    case "not_submitted":
      return t("composer.sendError");
  }
}

function sendFailureDetail(
  kind: SendFailureKind,
  t: (key: string) => string,
  error: unknown,
) {
  switch (kind) {
    case "delivery_uncertain":
      return t("composer.deliveryUncertainDetail");
    case "sent_copy_failed":
      return t("composer.sentCopyFailedDetail");
    case "not_submitted":
      return error instanceof Error ? error.message : "Message was not sent.";
  }
}

function readJson<T>(key: string, fallback: T): T {
  try {
    return { ...fallback, ...JSON.parse(localStorage.getItem(key) ?? "{}") };
  } catch {
    return fallback;
  }
}
