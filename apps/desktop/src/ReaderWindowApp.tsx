import { Loader } from "@mantine/core";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "./api";
import { openComposeWindow } from "./composeWindow";
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
  notifyReaderWindowMutated,
  notifyReaderWindowFailed,
  onReaderTarget,
  readReaderSeed,
  type ReaderWindowMutation,
  type ReaderWindowSeed,
} from "./readerWindow";
import { replyRecipients } from "./recipients";
import { formatReplyHistory } from "./replyHistory";
import type { AiSettings, MailSummary, MailThread } from "./types";
import { concreteThreadMessages } from "./threads";

const defaultAi: AiSettings = {
  provider: "ollama",
  baseUrl: "http://127.0.0.1:11434/",
  model: "qwen2.5:1.5b",
  apiKey: "",
  executable: "",
  modelPath: "",
};

const readerApi = api;

export function ReaderWindowApp() {
  const { t } = useTranslation();
  const [seed, setSeed] = useState<ReaderWindowSeed | undefined>(
    readReaderSeed,
  );
  const [thread, setThread] = useState<MailThread>();
  const [accountEmail, setAccountEmail] = useState<string>();
  const [loading, setLoading] = useState(Boolean(seed));
  const [actionBusy, setActionBusy] = useState(false);
  const [unsubscribeLoading, setUnsubscribeLoading] = useState(false);
  const [aiLoading, setAiLoading] = useState(false);
  const [aiResult, setAiResult] = useState<string>();
  const [aiConnected, setAiConnected] = useState(false);
  const loadGeneration = useRef(0);
  const translate = useRef(t);
  translate.current = t;
  const aiSettings = useMemo(() => readAiSettings(), []);

  const loadThread = useCallback(async (nextSeed: ReaderWindowSeed) => {
    const generation = ++loadGeneration.current;
    setLoading(true);
    setThread(undefined);
    setAiResult(undefined);
    try {
      const [accounts, conversation] = await Promise.all([
        readerApi.accounts(),
        readerApi.conversationForTarget(nextSeed.target),
      ]);
      if (generation !== loadGeneration.current) return;
      setAccountEmail(
        accounts.find((account) => account.id === nextSeed.target.accountId)
          ?.email,
      );
      setThread(conversation ?? undefined);
      if (conversation) document.title = conversation.latest.subject || "Dakia";
      if (!conversation) {
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
        const unread = concreteThreadMessages(conversation).filter(
          (message) => !message.is_read,
        );
        if (unread.length) {
          void Promise.allSettled(
            unread.map((message) => readerApi.setRead(message.id, true)),
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
                  conversation.threadId ?? conversation.latest.thread_id,
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
    void onReaderTarget((nextSeed) => setSeed(nextSeed)).then((unlisten) => {
      if (disposed) unlisten();
      else dispose = unlisten;
    });
    return () => {
      disposed = true;
      loadGeneration.current += 1;
      dispose();
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
        changes.map((message) => readerApi.setRead(message.id, read)),
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
      const updated = await readerApi.setStarred(message.id, starred);
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
          messages.map((message) => message.id),
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
      const content = await readerApi.content(message.id);
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
      });
    } catch (error) {
      showError(error, t("reader.replyErrorTitle"));
    }
  };

  const forward = async (message: MailSummary) => {
    try {
      const content = await readerApi.content(message.id);
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
        contextMessageIds: [message.id],
      });
    } catch (error) {
      showError(error, t("reader.forwardErrorTitle"));
    }
  };

  const unsubscribe = async (message: MailSummary) => {
    if (unsubscribeLoading) return;
    setUnsubscribeLoading(true);
    try {
      await readerApi.unsubscribe(message.id);
      await notifyMutation("unsubscribe", [message.id]);
    } catch (error) {
      showError(error);
    } finally {
      setUnsubscribeLoading(false);
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
