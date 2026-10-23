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

  it("uses Rust-owned routes for account and settings windows", async () => {
    const webviews = await import("@tauri-apps/api/webviewWindow");
    vi.mocked(webviews.getAllWebviewWindows).mockResolvedValue([]);
    const { openAccountWindow, openSettingsWindowForAccount } =
      await import("./nativeWindows");

    await openAccountWindow();
    await openSettingsWindowForAccount("d3e5f34a-feb7-4f33-9f9b-3c4f2e435c86");

    expect(apiMocks.invoke).toHaveBeenCalledWith("open_account_window");
    expect(apiMocks.invoke).toHaveBeenCalledWith("open_settings_window", {
      accountId: "d3e5f34a-feb7-4f33-9f9b-3c4f2e435c86",
    });
    expect(webviews.WebviewWindow).not.toHaveBeenCalled();
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
      api.content(
        fixture.commands.messageContent.arguments.messageId,
        fixture.commands.messageContent.arguments.expectedUidValidity,
      ),
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
        api.content(
          fixture.commands.messageContent.arguments.messageId,
          fixture.commands.messageContent.arguments.expectedUidValidity,
        ),
      ).rejects.toMatchObject({
        kind: variant.kind,
        retryable: variant.kind === "transient",
        message: "Message content could not be loaded",
      });
    }
  });

  it("decodes the shared structured remote-search response", async () => {
    apiMocks.invoke.mockImplementation((command: string) => {
      if (command === fixture.commands.searchRemote.command) {
        return Promise.resolve(decoded(fixture.remoteSearch.success));
      }
      return Promise.resolve(undefined);
    });
    const { api } = await import("./api");
    const query = fixture.commands.searchRemote.arguments.query;

    await expect(
      api.searchRemote(
        query.text,
        query.account_ids,
        query.mailbox,
        query.unread_only,
        query.flagged_only,
      ),
    ).resolves.toEqual(fixture.remoteSearch.success);
    expect(apiMocks.invoke).toHaveBeenCalledWith(
      fixture.commands.searchRemote.command,
      fixture.commands.searchRemote.arguments,
    );
  });

  it("uses exact recovery command names and camelCase arguments without putting seed content in the list", async () => {
    const commands = fixture.commands;
    apiMocks.invoke.mockImplementation((command: string) => {
      if (command === commands.listSendRecoveries.command) {
        return Promise.resolve(decoded(fixture.sendRecovery.summaryPage));
      }
      if (command === commands.readSendRecoverySeed.command) {
        return Promise.resolve(decoded(fixture.sendRecovery.seed));
      }
      return Promise.resolve(undefined);
    });
    const { api } = await import("./api");

    await expect(
      api.listSendRecoveries(commands.listSendRecoveries.arguments.accountIds),
    ).resolves.toEqual(fixture.sendRecovery.summaryPage);
    await api.openSendRecoveryWindow(
      commands.openSendRecoveryWindow.arguments.accountId,
      commands.openSendRecoveryWindow.arguments.attemptId,
    );
    await expect(
      api.readSendRecoverySeed(
        commands.readSendRecoverySeed.arguments.recoveryKey,
      ),
    ).resolves.toEqual(fixture.sendRecovery.seed);
    await api.acknowledgeSendRecoverySeed(
      commands.acknowledgeSendRecoverySeed.arguments.recoveryKey,
    );

    expect(apiMocks.invoke.mock.calls).toEqual([
      [
        commands.listSendRecoveries.command,
        commands.listSendRecoveries.arguments,
      ],
      [
        commands.openSendRecoveryWindow.command,
        commands.openSendRecoveryWindow.arguments,
      ],
      [
        commands.readSendRecoverySeed.command,
        commands.readSendRecoverySeed.arguments,
      ],
      [
        commands.acknowledgeSendRecoverySeed.command,
        commands.acknowledgeSendRecoverySeed.arguments,
      ],
    ]);
    expect(JSON.stringify(fixture.sendRecovery.summaryPage)).not.toContain(
      fixture.sendRecovery.seed.message.bodyText,
    );
    expect(JSON.stringify(fixture.sendRecovery.summaryPage)).not.toContain(
      fixture.sendRecovery.seed.message.to[0],
    );
  });

  it("rejects malformed or cross-account recovery list pages before Main can display them", async () => {
    const { api, sendRecoveryPageFromUnknown } = await import("./api");
    const page = decoded<Record<string, unknown>>(
      fixture.sendRecovery.summaryPage,
    );
    const item = (page.items as Array<Record<string, unknown>>)[0];
    const accountId =
      fixture.commands.listSendRecoveries.arguments.accountIds[0];
    const variants: Array<[string, unknown]> = [
      [
        "unknown status",
        {
          ...page,
          items: [{ ...item, status: "invented" }],
        },
      ],
      [
        "duplicate identity",
        {
          ...page,
          items: [{ ...item }, { ...item }],
          total_count: 2,
        },
      ],
      ["impossible count", { ...page, total_count: 0 }],
      ["empty opaque cursor", { ...page, next_cursor: "" }],
      [
        "unknown account",
        {
          ...page,
          items: [
            {
              ...item,
              account_id: "b25c2af5-2d65-42ca-835e-48901cc2d0a9",
            },
          ],
        },
      ],
    ];

    for (const [, variant] of variants) {
      expect(() => sendRecoveryPageFromUnknown(variant, [accountId])).toThrow(
        "Saved messages could not be loaded.",
      );
    }

    const unknownAccountPage = variants.at(-1)?.[1];
    apiMocks.invoke.mockResolvedValueOnce(unknownAccountPage);
    await expect(api.listSendRecoveries([accountId])).rejects.toThrow(
      "Saved messages could not be loaded.",
    );
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
      api.content(
        fixture.commands.messageContent.arguments.messageId,
        fixture.commands.messageContent.arguments.expectedUidValidity,
      ),
    ).resolves.toEqual(fixture.messageContent.providerSignature);
  });

  it("keeps unsafe-attachment warning metadata through the shared native decoder", async () => {
    apiMocks.invoke.mockResolvedValueOnce(
      decoded(fixture.messageContent.unsafeAttachment),
    );
    const { api } = await import("./api");

    await expect(
      api.content(
        fixture.commands.messageContent.arguments.messageId,
        fixture.commands.messageContent.arguments.expectedUidValidity,
      ),
    ).resolves.toEqual(fixture.messageContent.unsafeAttachment);
    expect(
      fixture.messageContent.unsafeAttachment.attachments[0],
    ).toMatchObject({
      filename: "invoice.猫.command",
      is_potentially_unsafe: true,
      presentation: "downloadable",
    });
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

    await api.content(
      commands.messageContent.arguments.messageId,
      commands.messageContent.arguments.expectedUidValidity,
    );
    await api.attachments(
      commands.messageAttachments.arguments.messageId,
      commands.messageAttachments.arguments.expectedUidValidity,
    );
    await api.hydrateMessage(
      commands.hydrateMessage.arguments.messageId,
      commands.hydrateMessage.arguments.expectedUidValidity,
    );
    await api.saveAttachment(
      commands.saveAttachment.arguments.messageId,
      commands.saveAttachment.arguments.attachmentId,
      commands.saveAttachment.arguments.expectedUidValidity,
    );
    await api.saveAllAttachments(
      commands.saveAllAttachments.arguments.messageId,
      commands.saveAllAttachments.arguments.expectedUidValidity,
    );
    await api.exportMessage(
      commands.exportMessage.arguments.messageId,
      commands.exportMessage.arguments.expectedUidValidity,
    );
    await api.forwardAttachments(
      commands.forwardAttachments.arguments.messageId,
      commands.forwardAttachments.arguments.expectedUidValidity,
    );
    await api.unsubscribe(
      commands.unsubscribeMessage.arguments.messageId,
      commands.unsubscribeMessage.arguments.expectedUidValidity,
    );
    const aiSettings = {
      provider: commands.aiSummarize.arguments.input.provider as "local",
      baseUrl: "",
      model: commands.aiSummarize.arguments.input.model,
      apiKey: "",
      executable: "",
      modelPath: "",
    };
    await api.summarize(
      aiSettings,
      commands.aiSummarize.arguments.input.messageTargets,
    );
    await api.draft(
      aiSettings,
      commands.aiDraft.arguments.input.messageTargets,
      commands.aiDraft.arguments.input.instruction,
    );
    await api.setCategory(
      commands.setMessageCategory.arguments.messageId,
      commands.setMessageCategory.arguments.category,
      commands.setMessageCategory.arguments.expectedUidValidity,
    );
    await api.setStarred(
      commands.setMessageStarred.arguments.messageId,
      commands.setMessageStarred.arguments.starred,
      commands.setMessageStarred.arguments.expectedUidValidity,
    );
    await api.setRead(
      commands.setMessageRead.arguments.messageId,
      commands.setMessageRead.arguments.read,
      commands.setMessageRead.arguments.expectedUidValidity,
    );
    await api.action(
      commands.applyMailboxAction.arguments.accountId,
      commands.applyMailboxAction.arguments.mailbox,
      commands.applyMailboxAction.arguments.uid,
      commands.applyMailboxAction.arguments.action as "archive",
      commands.applyMailboxAction.arguments.messageId,
      commands.applyMailboxAction.arguments.expectedUidValidity,
    );
    await api.sync(commands.syncAccount.arguments.accountId, progress);

    expect(apiMocks.invoke.mock.calls).toEqual([
      [commands.messageContent.command, commands.messageContent.arguments],
      [
        commands.messageAttachments.command,
        commands.messageAttachments.arguments,
      ],
      [commands.hydrateMessage.command, commands.hydrateMessage.arguments],
      [commands.saveAttachment.command, commands.saveAttachment.arguments],
      [
        commands.saveAllAttachments.command,
        commands.saveAllAttachments.arguments,
      ],
      [commands.exportMessage.command, commands.exportMessage.arguments],
      [
        commands.forwardAttachments.command,
        commands.forwardAttachments.arguments,
      ],
      [
        commands.unsubscribeMessage.command,
        commands.unsubscribeMessage.arguments,
      ],
      [commands.aiSummarize.command, commands.aiSummarize.arguments],
      [commands.aiDraft.command, commands.aiDraft.arguments],
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

  it("uses the mailbox action command with the exact permanent-delete locator", async () => {
    apiMocks.invoke.mockResolvedValue(undefined);
    const { api } = await import("./api");

    await api.action(
      "account-1",
      "Archive::All Mail",
      42,
      "delete",
      "message-42",
      77,
    );

    expect(apiMocks.invoke).toHaveBeenCalledWith("apply_mailbox_action", {
      accountId: "account-1",
      mailbox: "Archive::All Mail",
      uid: 42,
      action: "delete",
      messageId: "message-42",
      expectedUidValidity: 77,
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

  it("delivers the shared removed-account then late-arrival sequence through the native adapters", async () => {
    const handlers = new Map<string, ListenHandler>();
    eventMocks.listen.mockImplementation(
      async (event: string, handler: ListenHandler) => {
        handlers.set(event, handler);
        return vi.fn();
      },
    );
    const { onAccountRemoved, onMailArrived } = await import("./nativeWindows");
    const removed = vi.fn();
    const arrived = vi.fn();
    await Promise.all([onAccountRemoved(removed), onMailArrived(arrived)]);

    for (const step of fixture.events.lateArrivalAfterAccountRemoval) {
      handlers.get(step.event)?.({ payload: decoded(step.payload) });
    }

    const [removal, lateArrival] =
      fixture.events.lateArrivalAfterAccountRemoval;
    const latePayload = decoded<import("./types").MailArrival>(
      lateArrival.payload,
    );
    expect(removed).toHaveBeenCalledOnce();
    expect(removed).toHaveBeenCalledWith(removal.payload);
    expect(arrived).toHaveBeenCalledOnce();
    expect(arrived).toHaveBeenCalledWith(latePayload);
    expect(latePayload.messages[0]).toMatchObject({
      id: fixture.messageContent.unsafeAttachment.attachments[0].message_id,
      mailbox_uid_validity:
        fixture.commands.messageContent.arguments.expectedUidValidity,
      has_attachments: true,
    });
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
