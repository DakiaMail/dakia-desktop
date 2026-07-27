import { beforeEach, describe, expect, it, vi } from "vitest";

const windowOpen = vi.fn();
const webviewWindowCtor = vi.fn();

vi.mock("@tauri-apps/api/event", () => ({
  emitTo: vi.fn(),
  listen: vi.fn(),
}));

vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: vi.fn(),
}));

vi.mock("@tauri-apps/api/webviewWindow", () => ({
  WebviewWindow: webviewWindowCtor,
}));

describe("composeWindow seed handoff", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    localStorage.clear();
    window.history.replaceState({}, "", "/");
    windowOpen.mockReset();
    webviewWindowCtor.mockReset();
    Object.defineProperty(window, "open", {
      configurable: true,
      writable: true,
      value: windowOpen,
    });
  });

  it("stores the compose seed behind a short token when opening a popup", async () => {
    const { openComposeWindow } = await import("./composeWindow");

    openComposeWindow({
      accountId: "account-1",
      to: "mara@example.com",
      contextMessageIds: Array.from(
        { length: 400 },
        (_, index) => `m-${index}`,
      ),
    });

    expect(windowOpen).toHaveBeenCalledOnce();
    const [url] = windowOpen.mock.calls[0];
    const params = new URL(String(url), "http://localhost").searchParams;
    expect(params.get("view")).toBe("compose");
    expect(params.get("seed")).toBeNull();
    const token = params.get("seedKey");
    expect(token).toBeTruthy();
    expect(localStorage.getItem(`dakia.compose-seed.${token}`)).toContain(
      '"accountId":"account-1"',
    );
  });

  it("reads and consumes a stored compose seed token", async () => {
    const token = "reply-seed";
    localStorage.setItem(
      `dakia.compose-seed.${token}`,
      JSON.stringify({
        subject: "Re: Weekly notes",
        contextMessageIds: ["message-1", "message-2"],
      }),
    );
    window.history.replaceState({}, "", `/?view=compose&seedKey=${token}`);

    const { readComposeSeed } = await import("./composeWindow");

    expect(readComposeSeed()).toEqual({
      subject: "Re: Weekly notes",
      contextMessageIds: ["message-1", "message-2"],
    });
    expect(localStorage.getItem(`dakia.compose-seed.${token}`)).toBeNull();
  });

  it("falls back to the inline seed for existing compose links", async () => {
    const seed = encodeURIComponent(
      JSON.stringify({ to: "legacy@example.com" }),
    );
    window.history.replaceState({}, "", `/?view=compose&seed=${seed}`);

    const { readComposeSeed } = await import("./composeWindow");

    expect(readComposeSeed()).toEqual({ to: "legacy@example.com" });
  });
});
