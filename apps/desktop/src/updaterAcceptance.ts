import { getVersion } from "@tauri-apps/api/app";
import { invoke, isTauri } from "@tauri-apps/api/core";
import {
  checkForUpdate,
  downloadUpdate,
  installUpdateAndRelaunch,
} from "./updater";

export type UpdaterAcceptanceConfig = {
  expectedVersion: string;
  expectRejection: boolean;
};

type AcceptanceDependencies = {
  currentVersion: () => Promise<string>;
  check: () => Promise<{ version: string } | null>;
  download: () => Promise<void>;
  installAndRelaunch: () => Promise<void>;
  record: (event: string, detail?: string) => Promise<void>;
};

const nativeDependencies: AcceptanceDependencies = {
  currentVersion: getVersion,
  check: () => checkForUpdate(true),
  download: () => downloadUpdate(() => undefined),
  installAndRelaunch: installUpdateAndRelaunch,
  record: (event, detail) =>
    invoke("record_updater_acceptance_event", { event, detail }),
};

export async function runUpdaterAcceptance(
  config: UpdaterAcceptanceConfig,
  dependencies: AcceptanceDependencies,
) {
  const currentVersion = await dependencies.currentVersion();
  await dependencies.record("launched", currentVersion);

  if (!config.expectRejection && currentVersion === config.expectedVersion) {
    await dependencies.record("completed", currentVersion);
    return;
  }
  if (config.expectRejection && currentVersion === config.expectedVersion) {
    throw new Error(
      `Expected rejection, but Dakia already runs ${currentVersion}.`,
    );
  }

  const update = await dependencies.check();
  if (!update) {
    throw new Error(
      `No update was offered to ${currentVersion}; expected ${config.expectedVersion}.`,
    );
  }
  if (update.version !== config.expectedVersion) {
    throw new Error(
      `Updater offered ${update.version}; expected ${config.expectedVersion}.`,
    );
  }
  await dependencies.record("update-available", update.version);

  try {
    await dependencies.download();
  } catch (error) {
    const message = String(error);
    if (
      config.expectRejection &&
      /signature|verification|verify|cryptographic/i.test(message)
    ) {
      await dependencies.record("signature-rejected", message);
      return;
    }
    throw error;
  }

  if (config.expectRejection) {
    throw new Error("Updater accepted an artifact that should be rejected.");
  }
  await dependencies.record("downloaded", update.version);
  await dependencies.record("installing", update.version);
  await dependencies.installAndRelaunch();
}

export async function runUpdaterAcceptanceIfConfigured() {
  if (!isTauri()) return false;
  const config = await invoke<UpdaterAcceptanceConfig | null>(
    "updater_acceptance_config",
  );
  if (!config) return false;

  try {
    await runUpdaterAcceptance(config, nativeDependencies);
  } catch (error) {
    await nativeDependencies.record("failed", String(error));
  }
  return true;
}
