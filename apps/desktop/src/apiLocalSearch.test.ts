// @vitest-environment jsdom

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import localSearchContract from "../testdata/tauri-contracts/local-search.json";

const invoke = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({
  invoke,
  Channel: class {},
}));

describe("local search API boundary", () => {
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

  it("preserves Unicode, filters, account scope, and cursor through IPC", async () => {
    const query = localSearchContract.arguments.query;
    invoke.mockResolvedValue(localSearchContract.response);
    const { api } = await import("./api");

    await expect(
      api.search(
        query.text,
        query.account_ids,
        query.mailbox,
        query.unread_only,
        query.flagged_only,
        query.limit,
        query.cursor,
        query.category,
        query.unflagged_only,
        query.read_only,
      ),
    ).resolves.toEqual(localSearchContract.response);

    expect(invoke).toHaveBeenCalledOnce();
    expect(invoke).toHaveBeenCalledWith(
      localSearchContract.command,
      localSearchContract.arguments,
    );
  });
});
