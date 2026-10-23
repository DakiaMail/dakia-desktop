import { act, renderHook, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { SendRecoveryPage, SendRecoverySummary } from "./types";
import { useSendRecoveries } from "./useSendRecoveries";

const mocks = vi.hoisted(() => ({ list: vi.fn() }));
vi.mock("./api", () => ({ api: { listSendRecoveries: mocks.list } }));

const row = (account: string, attempt = "attempt"): SendRecoverySummary => ({
  account_id: account,
  attempt_id: attempt,
  subject: `${account} saved message`,
  status: "sent_copy_failed",
  updated_at: "2026-10-22T00:00:00Z",
});
const page = (
  items: SendRecoverySummary[],
  next_cursor: string | null = null,
  total_count = items.length,
): SendRecoveryPage => ({ items, next_cursor, total_count });
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => (resolve = done));
  return { promise, resolve };
}

beforeEach(() => {
  mocks.list.mockReset().mockResolvedValue(page([]));
});

describe("saved send recovery listing", () => {
  it("immediately hides another account and ignores its late response", async () => {
    const a = deferred<SendRecoveryPage>();
    const b = deferred<SendRecoveryPage>();
    mocks.list.mockReturnValueOnce(a.promise).mockReturnValueOnce(b.promise);
    const { result, rerender } = renderHook(
      ({ ids }) => useSendRecoveries(ids),
      { initialProps: { ids: ["a"] } },
    );
    rerender({ ids: ["b"] });
    expect(result.current.items).toEqual([]);
    await act(async () => b.resolve(page([row("b")])));
    await act(async () => a.resolve(page([row("a")])));
    expect(result.current.items).toEqual([row("b")]);
    expect(mocks.list.mock.calls).toEqual([
      [["a"], undefined],
      [["b"], undefined],
    ]);
  });

  it("keeps account identity during pagination and prevents repeated load clicks", async () => {
    const more = deferred<SendRecoveryPage>();
    mocks.list
      .mockResolvedValueOnce(page([row("a")], "opaque-cursor", 2))
      .mockReturnValueOnce(more.promise);
    const { result } = renderHook(() => useSendRecoveries(["a", "b"]));
    await waitFor(() => expect(result.current.items).toHaveLength(1));
    act(() => {
      result.current.loadMore();
      result.current.loadMore();
    });
    expect(mocks.list).toHaveBeenCalledTimes(2);
    expect(mocks.list).toHaveBeenLastCalledWith(["a", "b"], "opaque-cursor");
    await act(async () => more.resolve(page([row("a"), row("b")], null, 2)));
    expect(result.current.items).toEqual([row("a"), row("b")]);
    expect(result.current.total).toBe(2);
  });

  it("retains known messages and reports failure instead of an empty result", async () => {
    mocks.list.mockResolvedValueOnce(page([row("a")]));
    const { result } = renderHook(() => useSendRecoveries(["a"]));
    await waitFor(() => expect(result.current.items).toHaveLength(1));
    mocks.list.mockRejectedValueOnce(new Error("storage failure"));
    await act(async () => result.current.refresh());
    expect(result.current.items).toEqual([row("a")]);
    expect(result.current.failed).toBe(true);
    expect(result.current.loading).toBe(false);
  });

  it("rejects a response outside the immutable account filter", async () => {
    mocks.list.mockResolvedValueOnce(page([row("b")]));
    const { result } = renderHook(() => useSendRecoveries(["a"]));
    await waitFor(() => expect(result.current.failed).toBe(true));
    expect(result.current.items).toEqual([]);
  });

  it("does not interpret an empty account filter as every account", async () => {
    const a = deferred<SendRecoveryPage>();
    mocks.list.mockReturnValueOnce(a.promise);
    const { result, rerender } = renderHook(
      ({ ids }) => useSendRecoveries(ids),
      { initialProps: { ids: ["a"] } },
    );
    rerender({ ids: [] });
    await act(async () => a.resolve(page([row("a")])));
    expect(result.current.items).toEqual([]);
    expect(result.current.total).toBe(0);
    expect(mocks.list).toHaveBeenCalledTimes(1);
  });

  it("refreshes on app focus after a compose window has closed", async () => {
    mocks.list
      .mockResolvedValueOnce(page([]))
      .mockResolvedValueOnce(page([row("a")]));
    const { result, unmount } = renderHook(() => useSendRecoveries(["a"]));
    await waitFor(() => expect(result.current.loading).toBe(false));
    act(() => window.dispatchEvent(new Event("focus")));
    await waitFor(() => expect(result.current.items).toEqual([row("a")]));
    unmount();
    act(() => window.dispatchEvent(new Event("focus")));
    expect(mocks.list).toHaveBeenCalledTimes(2);
  });

  it("does not let a late page undo a newer refresh", async () => {
    const more = deferred<SendRecoveryPage>();
    mocks.list
      .mockResolvedValueOnce(page([row("a", "old")], "next", 2))
      .mockReturnValueOnce(more.promise)
      .mockResolvedValueOnce(page([row("a", "new")]));
    const { result } = renderHook(() => useSendRecoveries(["a"]));
    await waitFor(() => expect(result.current.nextCursor).toBe("next"));
    act(() => result.current.loadMore());
    await act(async () => result.current.refresh());
    await act(async () => more.resolve(page([row("a", "stale")])));
    expect(result.current.items).toEqual([row("a", "new")]);
    expect(result.current.nextCursor).toBeUndefined();
  });
});
