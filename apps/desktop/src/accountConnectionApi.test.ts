import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const apiMocks = vi.hoisted(() => ({
  invoke: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: apiMocks.invoke,
  Channel: class {},
}));

describe("account connection API bridge", () => {
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

  it("preserves the account connection response from password setup", async () => {
    const response = {
      account: { id: "account-1", email: "me@example.com" },
      reusedExistingAccount: true,
    };
    apiMocks.invoke.mockResolvedValue(response);
    const { api } = await import("./api");
    const draft = { email: "me@example.com" };

    await expect(api.addAccount(draft, "app-password")).resolves.toEqual(
      response,
    );
    expect(apiMocks.invoke.mock.calls).toEqual([
      ["add_account", { input: { draft, password: "app-password" } }],
    ]);
  });
});
