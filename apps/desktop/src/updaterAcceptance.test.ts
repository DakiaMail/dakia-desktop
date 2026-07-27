import { describe, expect, it, vi } from "vitest";
import {
  runUpdaterAcceptance,
  type UpdaterAcceptanceConfig,
} from "./updaterAcceptance";

function dependencies(currentVersion = "0.2.7") {
  return {
    currentVersion: vi.fn(async () => currentVersion),
    check: vi.fn(async () => ({ version: "0.2.8" })),
    download: vi.fn(async () => undefined),
    installAndRelaunch: vi.fn(async () => undefined),
    record: vi.fn(async () => undefined),
  };
}

const successful: UpdaterAcceptanceConfig = {
  expectedVersion: "0.2.8",
  expectRejection: false,
};

describe("updater acceptance harness", () => {
  it("downloads, installs, and requests a relaunch from the older version", async () => {
    const deps = dependencies();

    await runUpdaterAcceptance(successful, deps);

    expect(deps.check).toHaveBeenCalledOnce();
    expect(deps.download).toHaveBeenCalledOnce();
    expect(deps.installAndRelaunch).toHaveBeenCalledOnce();
    expect(deps.record).toHaveBeenCalledWith("installing", "0.2.8");
  });

  it("records completion after the updated app relaunches", async () => {
    const deps = dependencies("0.2.8");

    await runUpdaterAcceptance(successful, deps);

    expect(deps.record).toHaveBeenLastCalledWith("completed", "0.2.8");
    expect(deps.check).not.toHaveBeenCalled();
    expect(deps.installAndRelaunch).not.toHaveBeenCalled();
  });

  it("accepts only a cryptographic rejection in tamper mode", async () => {
    const deps = dependencies();
    deps.download.mockRejectedValue(
      new Error("Failed to verify updater signature"),
    );

    await runUpdaterAcceptance(
      { expectedVersion: "0.2.8", expectRejection: true },
      deps,
    );

    expect(deps.record).toHaveBeenLastCalledWith(
      "signature-rejected",
      "Error: Failed to verify updater signature",
    );
    expect(deps.installAndRelaunch).not.toHaveBeenCalled();
  });

  it("does not mistake a network failure for signature rejection", async () => {
    const deps = dependencies();
    deps.download.mockRejectedValue(new Error("connection reset"));

    await expect(
      runUpdaterAcceptance(
        { expectedVersion: "0.2.8", expectRejection: true },
        deps,
      ),
    ).rejects.toThrow("connection reset");
    expect(deps.record).not.toHaveBeenCalledWith(
      "signature-rejected",
      expect.anything(),
    );
  });
});
