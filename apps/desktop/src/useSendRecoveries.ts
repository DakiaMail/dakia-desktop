import { useCallback, useEffect, useRef, useState } from "react";
import { api } from "./api";
import { outboxItemId } from "./outboxIdentity";
import type { SendRecoverySummary } from "./types";

type RecoveryState = {
  key: string;
  items: SendRecoverySummary[];
  nextCursor?: string;
  total: number;
  loading: boolean;
  failed: boolean;
};

const emptyState = (key: string): RecoveryState => ({
  key,
  items: [],
  total: 0,
  loading: false,
  failed: false,
});

/** Keep saved submissions separate from optimistic rows and from mail targets. */
export function useSendRecoveries(accountIds: string[]) {
  const key = JSON.stringify([...new Set(accountIds)].sort());
  const [state, setState] = useState<RecoveryState>(() => ({
    ...emptyState(key),
    loading: accountIds.length > 0,
  }));
  const requestId = useRef(0);
  const busy = useRef(false);

  const request = useCallback(
    async (cursor?: string, completedOutboxId?: string) => {
      const ids: string[] = JSON.parse(key);
      const id = ++requestId.current;
      busy.current = true;
      if (!ids.length) {
        setState(emptyState(key));
        busy.current = false;
        return;
      }
      setState((current) => {
        const previous = current.key === key ? current : emptyState(key);
        const items = completedOutboxId
          ? previous.items.filter(
              (item) =>
                outboxItemId(item.account_id, item.attempt_id) !==
                completedOutboxId,
            )
          : previous.items;
        return {
          ...previous,
          items,
          total: Math.max(
            0,
            previous.total - (previous.items.length - items.length),
          ),
          loading: true,
          failed: false,
        };
      });
      try {
        const page = await api.listSendRecoveries(ids, cursor);
        if (id !== requestId.current) return;
        if (page.items.some((item) => !ids.includes(item.account_id))) {
          throw new Error("Saved message belongs to another account");
        }
        setState((current) => {
          const items = new Map<string, SendRecoverySummary>();
          if (cursor && current.key === key) {
            for (const item of current.items) {
              items.set(`${item.account_id}:${item.attempt_id}`, item);
            }
          }
          for (const item of page.items) {
            items.set(`${item.account_id}:${item.attempt_id}`, item);
          }
          return {
            key,
            items: [...items.values()],
            nextCursor: page.next_cursor ?? undefined,
            total: page.total_count,
            loading: false,
            failed: false,
          };
        });
      } catch {
        if (id !== requestId.current) return;
        setState((current) => ({
          ...(current.key === key ? current : emptyState(key)),
          loading: false,
          failed: true,
        }));
      } finally {
        if (id === requestId.current) busy.current = false;
      }
    },
    [key],
  );

  const refresh = useCallback(
    (completedOutboxId?: string) => request(undefined, completedOutboxId),
    [request],
  );
  useEffect(() => {
    void refresh();
    const onFocus = () => void refresh();
    window.addEventListener("focus", onFocus);
    return () => {
      ++requestId.current;
      busy.current = false;
      window.removeEventListener("focus", onFocus);
    };
  }, [refresh]);

  const current =
    state.key === key
      ? state
      : { ...emptyState(key), loading: accountIds.length > 0 };
  const loadMore = () => {
    if (busy.current || !current.nextCursor) return;
    void request(current.nextCursor);
  };
  return { ...current, refresh, loadMore };
}
