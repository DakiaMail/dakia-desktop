import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import fixture from "../testdata/tauri-contracts/high-risk.json";

const eventMocks = vi.hoisted(() => ({
  emitTo: vi.fn(),
  listen: vi.fn(),
}));
const apiMocks = vi.hoisted(() => ({
  channels: [] as Array<{ onmessage?: (message: unknown) => void }>,
  invoke: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: apiMocks.invoke,
  Channel: class {
    onmessage?: (message: unknown) => void;

    constructor() {
      apiMocks.channels.push(this);
    }
  },
}));

vi.mock("@tauri-apps/api/event", () => eventMocks);
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: vi.fn() }));
vi.mock("@tauri-apps/api/webviewWindow", () => ({
  getAllWebviewWindows: vi.fn(),
  WebviewWindow: vi.fn(),
}));

type ListenHandler = (event: { payload: unknown }) => void;

function decoded<T>(value: unknown): T {
  return JSON.parse(JSON.stringify(value)) as T;
}

describe("Tauri payload contracts", () => {
  beforeEach(() => {
    vi.resetModules();
    vi.clearAllMocks();
    apiMocks.channels.length = 0;
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
  });

  afterEach(() => {
    Reflect.deleteProperty(window, "__TAURI_INTERNALS__");
  });

  it("decodes native MessageContent success and sanitizes its error envelope", async () => {
    apiMocks.invoke.mockImplementation((command: string) => {
      if (command === fixture.commands.messageContent.command) {
        return Promise.resolve(decoded(fixture.messageContent.success));
      }
      return Promise.resolve(undefined);
    });
    const { api } = await import("./api");

    await expect(
      api.content(fixture.commands.messageContent.arguments.messageId),
    ).resolves.toEqual(fixture.messageContent.success);
    expect(apiMocks.invoke).toHaveBeenCalledWith(
      fixture.commands.messageContent.command,
      fixture.commands.messageContent.arguments,
    );

    apiMocks.invoke.mockRejectedValueOnce(
      decoded(fixture.messageContent.error),
    );
    await expect(
      api.content(fixture.commands.messageContent.arguments.messageId),
    ).rejects.toMatchObject({
      ...fixture.messageContent.error,
      retryable: false,
      message: "Message content could not be loaded",
    });

    for (const variant of fixture.messageContent.errorVariants) {
      apiMocks.invoke.mockRejectedValueOnce(decoded(variant));
      await expect(
        api.content(fixture.commands.messageContent.arguments.messageId),
      ).rejects.toMatchObject({
        kind: variant.kind,
        retryable: variant.kind === "transient",
        message: "Message content could not be loaded",
      });
    }
  });

  it("decodes provider-signature-inline through the native MessageContent boundary", async () => {
    expect(fixture.realisticFixtureIds.providerSignature).toBe(
      "provider-signature-inline",
    );
    apiMocks.invoke.mockResolvedValueOnce(
      decoded(fixture.messageContent.providerSignature),
    );
    const { api } = await import("./api");

    await expect(
      api.content(fixture.commands.messageContent.arguments.messageId),
    ).resolves.toEqual(fixture.messageContent.providerSignature);
  });

  it("uses the fixture's exact command names, top-level argument keys, and sync Channel", async () => {
    const progress = vi.fn();
    apiMocks.invoke.mockImplementation(
      (command: string, arguments_: Record<string, unknown>) => {
        if (command === fixture.commands.syncAccount.command) {
          (
            arguments_.onProgress as { onmessage?: (value: unknown) => void }
          ).onmessage?.({ phase: "complete", completed: 1, total: 1 });
        }
        return Promise.resolve(undefined);
      },
    );
    const { api } = await import("./api");
    const commands = fixture.commands;

    await api.hydrateMessage(commands.hydrateMessage.arguments.messageId);
    await api.setCategory(
      commands.setMessageCategory.arguments.messageId,
      commands.setMessageCategory.arguments.category,
    );
    await api.setStarred(
      commands.setMessageStarred.arguments.messageId,
      commands.setMessageStarred.arguments.starred,
    );
    await api.setRead(
      commands.setMessageRead.arguments.messageId,
      commands.setMessageRead.arguments.read,
    );
    await api.action(
      commands.applyMailboxAction.arguments.accountId,
      commands.applyMailboxAction.arguments.mailbox,
      commands.applyMailboxAction.arguments.uid,
      commands.applyMailboxAction.arguments.action as "archive",
    );
    await api.sync(commands.syncAccount.arguments.accountId, progress);

    expect(apiMocks.invoke.mock.calls).toEqual([
      [commands.hydrateMessage.command, commands.hydrateMessage.arguments],
      [
        commands.setMessageCategory.command,
        commands.setMessageCategory.arguments,
      ],
      [
        commands.setMessageStarred.command,
        commands.setMessageStarred.arguments,
      ],
      [commands.setMessageRead.command, commands.setMessageRead.arguments],
      [
        commands.applyMailboxAction.command,
        commands.applyMailboxAction.arguments,
      ],
      [
        commands.syncAccount.command,
        {
          ...commands.syncAccount.arguments,
          onProgress: apiMocks.channels[0],
        },
      ],
    ]);
    expect(commands.syncAccount.arguments.onProgress).toBe("__TAURI_CHANNEL__");
    expect(progress).toHaveBeenCalledWith({
      phase: "complete",
      completed: 1,
      total: 1,
    });
  });

  it("delivers camelCase event envelopes while preserving native null fields", async () => {
    const handlers = new Map<string, ListenHandler>();
    eventMocks.listen.mockImplementation(
      async (event: string, handler: ListenHandler) => {
        handlers.set(event, handler);
        return vi.fn();
      },
    );
    const { onMailArrived, onMailChanged, onMailHydrated, onMailSyncState } =
      await import("./nativeWindows");
    const arrived = vi.fn();
    const changed = vi.fn();
    const hydrated = vi.fn();
    const syncStates = vi.fn();

    await Promise.all([
      onMailArrived(arrived),
      onMailChanged(changed),
      onMailHydrated(hydrated),
      onMailSyncState(syncStates),
    ]);

    handlers.get("mail-arrived")?.({
      payload: decoded(fixture.events.mailArrived),
    });
    handlers.get("mail-changed")?.({
      payload: decoded(fixture.events.mailChanged),
    });
    handlers.get("mail-hydrated")?.({
      payload: decoded(fixture.events.mailHydrated),
    });
    handlers.get("mail-sync-state")?.({
      payload: decoded(fixture.events.mailSyncStateWithNulls),
    });
    handlers.get("mail-sync-state")?.({
      payload: decoded(fixture.events.mailSyncStateRetrying),
    });

    expect(arrived).toHaveBeenCalledWith(fixture.events.mailArrived);
    expect(changed).toHaveBeenCalledWith(fixture.events.mailChanged.accountId);
    expect(hydrated).toHaveBeenCalledWith(fixture.events.mailHydrated);
    expect(syncStates).toHaveBeenNthCalledWith(
      1,
      fixture.events.mailSyncStateWithNulls,
    );
    expect(syncStates).toHaveBeenNthCalledWith(
      2,
      fixture.events.mailSyncStateRetrying,
    );
    expect(fixture.events.mailSyncStateWithNulls).toHaveProperty(
      "retryAt",
      null,
    );
    expect(fixture.events.mailSyncStateWithNulls).toHaveProperty(
      "errorKind",
      null,
    );
  });

  it("never broadcasts an AI API key across native windows", async () => {
    const { notifySettingsChanged } = await import("./nativeWindows");
    await notifySettingsChanged({
      provider: "openai",
      baseUrl: "https://api.example.test/",
      model: "example-model",
      apiKey: "production-shaped-secret",
      executable: "",
      modelPath: "",
    });

    expect(eventMocks.emitTo).toHaveBeenCalledWith(
      "main",
      "settings-changed",
      expect.objectContaining({ apiKey: "" }),
    );
    expect(JSON.stringify(eventMocks.emitTo.mock.calls)).not.toContain(
      "production-shaped-secret",
    );
  });
});
