import { Button, Loader } from "@mantine/core";
import { IconHistory, IconNotes, IconRobot } from "@tabler/icons-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { api } from "../api";
import type {
  ConversationTarget,
  ThreadContextBundle,
  ThreadContextDocument,
  ThreadContextKind,
  ThreadContextRevision,
} from "../types";

type ContextState = {
  content: string;
  revision: number;
  updatedAt?: string | null;
};

type Props = {
  target: ConversationTarget;
};

const emptyContext: ContextState = { content: "", revision: 0 };

function fromDocument(document: ThreadContextDocument): ContextState {
  return {
    content: document.content,
    revision: document.revision,
    updatedAt: document.updatedAt,
  };
}

function targetKey(target: ConversationTarget) {
  return [
    target.accountId,
    target.localMessageId ?? "",
    target.rfcMessageId ?? "",
    target.threadId ?? "",
    target.mailbox ?? "",
  ].join("\\u0000");
}

function isConflict(error: unknown) {
  return (
    typeof error === "object" &&
    error !== null &&
    "kind" in error &&
    (error as { kind?: unknown }).kind === "conflict"
  );
}

/**
 * A conversation target is captured at render time. Every mutation uses that
 * immutable target, so changing the selected email cannot redirect a pending
 * save or restore to the next conversation.
 */
export function ThreadContextPanel({ target }: Props) {
  const { t } = useTranslation();
  const key = targetKey(target);
  const immutableTarget = useMemo(() => target, [key]);
  const [expanded, setExpanded] = useState(false);
  const [activeKind, setActiveKind] = useState<ThreadContextKind>("notes");
  const [contexts, setContexts] = useState<Record<ThreadContextKind, ContextState>>({
    notes: emptyContext,
    agentMemory: emptyContext,
  });
  const [loading, setLoading] = useState(false);
  const [loadError, setLoadError] = useState(false);
  const [saving, setSaving] = useState<Record<ThreadContextKind, boolean>>({
    notes: false,
    agentMemory: false,
  });
  const [saveError, setSaveError] = useState<ThreadContextKind>();
  const [conflict, setConflict] = useState<ThreadContextKind>();
  const [requiresReview, setRequiresReview] = useState(false);
  const [history, setHistory] = useState<ThreadContextRevision[]>();
  const [historyLoading, setHistoryLoading] = useState(false);
  const [dirty, setDirty] = useState<Record<ThreadContextKind, boolean>>({
    notes: false,
    agentMemory: false,
  });
  const generation = useRef(0);
  const editVersion = useRef(0);
  const editVersions = useRef<Record<ThreadContextKind, number>>({
    notes: 0,
    agentMemory: 0,
  });
  const historyRequest = useRef(0);
  const contextsRef = useRef(contexts);
  contextsRef.current = contexts;

  useEffect(() => {
    generation.current += 1;
    editVersion.current += 1;
    editVersions.current = { notes: 0, agentMemory: 0 };
    historyRequest.current += 1;
    setContexts({ notes: emptyContext, agentMemory: emptyContext });
    setDirty({ notes: false, agentMemory: false });
    setRequiresReview(false);
    setLoading(false);
    setLoadError(false);
    setSaveError(undefined);
    setConflict(undefined);
    setHistory(undefined);
    setHistoryLoading(false);
    setSaving({ notes: false, agentMemory: false });
  }, [key]);

  const setFromBundle = useCallback((bundle: ThreadContextBundle) => {
    setContexts({
      notes: fromDocument(bundle.notes),
      agentMemory: fromDocument(bundle.agentMemory),
    });
    setRequiresReview(bundle.conversation.requiresReview);
  }, []);

  const load = useCallback(async () => {
    const request = ++generation.current;
    setLoading(true);
    setLoadError(false);
    setHistory(undefined);
    setDirty({ notes: false, agentMemory: false });
    try {
      const bundle = await api.threadContext(immutableTarget);
      if (generation.current === request) setFromBundle(bundle);
    } catch (error) {
      if (generation.current === request) setLoadError(true);
    } finally {
      if (generation.current === request) setLoading(false);
    }
  }, [immutableTarget, key, setFromBundle]);

  useEffect(() => {
    if (expanded) void load();
  }, [expanded, key, load]);

  const updateContent = (kind: ThreadContextKind, content: string) => {
    editVersion.current += 1;
    editVersions.current[kind] += 1;
    setContexts((current) => ({
      ...current,
      [kind]: { ...current[kind], content },
    }));
    setDirty((current) => ({ ...current, [kind]: true }));
    setSaveError((current) => (current === kind ? undefined : current));
    setConflict((current) => (current === kind ? undefined : current));
  };

  const save = async (kind: ThreadContextKind) => {
    if (saving[kind]) return;
    const request = generation.current;
    const snapshot = contexts[kind];
    setSaving((current) => ({ ...current, [kind]: true }));
    setSaveError(undefined);
    setConflict(undefined);
    try {
      const next = await api.updateThreadContext({
        target: immutableTarget,
        kind,
        content: snapshot.content,
        expectedRevision: snapshot.revision,
      });
      if (generation.current !== request) return;
      setContexts((current) =>
        current[kind].revision === snapshot.revision
          ? {
              ...current,
              [kind]: {
                ...fromDocument(next),
                // A later edit must remain visible while the earlier save is
                // reconciled. It can use the returned revision on its next
                // explicit save instead of being forced into a false conflict.
                content: current[kind].content,
              },
            }
          : current,
      );
      const hasLaterEdit = contextsRef.current[kind].content !== snapshot.content;
      setDirty((current) => ({ ...current, [kind]: hasLaterEdit }));
    } catch (error) {
      if (generation.current !== request) return;
      if (isConflict(error)) setConflict(kind);
      else setSaveError(kind);
    } finally {
      if (generation.current !== request) return;
      setSaving((current) => ({ ...current, [kind]: false }));
    }
  };

  useEffect(() => {
    if (!expanded || saving.notes || saving.agentMemory || dirty.notes || dirty.agentMemory)
      return;
    const refresh = () => {
      const request = generation.current;
      const edits = editVersion.current;
      void api
        .threadContext(immutableTarget)
        .then((bundle) => {
          if (generation.current === request && editVersion.current === edits)
            setFromBundle(bundle);
        })
        .catch(() => undefined);
    };
    window.addEventListener("focus", refresh);
    return () => window.removeEventListener("focus", refresh);
  }, [dirty.agentMemory, dirty.notes, expanded, immutableTarget, saving, setFromBundle]);

  const showHistory = async () => {
    if (history) {
      historyRequest.current += 1;
      setHistory(undefined);
      return;
    }
    const request = generation.current;
    const requestId = ++historyRequest.current;
    const requestedKind = activeKind;
    setHistoryLoading(true);
    try {
      const next = await api.threadContextHistory(immutableTarget, requestedKind);
      if (generation.current === request && historyRequest.current === requestId)
        setHistory(next);
    } catch {
      if (generation.current === request && historyRequest.current === requestId)
        setSaveError(requestedKind);
    } finally {
      if (generation.current === request && historyRequest.current === requestId)
        setHistoryLoading(false);
    }
  };

  const restore = async (revision: ThreadContextRevision) => {
    const kind = activeKind;
    const snapshot = contexts[kind];
    if (saving[kind]) return;
    const request = generation.current;
    const edits = editVersions.current[kind];
    const optimisticContent = revision.content;
    setSaving((current) => ({ ...current, [kind]: true }));
    setSaveError(undefined);
    setConflict(undefined);
    setContexts((current) => ({
      ...current,
      [kind]: { ...current[kind], content: revision.content },
    }));
    try {
      const next = await api.restoreThreadContext(
        immutableTarget,
        kind,
        revision.id,
        snapshot.revision,
      );
      if (generation.current !== request) return;
      setContexts((current) => ({
        ...current,
        [kind]: {
          ...fromDocument(next),
          content:
            editVersions.current[kind] === edits
              ? next.content
              : current[kind].content,
        },
      }));
      setDirty((current) => ({
        ...current,
        [kind]:
          editVersions.current[kind] !== edits ||
          contextsRef.current[kind].content !== optimisticContent,
      }));
      setHistory(undefined);
    } catch (error) {
      if (generation.current !== request) return;
      if (editVersions.current[kind] === edits) {
        setContexts((current) => ({ ...current, [kind]: snapshot }));
      }
      if (isConflict(error)) setConflict(kind);
      else setSaveError(kind);
    } finally {
      if (generation.current !== request) return;
      setSaving((current) => ({ ...current, [kind]: false }));
    }
  };

  const current = contexts[activeKind];
  return (
    <aside className="thread-context" aria-label={t("reader.context.title")}>
      <div className="thread-context-heading">
        <span>{t("reader.context.title")}</span>
        <Button
          size="compact-xs"
          variant="subtle"
          color="gray"
          onClick={() => setExpanded((value) => !value)}
          aria-expanded={expanded}
        >
          {expanded ? t("reader.context.hide") : t("reader.context.show")}
        </Button>
      </div>
      {expanded ? (
        <div className="thread-context-content">
          <div className="thread-context-tabs" role="tablist">
            <button
              type="button"
              role="tab"
              aria-selected={activeKind === "notes"}
              onClick={() => {
                historyRequest.current += 1;
                setActiveKind("notes");
                setHistory(undefined);
                setHistoryLoading(false);
              }}
            >
              <IconNotes size={15} aria-hidden="true" />
              {t("reader.context.notes")}
            </button>
            <button
              type="button"
              role="tab"
              aria-selected={activeKind === "agentMemory"}
              onClick={() => {
                historyRequest.current += 1;
                setActiveKind("agentMemory");
                setHistory(undefined);
                setHistoryLoading(false);
              }}
            >
              <IconRobot size={15} aria-hidden="true" />
              {t("reader.context.agentMemory")}
            </button>
          </div>
          {loading ? <Loader size="xs" aria-label={t("reader.loadingContent")} /> : null}
          {loadError ? (
            <div className="thread-context-error" role="alert">
              <span>{t("reader.context.loadError")}</span>
              <Button size="compact-xs" variant="subtle" onClick={() => void load()}>
                {t("actions.retry")}
              </Button>
            </div>
          ) : null}
          {!loading && !loadError ? (
            <>
              {requiresReview ? (
                <div className="thread-context-review" role="status">
                  {t("reader.context.requiresReview")}
                </div>
              ) : null}
              <textarea
                className="thread-context-editor"
                value={current.content}
                placeholder={t("reader.context.placeholder")}
                onChange={(event) => updateContent(activeKind, event.currentTarget.value)}
              />
              {saveError === activeKind ? (
                <div className="thread-context-error" role="alert">
                  <span>{t("reader.context.saveError")}</span>
                  <Button size="compact-xs" variant="subtle" onClick={() => void save(activeKind)}>
                    {t("actions.retry")}
                  </Button>
                </div>
              ) : null}
              {conflict === activeKind ? (
                <div className="thread-context-error" role="alert">
                  <span>{t("reader.context.conflict")}</span>
                  <Button size="compact-xs" variant="subtle" onClick={() => void load()}>
                    {t("reader.context.useLatest")}
                  </Button>
                </div>
              ) : null}
              <div className="thread-context-actions">
                <Button
                  size="compact-sm"
                  onClick={() => void save(activeKind)}
                  loading={saving[activeKind]}
                >
                  {saving[activeKind] ? t("reader.context.saving") : t("actions.save")}
                </Button>
                <Button
                  size="compact-sm"
                  variant="subtle"
                  color="gray"
                  leftSection={<IconHistory size={15} />}
                  onClick={() => void showHistory()}
                  loading={historyLoading}
                >
                  {history ? t("reader.context.hideHistory") : t("reader.context.history")}
                </Button>
                {!saving[activeKind] && !saveError && !conflict && !dirty[activeKind] ? (
                  <span className="thread-context-saved">{t("reader.context.saved")}</span>
                ) : null}
              </div>
              {history ? (
                <ol className="thread-context-history" aria-label={t("reader.context.history")}>
                  {history.length ? history.map((revision) => (
                    <li key={revision.id}>
                      <span>{revision.content || "(empty)"}</span>
                      <Button
                        size="compact-xs"
                        variant="subtle"
                        onClick={() => void restore(revision)}
                      >
                        {t("reader.context.restoreVersion", { revision: revision.revision })}
                      </Button>
                    </li>
                  )) : <li>{t("reader.context.historyEmpty")}</li>}
                </ol>
              ) : null}
            </>
          ) : null}
        </div>
      ) : null}
    </aside>
  );
}
