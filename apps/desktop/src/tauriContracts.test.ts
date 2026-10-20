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

  it("does not expose a command that can start a new OAuth account flow", async () => {
    const { api } = await import("./api");

    expect(api).not.toHaveProperty("addOAuthAccount");
    expect(apiMocks.invoke).not.toHaveBeenCalledWith(
      "add_oauth_account",
      expect.anything(),
    );
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
    const immutableAction = {
      messageId: "message-1",
      action: "archive" as const,
    };
    await api.action(immutableAction.messageId, immutableAction.action);
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
      [commands.applyMailboxAction.command, immutableAction],
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

  it("uses the mailbox action command with its immutable local message ID", async () => {
    apiMocks.invoke.mockResolvedValue(undefined);
    const { api } = await import("./api");

    await api.action("message-42", "delete");

    expect(apiMocks.invoke).toHaveBeenCalledWith("apply_mailbox_action", {
      messageId: "message-42",
      action: "delete",
    });
  });

  it("uses the sender cleanup command with the immutable account and address", async () => {
    apiMocks.invoke.mockResolvedValue({ matched: 3, moved: 2, failed: 1 });
    const { api } = await import("./api");

    await expect(
      api.trashMessagesFromSender("account-1", "sender@example.test"),
    ).resolves.toEqual({ matched: 3, moved: 2, failed: 1 });
    expect(apiMocks.invoke).toHaveBeenCalledWith("trash_messages_from_sender", {
      accountId: "account-1",
      senderAddress: "sender@example.test",
    });
  });

  it("queries durable synchronization coverage through its dedicated command", async () => {
    apiMocks.invoke.mockResolvedValue([]);
    const { api } = await import("./api");

    await expect(api.mailSyncStatus()).resolves.toEqual([]);
    expect(apiMocks.invoke).toHaveBeenCalledWith("mail_sync_status");
  });

  it("reads restart-recovery operations and a saved SMTP draft through dedicated commands", async () => {
    apiMocks.invoke.mockResolvedValue([]);
    const { api } = await import("./api");

    await api.mailUnresolvedOperations(["account-1"]);
    await api.outgoingOperationDraft("operation-1");

    expect(apiMocks.invoke).toHaveBeenNthCalledWith(
      1,
      "mail_unresolved_operations",
      { accountIds: ["account-1"] },
    );
    expect(apiMocks.invoke).toHaveBeenNthCalledWith(
      2,
      "outgoing_operation_draft",
      { operationId: "operation-1" },
    );
  });

  it("uses the durable outcome command for SMTP acceptance state", async () => {
    apiMocks.invoke.mockResolvedValue({
      operationId: "operation-1",
      status: "sent_copy_pending",
      response: "250 queued",
    });
    const { api } = await import("./api");
    const draft = { account_id: "account-1", to: ["recipient@example.test"] };

    await expect(api.sendOutcome(draft)).resolves.toMatchObject({
      operationId: "operation-1",
      status: "sent_copy_pending",
    });
    expect(apiMocks.invoke).toHaveBeenCalledWith("send_message_outcome", {
      draft,
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

  it("delivers committed catalogue revisions without changing the payload", async () => {
    const handlers = new Map<string, ListenHandler>();
    eventMocks.listen.mockImplementation(
      async (event: string, handler: ListenHandler) => {
        handlers.set(event, handler);
        return vi.fn();
      },
    );
    const { onMailCatalogueUpdated } = await import("./nativeWindows");
    const updated = vi.fn();
    const event = { accountId: "account-1", mailbox: "INBOX", revision: 42 };

    await onMailCatalogueUpdated(updated);
    handlers.get("mail-catalogue-updated")?.({ payload: decoded(event) });

    expect(updated).toHaveBeenCalledWith(event);
  });

  it("delivers content-activity wakeups by account without a mail query payload", async () => {
    const handlers = new Map<string, ListenHandler>();
    eventMocks.listen.mockImplementation(
      async (event: string, handler: ListenHandler) => {
        handlers.set(event, handler);
        return vi.fn();
      },
    );
    const { onMailContentActivity } = await import("./nativeWindows");
    const activity = vi.fn();

    await onMailContentActivity(activity);
    handlers.get("mail-content-activity")?.({
      payload: { accountId: "account-1" },
    });

    expect(activity).toHaveBeenCalledWith("account-1");
  });

  it("delivers durable mutation outcomes with the immutable message identity", async () => {
    const handlers = new Map<string, ListenHandler>();
    eventMocks.listen.mockImplementation(
      async (event: string, handler: ListenHandler) => {
        handlers.set(event, handler);
        return vi.fn();
      },
    );
    const { onMailOperationUpdated } = await import("./nativeWindows");
    const updated = vi.fn();
    const event = {
      operationId: "operation-1",
      accountId: "account-1",
      messageId: "message-1",
      kind: "message_read" as const,
      status: "permanent_failed" as const,
      error: "provider rejected the flag update",
    };

    await onMailOperationUpdated(updated);
    handlers.get("mail-operation-updated")?.({ payload: decoded(event) });

    expect(updated).toHaveBeenCalledWith(event);
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
