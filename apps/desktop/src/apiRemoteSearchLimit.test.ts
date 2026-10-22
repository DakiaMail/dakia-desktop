// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invoke = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({
  invoke,
  Channel: class {},
}));

describe("remote search API boundary", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
  });

  afterEach(() => {
    Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
  });

  it("asks the native boundary for at most 500 account-scoped mailbox results", async () => {
    invoke.mockResolvedValue({ messages: [], limitReached: false });
    const { api } = await import("./api");

    const result = await api.searchRemote(
      "  exact provider query  ",
      ["account-a", "account-b"],
      "Archive/Receipts",
      true,
      false,
    );

    expect(invoke).toHaveBeenCalledWith("search_remote", {
      query: {
        text: "  exact provider query  ",
        account_ids: ["account-a", "account-b"],
        mailbox: "Archive/Receipts",
        from: null,
        unread_only: true,
        flagged_only: false,
        limit: 500,
      },
    });
    expect(result).toEqual({ messages: [], limitReached: false });
  });
});
