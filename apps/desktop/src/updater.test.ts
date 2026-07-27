import type { Update } from "@tauri-apps/plugin-updater";
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  checkForUpdate,
  downloadUpdate,
  installUpdateAndRelaunch,
  isAutomaticUpdateCheckDue,
  resetUpdaterForTests,
  UPDATE_CHECK_INTERVAL_MS,
} from "./updater";

function nativeUpdate(version = "0.3.0") {
  return {
    version,
    body: "Release notes",
    date: "2026-07-25T12:00:00Z",
    download: vi.fn(async (onEvent) => {
      onEvent?.({ event: "Started", data: { contentLength: 100 } });
      onEvent?.({ event: "Progress", data: { chunkLength: 40 } });
      onEvent?.({ event: "Progress", data: { chunkLength: 60 } });
      onEvent?.({ event: "Finished" });
    }),
    install: vi.fn(async () => undefined),
    close: vi.fn(async () => undefined),
  } as unknown as Update;
}

beforeEach(async () => {
  localStorage.clear();
  await resetUpdaterForTests();
});

describe("automatic update cadence", () => {
  const now = Date.parse("2026-07-24T12:00:00.000Z");

  it("checks on first launch", () => {
    expect(isAutomaticUpdateCheckDue(null, now)).toBe(true);
  });

  it("does not check twice within 24 hours", () => {
    expect(
      isAutomaticUpdateCheckDue(
        new Date(now - UPDATE_CHECK_INTERVAL_MS + 1).toISOString(),
        now,
      ),
    ).toBe(false);
  });

  it("checks again after 24 hours", () => {
    expect(
      isAutomaticUpdateCheckDue(
        new Date(now - UPDATE_CHECK_INTERVAL_MS).toISOString(),
        now,
      ),
    ).toBe(true);
  });
});

describe("native signed updater", () => {
  it("returns metadata from Tauri's verified update check", async () => {
    const update = nativeUpdate();
    const nativeCheck = vi.fn(async () => update);

    await expect(checkForUpdate(true, nativeCheck)).resolves.toEqual({
      version: "0.3.0",
      notes: "Release notes",
      pubDate: "2026-07-25T12:00:00Z",
    });
    expect(nativeCheck).toHaveBeenCalledWith({ timeout: 15_000 });
  });

  it("records a successful no-update check for the daily cadence", async () => {
    const nativeCheck = vi.fn(async () => null);
    await expect(checkForUpdate(true, nativeCheck)).resolves.toBeNull();
    expect(localStorage.getItem("dakia.updates.last-successful-check")).toMatch(
      /^\d{4}-\d{2}-\d{2}T/,
    );
  });

  it("does not suppress retries after a failed check", async () => {
    const nativeCheck = vi
      .fn()
      .mockRejectedValueOnce(new Error("offline"))
      .mockResolvedValueOnce(null);

    await expect(checkForUpdate(true, nativeCheck)).rejects.toThrow("offline");
    expect(
      localStorage.getItem("dakia.updates.last-successful-check"),
    ).toBeNull();
    await expect(checkForUpdate(false, nativeCheck)).resolves.toBeNull();
    expect(nativeCheck).toHaveBeenCalledTimes(2);
  });

  it("downloads with progress, then installs and relaunches explicitly", async () => {
    const update = nativeUpdate();
    await checkForUpdate(
      true,
      vi.fn(async () => update),
    );
    const progress = vi.fn();
    const nativeRelaunch = vi.fn(async () => undefined);

    await downloadUpdate(progress);
    await installUpdateAndRelaunch(nativeRelaunch);

    expect(progress).toHaveBeenLastCalledWith({
      downloadedBytes: 100,
      totalBytes: 100,
    });
    expect(update.download).toHaveBeenCalledOnce();
    expect(update.install).toHaveBeenCalledOnce();
    expect(nativeRelaunch).toHaveBeenCalledOnce();
  });
});
