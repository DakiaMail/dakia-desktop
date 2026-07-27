import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "../i18n";
import { api } from "../api";
import type { AiSettings, NotificationSettings } from "../types";
import { Settings } from "./Settings";

const translationSettingsMocks = vi.hoisted(() => ({
  confirm: vi.fn(),
  resetTranslator: vi.fn(),
}));

vi.mock("../nativeFeedback", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../nativeFeedback")>()),
  confirmNativeAction: translationSettingsMocks.confirm,
}));

vi.mock("../offlineTranslation", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../offlineTranslation")>()),
  resetOfflineTranslator: translationSettingsMocks.resetTranslator,
}));

const ai: AiSettings = {
  provider: "ollama",
  baseUrl: "http://127.0.0.1:11434/",
  model: "qwen2.5:1.5b",
  apiKey: "",
  executable: "",
  modelPath: "",
};

const notifications: NotificationSettings = {
  enabled: true,
  soundEnabled: true,
  showPreview: false,
};

const props = {
  ai,
  accounts: [],
  accountsLoading: false,
  accountSaving: false,
  accountRemoving: false,
  accountFullSyncing: false,
  notifications,
  notificationPermission: true,
  launchAtLogin: false,
  realtimeStatuses: [],
  onAiChange: vi.fn(),
  onAddAccount: vi.fn(),
  onSaveAccount: vi.fn(),
  onRemoveAccount: vi.fn(),
  onFullSyncAccount: vi.fn(),
  onNotificationsChange: vi.fn(),
  onTestNotification: vi.fn(),
  onLaunchAtLoginChange: vi.fn(),
};

describe("Settings offline translation models", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    translationSettingsMocks.confirm.mockResolvedValue(true);
    translationSettingsMocks.resetTranslator.mockResolvedValue(undefined);
    vi.spyOn(api, "translationModels").mockResolvedValue([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_944_124,
        installed: true,
      },
      {
        source: "de",
        sourceName: "German",
        target: "en",
        downloadBytes: 22_972_674,
        installed: false,
      },
    ]);
    vi.spyOn(api, "removeTranslationModel").mockResolvedValue();
  });

  it("does not expose AI or plugin settings in the current product surface", () => {
    render(
      <MantineProvider>
        <Settings {...props} />
      </MantineProvider>,
    );

    expect(screen.queryByRole("tab", { name: "AI model" })).toBeNull();
    expect(screen.queryByRole("tab", { name: "Plugins" })).toBeNull();
    expect(screen.queryByText("Before you use AI")).toBeNull();
  });

  it("loads and labels installed and available dedicated language packs", async () => {
    render(
      <MantineProvider>
        <Settings {...props} />
      </MantineProvider>,
    );

    fireEvent.click(screen.getByRole("tab", { name: "Translation" }));

    expect(await screen.findByText("Estonian → English")).toBeVisible();
    expect(screen.getByText("German → English")).toBeVisible();
    expect(screen.getByText(/Installed · 20.9 MB/)).toBeVisible();
    expect(screen.getByText(/Available · 21.9 MB/)).toBeVisible();
    expect(api.translationModels).toHaveBeenCalledOnce();
    expect(
      screen.getByRole("button", { name: "Remove Estonian pack" }),
    ).toBeVisible();
    expect(
      screen.queryByRole("button", { name: "Remove German pack" }),
    ).not.toBeInTheDocument();
  });

  it("unloads the worker before removing a pack and updates local status", async () => {
    render(
      <MantineProvider>
        <Settings {...props} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("tab", { name: "Translation" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "Remove Estonian pack" }),
    );

    await waitFor(() =>
      expect(translationSettingsMocks.confirm).toHaveBeenCalledWith(
        "Remove Estonian pack",
        expect.stringContaining("download it again"),
        "Remove Estonian pack",
      ),
    );
    await waitFor(() =>
      expect(translationSettingsMocks.resetTranslator).toHaveBeenCalledOnce(),
    );
    expect(api.removeTranslationModel).toHaveBeenCalledWith("et");
    expect(
      screen.queryByRole("button", { name: "Remove Estonian pack" }),
    ).not.toBeInTheDocument();
    expect(screen.getAllByText(/Available ·/)).toHaveLength(2);
  });

  it("preserves the installed pack when removal is declined", async () => {
    translationSettingsMocks.confirm.mockResolvedValue(false);
    render(
      <MantineProvider>
        <Settings {...props} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("tab", { name: "Translation" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "Remove Estonian pack" }),
    );

    await waitFor(() =>
      expect(translationSettingsMocks.confirm).toHaveBeenCalledOnce(),
    );
    expect(translationSettingsMocks.resetTranslator).not.toHaveBeenCalled();
    expect(api.removeTranslationModel).not.toHaveBeenCalled();
    expect(
      screen.getByRole("button", { name: "Remove Estonian pack" }),
    ).toBeVisible();
  });

  it("shows model-list and removal failures without hiding the pack", async () => {
    vi.mocked(api.translationModels).mockRejectedValueOnce(
      "Could not load packs",
    );
    const { unmount } = render(
      <MantineProvider>
        <Settings {...props} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("tab", { name: "Translation" }));
    expect(await screen.findByText("Could not load packs")).toBeVisible();
    unmount();

    vi.mocked(api.translationModels).mockResolvedValueOnce([
      {
        source: "et",
        sourceName: "Estonian",
        target: "en",
        downloadBytes: 21_944_124,
        installed: true,
      },
    ]);
    vi.mocked(api.removeTranslationModel).mockRejectedValueOnce("Pack is busy");
    render(
      <MantineProvider>
        <Settings {...props} />
      </MantineProvider>,
    );
    fireEvent.click(screen.getByRole("tab", { name: "Translation" }));
    fireEvent.click(
      await screen.findByRole("button", { name: "Remove Estonian pack" }),
    );

    expect(await screen.findByText("Pack is busy")).toBeVisible();
    expect(
      screen.getByRole("button", { name: "Remove Estonian pack" }),
    ).toBeVisible();
  });
});
