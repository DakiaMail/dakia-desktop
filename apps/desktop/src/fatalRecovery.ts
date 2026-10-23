import { invoke } from "@tauri-apps/api/core";

export type FatalRecoveryReason =
  "newer_profile" | "upgrade_failed" | "storage_unavailable";

export const fatalRecoveryCopy: Record<
  FatalRecoveryReason,
  { title: string; message: string }
> = {
  newer_profile: {
    title: "This mail data needs a newer Dakia version",
    message:
      "Dakia stopped before migrating this mail data because a newer version created it. Open it with that newer Dakia version. To return to an older version, quit Dakia and restore the complete backup made before the upgrade.",
  },
  upgrade_failed: {
    title: "Dakia couldn't safely upgrade your mail data",
    message:
      "Dakia stopped instead of opening an empty mailbox. Keep the complete Dakia data folder for recovery. Restore the complete backup made before the upgrade, or reopen it with a compatible newer Dakia version.",
  },
  storage_unavailable: {
    title: "Dakia couldn't safely open your mail data",
    message:
      "Dakia stopped instead of opening an empty mailbox. Quit every Dakia window and try once more. If it still fails, keep the complete Dakia data folder for recovery and restore a complete backup before changing it.",
  },
};

export function fatalRecoveryReasonFromSearch(
  search: string,
): FatalRecoveryReason {
  const reason = new URLSearchParams(search).get("reason");
  if (reason === "newer_profile" || reason === "upgrade_failed") {
    return reason;
  }
  return "storage_unavailable";
}

export function initializeFatalRecovery(
  document_: Document,
  search: string,
  requestQuit: () => Promise<unknown> = () => invoke("quit_startup_recovery"),
) {
  const root = document_.querySelector<HTMLElement>("[data-fatal-recovery]");
  const title = document_.querySelector<HTMLElement>("#fatal-recovery-title");
  const message = document_.querySelector<HTMLElement>(
    "#fatal-recovery-message",
  );
  const quit = document_.querySelector<HTMLButtonElement>(
    "#fatal-recovery-quit",
  );
  if (!root || !title || !message || !quit) return;

  const copy = fatalRecoveryCopy[fatalRecoveryReasonFromSearch(search)];
  document_.title = "Dakia";
  title.textContent = copy.title;
  message.textContent = copy.message;
  quit.addEventListener("click", async () => {
    if (quit.disabled) return;
    quit.disabled = true;
    try {
      await requestQuit();
    } catch {
      // The Rust command terminates the app. If it could not, leave the only
      // safe action available so the user can try the same explicit exit again.
      quit.disabled = false;
    }
  });
}

if (document.querySelector("[data-fatal-recovery]")) {
  initializeFatalRecovery(document, window.location.search);
}
