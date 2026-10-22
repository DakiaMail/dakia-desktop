import { beforeEach, describe, expect, it, vi } from "vitest";

const apiMocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: apiMocks.invoke }));

const markup = `
  <main data-fatal-recovery role="alert" aria-labelledby="fatal-recovery-title">
    <h1 id="fatal-recovery-title"></h1>
    <p id="fatal-recovery-message"></p>
    <button id="fatal-recovery-quit" type="button">Quit Dakia</button>
  </main>`;

describe("fatal recovery page", () => {
  let initializeFatalRecovery: typeof import("./fatalRecovery").initializeFatalRecovery;

  beforeEach(async () => {
    vi.resetModules();
    vi.clearAllMocks();
    document.body.innerHTML = "";
    ({ initializeFatalRecovery } = await import("./fatalRecovery"));
    document.body.innerHTML = markup;
  });

  it.each([
    [
      "newer_profile",
      "This mail data needs a newer Dakia version",
      "Dakia stopped before migrating this mail data because a newer version created it. Open it with that newer Dakia version. To return to an older version, quit Dakia and restore the complete backup made before the upgrade.",
    ],
    [
      "upgrade_failed",
      "Dakia couldn't safely upgrade your mail data",
      "Dakia stopped instead of opening an empty mailbox. Keep the complete Dakia data folder for recovery. Restore the complete backup made before the upgrade, or reopen it with a compatible newer Dakia version.",
    ],
    [
      "storage_unavailable",
      "Dakia couldn't safely open your mail data",
      "Dakia stopped instead of opening an empty mailbox. Quit every Dakia window and try once more. If it still fails, keep the complete Dakia data folder for recovery and restore a complete backup before changing it.",
    ],
  ])("renders the bounded %s recovery copy", async (reason, title, message) => {
    initializeFatalRecovery(document, `?reason=${reason}`, () =>
      Promise.resolve(),
    );

    expect(document.title).toBe("Dakia");
    expect(document.querySelector("h1")).toHaveTextContent(title);
    expect(document.querySelector("p")).toHaveTextContent(message);
    expect(document.body).not.toHaveTextContent("Inbox");
  });

  it("fails closed to generic recovery copy without reflecting a malformed query", async () => {
    initializeFatalRecovery(
      document,
      "?reason=<script>private-subject-and-token</script>",
      () => Promise.resolve(),
    );

    expect(document.querySelector("h1")).toHaveTextContent(
      "Dakia couldn't safely open your mail data",
    );
    expect(document.body.textContent).not.toContain(
      "private-subject-and-token",
    );
  });

  it("offers only the exact fatal-window exit command and prevents duplicate clicks", async () => {
    const requestQuit = vi.fn(() => new Promise<void>(() => undefined));
    initializeFatalRecovery(document, "?reason=newer_profile", requestQuit);
    const button = document.querySelector<HTMLButtonElement>("button")!;

    button.click();
    button.click();

    expect(button).toBeDisabled();
    expect(requestQuit).toHaveBeenCalledTimes(1);
    expect(apiMocks.invoke).not.toHaveBeenCalled();
  });

  it("uses quit_startup_recovery when no test request is supplied", async () => {
    apiMocks.invoke.mockResolvedValueOnce(undefined);
    initializeFatalRecovery(document, "?reason=upgrade_failed");

    document.querySelector<HTMLButtonElement>("button")!.click();
    await Promise.resolve();

    expect(apiMocks.invoke).toHaveBeenCalledTimes(1);
    expect(apiMocks.invoke).toHaveBeenCalledWith("quit_startup_recovery");
  });
});
