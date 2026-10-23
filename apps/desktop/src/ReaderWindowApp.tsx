import { Loader } from "@mantine/core";
import {
  useCallback,
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useTranslation } from "react-i18next";
import {
  api,
  type SenderCleanupTarget,
  type TrashMessagesFromSenderResult,
} from "./api";
import { openComposeWindow } from "./composeWindow";
import { ActionStatus } from "./components/ActionStatus";
import { Reader } from "./components/Reader";
import { parseEmailAddressMenuAction } from "./emailAddressMenu";
import { AI_FEATURES_VISIBLE } from "./features";
import { formatForwardHistory, forwardSubject } from "./forward";
import {
  conversationActionMessages,
  removeConcreteMessage,
  type MailAction,
} from "./mailActions";
import { showNativeMessage } from "./nativeFeedback";
import { onNativeMenuAction } from "./nativeWindows";
import {
  closeReaderWindow,
  notifyReaderWindowReady,
  notifyReaderWindowMutated,
  notifyReaderWindowFailed,
  onReaderTarget,
  hasNativeReaderSeedHandoff,
  rememberReaderSeed,
  readReaderSeed,
  type ReaderWindowMutation,
  type ReaderWindowSeed,
} from "./readerWindow";
import { replyRecipients } from "./recipients";
import { formatReplyHistory } from "./replyHistory";
import type { AiSettings, MailSummary, MailThread } from "./types";
import { concreteThreadMessages, groupMessages } from "./threads";

const defaultAi: AiSettings = {
  provider: "ollama",
  baseUrl: "http://127.0.0.1:11434/",
  model: "qwen2.5:1.5b",
  apiKey: "",
  executable: "",
  modelPath: "",
};

const readerApi = api;

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

function unsubscribeTargetKey(
  message: Pick<MailSummary, "account_id" | "id" | "mailbox_uid_validity">,
) {
  return `${message.account_id}\u0000${message.id}\u0000${message.mailbox_uid_validity ?? "none"}`;
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
  thread: MailThread,
  target: SenderCleanupTarget,
) {
  const messages = concreteThreadMessages(thread);
  const remaining = messages.filter(
    (message) => !isSenderCleanupMessage(message, target),
  );
  if (remaining.length === messages.length) return thread;
  return remaining.length
    ? { ...thread, ...groupMessages(remaining)[0] }
    : undefined;
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

export function ReaderWindowApp() {
  const { t } = useTranslation();
  const [seed, setSeed] = useState<ReaderWindowSeed | undefined>(
    readReaderSeed,
  );
  const [databaseSeedPending, setDatabaseSeedPending] = useState(
    hasNativeReaderSeedHandoff,
  );
  const [thread, setThread] = useState<MailThread>();
  const [accountEmail, setAccountEmail] = useState<string>();
  const [loading, setLoading] = useState(Boolean(seed) || databaseSeedPending);
  const [actionBusy, setActionBusy] = useState(false);
  const [unsubscribeLoadingTargets, setUnsubscribeLoadingTargets] = useState<
    string[]
  >([]);
  const [senderCleanupLoading, setSenderCleanupLoading] = useState(false);
  const [actionStatus, setActionStatus] = useState<ActionStatusState>();
  const [aiLoading, setAiLoading] = useState(false);
  const [aiResult, setAiResult] = useState<string>();
  const [aiConnected, setAiConnected] = useState(false);
  const loadGeneration = useRef(0);
  const seedGeneration = useRef(0);
  const statusId = useRef(0);
  const senderCleanupInFlightRef = useRef(false);
  const nativeHandoffFailed = useRef(false);
  const nativeHandoffTimeout = useRef<number | undefined>(undefined);
  const unsubscribeInFlightRef = useRef(new Map<string, number>());
  const actionBusyRef = useRef(false);
  const threadRef = useRef<MailThread | undefined>(undefined);
  const focusedMessageRef = useRef<MailSummary | undefined>(undefined);
  const translate = useRef(t);
  translate.current = t;
  const aiSettings = useMemo(() => readAiSettings(), []);
  actionBusyRef.current = actionBusy;
  threadRef.current = thread;

  const failNativeHandoff = () => {
    if (nativeHandoffFailed.current) return;
    nativeHandoffFailed.current = true;
    if (nativeHandoffTimeout.current !== undefined) {
      window.clearTimeout(nativeHandoffTimeout.current);
      nativeHandoffTimeout.current = undefined;
    }
    void (async () => {
      try {
        await showNativeMessage(
          translate.current("errors.generic"),
          translate.current("reader.emptyBody"),
          "warning",
        );
      } catch {
        // Feedback failure must not strand an untrusted blank Reader window.
      } finally {
        try {
          await closeReaderWindow();
        } catch {
          // The window may already be closing.
        }
      }
    })();
  };

  const loadThread = useCallback(async (nextSeed: ReaderWindowSeed) => {
    const generation = ++loadGeneration.current;
    setActionStatus(undefined);
    setLoading(true);
    setThread(undefined);
    setAiResult(undefined);
    try {
      const [accounts, conversation] = await Promise.all([
        readerApi.accounts(),
        readerApi.conversationForTarget(nextSeed.target),
      ]);
      if (generation !== loadGeneration.current) return;
      const account = accounts.find(
        (item) => item.id === nextSeed.target.accountId,
      );
      const scopedConversation =
        account &&
        conversation &&
        concreteThreadMessages(conversation).length > 0 &&
        concreteThreadMessages(conversation).every(
          (message) => message.account_id === nextSeed.target.accountId,
        )
          ? conversation
          : undefined;
      setAccountEmail(account?.email);
      setThread(scopedConversation);
      if (scopedConversation) {
        document.title = scopedConversation.latest.subject || "Dakia";
      }
      if (!scopedConversation) {
        await showNativeMessage(
          translate.current("errors.generic"),
          translate.current("reader.emptyBody"),
          "warning",
        );
        try {
          await notifyReaderWindowFailed({
            accountId: nextSeed.target.accountId,
          });
        } catch (error) {
          showError(error);
        } finally {
          await closeReaderWindow();
        }
      } else {
        const unread = concreteThreadMessages(scopedConversation).filter(
          (message) => !message.is_read,
        );
        if (unread.length) {
          void Promise.allSettled(
            unread.map((message) =>
              readerApi.setRead(
                message.id,
                true,
                message.mailbox_uid_validity ?? null,
              ),
            ),
          )
            .then(async (results) => {
              const succeeded = unread.filter(
                (_message, index) => results[index].status === "fulfilled",
              );
              if (!succeeded.length) {
                const failed = results.find(
                  (result): result is PromiseRejectedResult =>
                    result.status === "rejected",
                );
                if (failed) throw failed.reason;
                return;
              }
              if (generation !== loadGeneration.current) return;
              setThread((current) =>
                current
                  ? updateThread(current, (message) =>
                      succeeded.some((item) => item.id === message.id)
                        ? { ...message, is_read: true }
                        : message,
                    )
                  : current,
              );
              await notifyReaderWindowMutated({
                accountId: nextSeed.target.accountId,
                threadId:
                  scopedConversation.threadId ??
                  scopedConversation.latest.thread_id,
                messageIds: succeeded.map((message) => message.id),
                mutation: "read",
              });
              const failed = results.find(
                (result): result is PromiseRejectedResult =>
                  result.status === "rejected",
              );
              if (failed) throw failed.reason;
            })
            .catch(showError);
        }
      }
    } catch (error) {
      if (generation !== loadGeneration.current) return;
      showError(error);
      try {
        await notifyReaderWindowFailed({
          accountId: nextSeed.target.accountId,
        });
      } catch (notificationError) {
        showError(notificationError);
      } finally {
        await closeReaderWindow();
      }
    } finally {
      if (generation === loadGeneration.current) setLoading(false);
    }
  }, []);

  useEffect(() => {
    document.title = "Dakia";
    if (seed) void loadThread(seed);
  }, [loadThread, seed]);

  useEffect(() => {
    let dispose: () => void = () => undefined;
    let disposed = false;
    void onReaderTarget((nextSeed) => {
      seedGeneration.current += 1;
      loadGeneration.current += 1;
      focusedMessageRef.current = undefined;
      setActionStatus(undefined);
      setDatabaseSeedPending(false);
      rememberReaderSeed(nextSeed);
      setSeed(nextSeed);
    })
      .then((unlisten) => {
        if (disposed) {
          unlisten();
        } else {
          dispose = unlisten;
          if (hasNativeReaderSeedHandoff()) {
            void notifyReaderWindowReady().catch(failNativeHandoff);
          }
        }
      })
      .catch(() => {
        if (!disposed) failNativeHandoff();
      });
    return () => {
      disposed = true;
      loadGeneration.current += 1;
      dispose();
    };
  }, []);

  useEffect(() => {
    if (!hasNativeReaderSeedHandoff()) return;
    const timeout = window.setTimeout(() => {
      if (!hasNativeReaderSeedHandoff()) return;
      failNativeHandoff();
    }, 5_000);
    nativeHandoffTimeout.current = timeout;
    return () => {
      window.clearTimeout(timeout);
      if (nativeHandoffTimeout.current === timeout) {
        nativeHandoffTimeout.current = undefined;
      }
    };
  }, []);

  useEffect(() => {
    if (!AI_FEATURES_VISIBLE) return;
    let current = true;
    void readerApi
      .aiAvailable(aiSettings)
      .then((available) => current && setAiConnected(available))
      .catch(() => current && setAiConnected(false));
    return () => {
      current = false;
    };
  }, [aiSettings]);

  const messages = thread?.messages ?? [];
  const focusedMessage =
    messages.find((message) => message.id === seed?.focusedMessageId) ??
    messages.at(-1);
  useLayoutEffect(() => {
    focusedMessageRef.current = focusedMessage;
  }, [focusedMessage]);
  const unsubscribeLoading = Boolean(
    focusedMessage &&
    unsubscribeLoadingTargets.includes(unsubscribeTargetKey(focusedMessage)),
  );

  const notifyMutation = useCallback(
    async (
      mutation: ReaderWindowMutation["mutation"],
      messageIds: string[],
    ) => {
      if (!seed) return;
      await notifyReaderWindowMutated({
        accountId: seed.target.accountId,
        threadId: seed.target.threadId,
        messageIds,
        mutation,
      });
    },
    [seed],
  );

  const applyMailboxAction = async (
    mutation: Extract<
      ReaderWindowMutation["mutation"],
      "archive" | "spam" | "not_spam" | "trash"
    >,
  ) => {
    if (actionBusy || !thread) return;
    const actionTargets = conversationActionMessages(
      thread,
      seed?.target.mailbox ?? "INBOX",
      mutation as MailAction,
    );
    if (!actionTargets.length) return;
    setActionBusy(true);
    try {
      const results = await Promise.allSettled(
        actionTargets.map((message) =>
          readerApi.action(
            message.account_id,
            message.mailbox,
            message.uid,
            mutation,
            message.id,
            message.mailbox_uid_validity ?? null,
          ),
        ),
      );
      const succeeded = actionTargets.filter(
        (_message, index) => results[index].status === "fulfilled",
      );
      if (succeeded.length)
        await notifyMutation(
          mutation,
          succeeded.map((message) => message.id),
        );
      const failed = results.find(
        (result): result is PromiseRejectedResult =>
          result.status === "rejected",
      );
      if (failed) {
        if (seed) {
          const refreshed = await readerApi.conversationForTarget(seed.target);
          setThread(refreshed ?? undefined);
        }
        throw failed.reason;
      }
      await closeReaderWindow();
    } catch (error) {
      showError(error);
    } finally {
      setActionBusy(false);
    }
  };

  const toggleRead = async (read: boolean) => {
    if (actionBusy) return;
    if (!thread) return;
    const changes = concreteThreadMessages(thread).filter(
      (message) => message.is_read !== read,
    );
    if (!changes.length) return;
    setActionBusy(true);
    try {
      const results = await Promise.allSettled(
        changes.map((message) =>
          readerApi.setRead(
            message.id,
            read,
            message.mailbox_uid_validity ?? null,
          ),
        ),
      );
      const succeeded = changes.filter(
        (_message, index) => results[index].status === "fulfilled",
      );
      setThread((current) =>
        current
          ? updateThread(current, (message) =>
              succeeded.some((change) => change.id === message.id)
                ? { ...message, is_read: read }
                : message,
            )
          : current,
      );
      if (succeeded.length)
        await notifyMutation(
          "read",
          succeeded.map((message) => message.id),
        );
      const failed = results.find(
        (result): result is PromiseRejectedResult =>
          result.status === "rejected",
      );
      if (failed) throw failed.reason;
    } catch (error) {
      showError(error);
    } finally {
      setActionBusy(false);
    }
  };

  const permanentlyDelete = async (message: MailSummary) => {
    if (actionBusy || !thread) return;
    setActionBusy(true);
    try {
      await readerApi.action(
        message.account_id,
        message.mailbox,
        message.uid,
        "delete",
        message.id,
        message.mailbox_uid_validity ?? null,
      );
      const remaining = removeConcreteMessage(thread, message);
      setThread(remaining);
      await notifyMutation("delete", [message.id]);
      if (!remaining) await closeReaderWindow();
    } catch (error) {
      showError(error);
    } finally {
      setActionBusy(false);
    }
  };

  const toggleStar = async (message: MailSummary, starred: boolean) => {
    if (actionBusy) return;
    setActionBusy(true);
    try {
      const updated = await readerApi.setStarred(
        message.id,
        starred,
        message.mailbox_uid_validity ?? null,
      );
      setThread((current) =>
        current
          ? updateThread(current, (item) =>
              item.id === message.id
                ? { ...item, ...updated, is_flagged: starred }
                : item,
            )
          : current,
      );
      await notifyMutation("star", [message.id]);
    } catch (error) {
      showError(error);
    } finally {
      setActionBusy(false);
    }
  };

  const summarize = async () => {
    if (aiLoading || !messages.length) return;
    setAiLoading(true);
    try {
      setAiResult(
        await readerApi.summarize(
          aiSettings,
          messages.map((message) => ({
            messageId: message.id,
            expectedUidValidity: message.mailbox_uid_validity ?? null,
          })),
        ),
      );
    } catch (error) {
      showError(error, t("ai.error"));
    } finally {
      setAiLoading(false);
    }
  };

  const reply = async (message: MailSummary, replyAll = false) => {
    const recipientMessage =
      accountEmail &&
      message.from_address.toLowerCase() === accountEmail.toLowerCase()
        ? ([...messages]
            .reverse()
            .find(
              (item) =>
                item.from_address.toLowerCase() !== accountEmail.toLowerCase(),
            ) ?? message)
        : message;
    const recipients = replyRecipients(
      recipientMessage,
      accountEmail,
      replyAll,
    );
    if (!recipients) return;
    try {
      const content = await readerApi.content(
        message.id,
        message.mailbox_uid_validity ?? null,
      );
      const history = formatReplyHistory({
        message,
        bodyText: content.body_text,
        bodyHtml: content.body_html,
        formatCitation: ({ date, sender }) =>
          t("reader.replyCitation", { date, sender }),
      });
      const prefix = t("reader.replyPrefix");
      openComposeWindow({
        accountId: message.account_id,
        to: recipients.to,
        ...(recipients.cc ? { cc: recipients.cc } : {}),
        subject: message.subject.toLowerCase().startsWith(prefix.toLowerCase())
          ? message.subject
          : `${prefix} ${message.subject}`,
        body: history.body,
        bodyHtml: history.bodyHtml,
        inReplyTo: message.message_id ?? undefined,
        references: [message.reference_ids, message.message_id]
          .filter(Boolean)
          .join(" "),
        contextMessageIds: messages.map((item) => item.id),
        contextMessageTargets: messages.map((item) => ({
          messageId: item.id,
          expectedUidValidity: item.mailbox_uid_validity ?? null,
        })),
      });
    } catch (error) {
      showError(error, t("reader.replyErrorTitle"));
    }
  };

  const forward = async (message: MailSummary) => {
    try {
      const content = await readerApi.content(
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
      showError(error, t("reader.forwardErrorTitle"));
    }
  };

  const sendAgain = async (message: MailSummary) => {
    try {
      const content = await readerApi.content(
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
      showError(error, t("reader.sendAgainErrorTitle"));
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

  const refreshAfterSenderCleanup = async (expectedGeneration: number) => {
    if (!seed) return;
    const refreshed = await readerApi.conversationForTarget(seed.target);
    if (loadGeneration.current !== expectedGeneration) return;
    setThread(refreshed ?? undefined);
    if (!refreshed) await closeReaderWindow();
  };

  const trashMessagesFromSender = async (
    target: SenderCleanupTarget,
    expectedGeneration: number,
  ) => {
    if (loadGeneration.current !== expectedGeneration) return;
    const currentThread = threadRef.current;
    if (
      senderCleanupInFlightRef.current ||
      actionBusyRef.current ||
      !currentThread
    )
      return;
    senderCleanupInFlightRef.current = true;
    const cleanupGeneration = loadGeneration.current;
    const removedMessageIds = concreteThreadMessages(currentThread)
      .filter((message) => isSenderCleanupMessage(message, target))
      .map((message) => message.id);
    setActionStatus(undefined);
    setSenderCleanupLoading(true);
    setThread((current) =>
      current ? removeSenderCleanupMessages(current, target) : current,
    );
    let result: TrashMessagesFromSenderResult;
    try {
      result = await readerApi.trashMessagesFromSender(
        target.accountId,
        target.senderAddress,
      );
    } catch (error) {
      if (loadGeneration.current === cleanupGeneration)
        setThread(currentThread);
      try {
        await refreshAfterSenderCleanup(cleanupGeneration);
      } catch (refreshError) {
        console.warn("Could not reconcile sender cleanup", refreshError);
      }
      if (loadGeneration.current === cleanupGeneration)
        showStatus(t("feedback.unsubscribeCleanupFailed"), "error");
      console.warn("Could not move sender messages to Trash", error);
      senderCleanupInFlightRef.current = false;
      setSenderCleanupLoading(false);
      return;
    }
    if (
      loadGeneration.current === cleanupGeneration &&
      (result.failed > 0 ||
        result.moved === 0 ||
        result.moved !== result.matched)
    )
      setThread(currentThread);
    try {
      await notifyMutation("trash", removedMessageIds);
    } catch (error) {
      console.warn(
        "Could not notify the main window about sender cleanup",
        error,
      );
    }
    try {
      await refreshAfterSenderCleanup(cleanupGeneration);
    } catch (error) {
      console.warn("Could not reconcile sender cleanup", error);
    }
    if (loadGeneration.current === cleanupGeneration)
      showStatus(
        senderCleanupOutcome(t, result),
        result.failed || result.moved !== result.matched ? "error" : "success",
      );
    senderCleanupInFlightRef.current = false;
    setSenderCleanupLoading(false);
  };

  const unsubscribe = async (message: MailSummary) => {
    const requestGeneration = loadGeneration.current;
    const target = unsubscribeTargetKey(message);
    const isCurrentTarget = () =>
      focusedMessageRef.current !== undefined &&
      unsubscribeTargetKey(focusedMessageRef.current) === target;
    if (unsubscribeInFlightRef.current.has(target)) return;
    unsubscribeInFlightRef.current.set(target, requestGeneration);
    setUnsubscribeLoadingTargets((current) =>
      current.includes(target) ? current : [...current, target],
    );
    try {
      let result: Awaited<ReturnType<typeof readerApi.unsubscribe>>;
      try {
        result = await readerApi.unsubscribe(
          message.id,
          message.mailbox_uid_validity ?? null,
        );
      } catch (error) {
        if (isCurrentTarget()) showError(error);
        return;
      }
      if (!isCurrentTarget()) return;
      try {
        await notifyMutation("unsubscribe", [message.id]);
      } catch (error) {
        console.warn(
          "Could not notify the main window about unsubscribe",
          error,
        );
      }
      if (!isCurrentTarget()) return;
      const completionGeneration = loadGeneration.current;
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
                void trashMessagesFromSender(
                  result.cleanupTarget!,
                  completionGeneration,
                ),
              disabled: senderCleanupLoading,
            }
          : undefined,
      );
    } finally {
      if (unsubscribeInFlightRef.current.get(target) === requestGeneration) {
        unsubscribeInFlightRef.current.delete(target);
        setUnsubscribeLoadingTargets((current) =>
          current.filter((currentTarget) => currentTarget !== target),
        );
      }
    }
  };

  useEffect(() => {
    let dispose: () => void = () => undefined;
    let disposed = false;
    void onNativeMenuAction((action) => {
      const addressAction = parseEmailAddressMenuAction(action);
      if (
        addressAction?.kind === "compose" &&
        addressAction.accountId === focusedMessage?.account_id
      ) {
        openComposeWindow({
          accountId: addressAction.accountId,
          to: addressAction.address,
        });
        return;
      }
      if (!focusedMessage) return;
      switch (action) {
        case "copy-email-address-failed":
          void showNativeMessage(
            t("errors.generic"),
            t("errors.copyFailed"),
            "error",
          );
          break;
        case "reply":
          void reply(focusedMessage);
          break;
        case "forward":
          void forward(focusedMessage);
          break;
        case "archive":
          void applyMailboxAction("archive");
          break;
        case "spam":
          void applyMailboxAction(
            focusedMessage.mailbox.split("::", 1)[0] === "Spam"
              ? "not_spam"
              : "spam",
          );
          break;
      }
    }).then((unlisten) => {
      if (disposed) unlisten();
      else dispose = unlisten;
    });
    return () => {
      disposed = true;
      dispose();
    };
  }, [focusedMessage, thread, actionBusy]);

  if (loading)
    return (
      <main className="reader-window reader-window-loading">
        <div className="reader-window-titlebar" data-tauri-drag-region />
        <Loader size="sm" />
      </main>
    );

  return (
    <div className="reader-window">
      <div className="reader-window-titlebar" data-tauri-drag-region />
      <Reader
        message={focusedMessage}
        messages={messages}
        focusedMessageId={focusedMessage?.id}
        accountEmail={accountEmail}
        aiResult={aiResult}
        aiLoading={aiLoading}
        aiConnected={aiConnected}
        actionsDisabled={actionBusy}
        onArchive={() => void applyMailboxAction("archive")}
        onSpam={() =>
          void applyMailboxAction(
            focusedMessage?.mailbox.split("::", 1)[0] === "Spam"
              ? "not_spam"
              : "spam",
          )
        }
        onTrash={() => void applyMailboxAction("trash")}
        onPermanentDelete={permanentlyDelete}
        onReply={(message) => void reply(message)}
        onReplyAll={(message) => void reply(message, true)}
        onForward={(message) => void forward(message)}
        onSendAgain={(message) => void sendAgain(message)}
        onToggleRead={(read) => void toggleRead(read)}
        onSummarize={() => void summarize()}
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
          void readerApi
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

function updateThread(
  thread: MailThread,
  update: (message: MailSummary) => MailSummary,
): MailThread {
  return {
    ...thread,
    messages: thread.messages.map(update),
    sourceMessages: thread.sourceMessages?.map(update),
    latest: update(thread.latest),
  };
}

function readAiSettings(): AiSettings {
  try {
    return {
      ...defaultAi,
      ...JSON.parse(window.localStorage.getItem("dakia.ai") ?? "{}"),
    };
  } catch {
    return defaultAi;
  }
}
