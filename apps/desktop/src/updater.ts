import { relaunch } from "@tauri-apps/plugin-process";
import {
  check,
  type DownloadEvent,
  type Update,
} from "@tauri-apps/plugin-updater";

const LAST_CHECK_KEY = "dakia.updates.last-successful-check";

export const UPDATE_CHECK_INTERVAL_MS = 24 * 60 * 60 * 1000;

export type AvailableUpdate = {
  version: string;
  notes?: string;
  pubDate?: string;
};

export type DownloadProgress = {
  downloadedBytes: number;
  totalBytes?: number;
};

type NativeCheck = typeof check;
type NativeRelaunch = typeof relaunch;

let checkPromise: Promise<AvailableUpdate | null> | null = null;
let pendingUpdate: Update | null = null;
let downloaded = false;

export function isAutomaticUpdateCheckDue(
  lastSuccessfulCheck: string | null,
  now = Date.now(),
) {
  if (!lastSuccessfulCheck) return true;
  const checkedAt = Date.parse(lastSuccessfulCheck);
  return (
    !Number.isFinite(checkedAt) || now - checkedAt >= UPDATE_CHECK_INTERVAL_MS
  );
}

export function automaticUpdateCheckDue(now = Date.now()) {
  return isAutomaticUpdateCheckDue(localStorage.getItem(LAST_CHECK_KEY), now);
}

export async function checkForUpdate(
  force = false,
  nativeCheck: NativeCheck = check,
): Promise<AvailableUpdate | null> {
  if (!force && !automaticUpdateCheckDue()) return null;
  if (checkPromise) return checkPromise;

  checkPromise = (async () => {
    const update = await nativeCheck({ timeout: 15_000 });
    localStorage.setItem(LAST_CHECK_KEY, new Date().toISOString());

    if (!update) {
      await releasePendingUpdate();
      return null;
    }

    if (pendingUpdate && pendingUpdate !== update) {
      await pendingUpdate.close();
    }
    pendingUpdate = update;
    downloaded = false;
    return {
      version: update.version,
      notes: update.body,
      pubDate: update.date,
    };
  })();

  try {
    return await checkPromise;
  } finally {
    checkPromise = null;
  }
}

export async function downloadUpdate(
  onProgress: (progress: DownloadProgress) => void,
) {
  if (!pendingUpdate) {
    throw new Error("No verified update is available to download.");
  }
  if (downloaded) return;

  let downloadedBytes = 0;
  let totalBytes: number | undefined;
  const report = (event: DownloadEvent) => {
    switch (event.event) {
      case "Started":
        downloadedBytes = 0;
        totalBytes = event.data.contentLength;
        onProgress({ downloadedBytes, totalBytes });
        break;
      case "Progress":
        downloadedBytes += event.data.chunkLength;
        onProgress({ downloadedBytes, totalBytes });
        break;
      case "Finished":
        if (totalBytes !== undefined) downloadedBytes = totalBytes;
        onProgress({ downloadedBytes, totalBytes });
        break;
    }
  };

  await pendingUpdate.download(report, { timeout: 10 * 60 * 1000 });
  downloaded = true;
}

export async function installUpdateAndRelaunch(
  nativeRelaunch: NativeRelaunch = relaunch,
) {
  if (!pendingUpdate || !downloaded) {
    throw new Error("Download the verified update before installing it.");
  }
  await pendingUpdate.install();
  await nativeRelaunch();
}

async function releasePendingUpdate() {
  const update = pendingUpdate;
  pendingUpdate = null;
  downloaded = false;
  if (update) await update.close();
}

export async function resetUpdaterForTests() {
  checkPromise = null;
  await releasePendingUpdate();
}
