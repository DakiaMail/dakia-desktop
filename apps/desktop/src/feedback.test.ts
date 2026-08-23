import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  getVersion: vi.fn(),
  arch: vi.fn(),
  osType: vi.fn(),
  osVersion: vi.fn(),
}));

vi.mock("@tauri-apps/api/app", () => ({ getVersion: mocks.getVersion }));
vi.mock("@tauri-apps/plugin-os", () => ({
  arch: mocks.arch,
  type: mocks.osType,
  version: mocks.osVersion,
}));

import {
  createFeedbackBody,
  createFeedbackComposeSeed,
  loadFeedbackEnvironment,
} from "./feedback";

describe("feedback composer seed", () => {
  beforeEach(() => {
    mocks.getVersion.mockResolvedValue("0.3.2");
    mocks.osType.mockResolvedValue("macOS");
    mocks.osVersion.mockResolvedValue("15.6");
    mocks.arch.mockResolvedValue("aarch64");
  });

  it("creates the approved editable feedback template", async () => {
    await expect(
      createFeedbackComposeSeed("account-1", "en-US"),
    ).resolves.toEqual({
      accountId: "account-1",
      to: "support@dakiamail.com",
      subject: "Dakia feedback",
      body: "Hi Dakia team,\n\n[Write your feedback here]\n\n---\nAutomatically included:\nDakia version: 0.3.2\nOperating system: macOS 15.6\nArchitecture: aarch64\nLanguage: en-US",
    });
  });

  it("uses Unavailable for each failed diagnostic without rejecting", async () => {
    mocks.getVersion.mockRejectedValue(new Error("no app API"));
    mocks.osVersion.mockRejectedValue(new Error("no os version"));

    await expect(loadFeedbackEnvironment("en")).resolves.toEqual({
      appVersion: undefined,
      osName: "macOS",
      osVersion: undefined,
      architecture: "aarch64",
      locale: "en",
    });
    await expect(
      createFeedbackComposeSeed(undefined, "en"),
    ).resolves.toMatchObject({
      body: "Hi Dakia team,\n\n[Write your feedback here]\n\n---\nAutomatically included:\nDakia version: Unavailable\nOperating system: macOS Unavailable\nArchitecture: aarch64\nLanguage: en",
    });
  });

  it("keeps private mailbox and account information out of the template", () => {
    const body = createFeedbackBody({
      appVersion: "0.3.2",
      osName: "macOS",
      osVersion: "15.6",
      architecture: "aarch64",
      locale: "en",
    });

    for (const privateValue of [
      "person@example.com",
      "account-1",
      "imap.example.com",
      "smtp.example.com",
      "password",
      "message-1",
      "mailbox",
      "log output",
    ]) {
      expect(body).not.toContain(privateValue);
    }
  });
});
