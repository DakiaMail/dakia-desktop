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

  it("subscribes to the contacted-people privacy event with its exact payload", async () => {
    const handler = vi.fn();
    eventMocks.listen.mockImplementation(
      async (
        event: string,
        listener: (event: {
          payload: { enabled: boolean; cleared: boolean };
        }) => void,
      ) => {
        expect(event).toBe("contacted-people-changed");
        listener({ payload: { enabled: false, cleared: true } });
        return vi.fn();
      },
    );
    const { api } = await import("./api");

    await api.onContactedPeopleChanged(handler);

    expect(handler).toHaveBeenCalledWith({ enabled: false, cleared: true });
  });

  it("subscribes to versioned search-progress coverage with its exact payload", async () => {
    const handler = vi.fn();
    const progress = {
      sessionId: "search-session",
      revision: 4,
      coverage: [
        {
          account_id: "account-1",
          mailbox: "INBOX",
          state: "provider_searched",
          detail: null,
        },
      ],
    };
    eventMocks.listen.mockImplementation(
      async (
        event: string,
        listener: (event: { payload: unknown }) => void,
      ) => {
        expect(event).toBe("search-progress");
        listener({ payload: progress });
        return vi.fn();
      },
    );
    const { onSearchProgress } = await import("./nativeWindows");

    await onSearchProgress(handler);

    expect(handler).toHaveBeenCalledWith(progress);
  });

  it("preserves the complete versioned search request and response contracts", async () => {
    const request = {
      client_request_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
      raw_query: 'from:"Mara Example" has:attachment',
      account_ids: ["11111111-1111-4111-8111-111111111111"],
      scope: {
        mailbox: "Archive::All Mail",
        include_spam_trash: true,
      },
      execution_mode: "hybrid" as const,
      page_size: 25,
      continuation: null,
    };
    const firstPage = {
      conversations: [],
      match_evidence: {
        "thread-01": {
          primary_message_id: "message-02",
          matched_message_ids: ["message-01", "message-02"],
          match_count: 2,
          excerpt: "Mara Example attached the report",
        },
      },
      coverage: [
        {
          account_id: request.account_ids[0],
          mailbox: request.scope.mailbox,
          state: "provider_partial",
          detail: "More provider results are available",
        },
      ],
      continuation: "opaque-search-continuation",
      session_id: "22222222-2222-4222-8222-222222222222",
      revision: 7,
    };
    const nextRequest = {
      ...request,
      continuation: firstPage.continuation,
    };
    const nextPage = {
      ...firstPage,
      match_evidence: {},
      coverage: [],
      continuation: null,
      revision: 8,
    };
    apiMocks.invoke.mockImplementation((command: string) => {
      if (command === "start_search") return Promise.resolve(firstPage);
      if (command === "next_search_page") return Promise.resolve(nextPage);
      return Promise.resolve(undefined);
    });
    const { api } = await import("./api");

    await expect(api.startSearchV2(request)).resolves.toEqual(firstPage);
    await expect(api.nextSearchPageV2(nextRequest)).resolves.toEqual(nextPage);
    await expect(
      api.cancelSearchV2(firstPage.session_id),
    ).resolves.toBeUndefined();

    expect(apiMocks.invoke.mock.calls).toEqual([
      ["start_search", { request }],
      ["next_search_page", { request: nextRequest }],
      ["cancel_search", { sessionId: firstPage.session_id }],
    ]);
    expect(firstPage).toHaveProperty("session_id");
    expect(firstPage).not.toHaveProperty("sessionId");
    expect(firstPage.match_evidence["thread-01"]).toEqual({
      primary_message_id: "message-02",
      matched_message_ids: ["message-01", "message-02"],
      match_count: 2,
      excerpt: "Mara Example attached the report",
    });
  });

  it("preserves the native SearchErrorV2 envelope", async () => {
    const searchError = {
      position: 5,
      category: "unsupported",
      unsupported_operator: "near",
      message: "This search operator is not supported yet.",
    };
    apiMocks.invoke.mockRejectedValueOnce(searchError);
    const { api } = await import("./api");

    await expect(
      api.startSearchV2({
        raw_query: "near:person",
        account_ids: [],
        scope: { include_spam_trash: false },
        execution_mode: "local",
        page_size: 50,
        continuation: null,
      }),
    ).rejects.toEqual(searchError);
    expect(searchError).toHaveProperty("unsupported_operator", "near");
    expect(searchError).not.toHaveProperty("unsupportedOperator");
  });

  it("uses exact contacted-people commands and preserves account ownership and hidden state", async () => {
    const suggestion = {
      address: "mara@example.test",
      display_name: "Mara Example",
      formatted_address: "Mara Example <mara@example.test>",
      account_id: "33333333-3333-4333-8333-333333333333",
      account_send_count: 4,
      account_last_contacted_at: "2026-09-11T09:30:00Z",
      last_contacted_at: "2026-09-11T09:30:00Z",
      hidden: false,
    };
    apiMocks.invoke.mockImplementation((command: string) => {
      if (command === "suggest_contacted_people") {
        return Promise.resolve([suggestion]);
      }
      if (command === "get_autocomplete_settings") {
        return Promise.resolve({ enabled: true });
      }
      if (command === "set_autocomplete_settings") {
        return Promise.resolve({ enabled: false });
      }
      return Promise.resolve(undefined);
    });
    const { api } = await import("./api");

    await expect(
      api.suggestContactedPeople("mar", suggestion.account_id),
    ).resolves.toEqual([suggestion]);
    await expect(
      api.hideContactedPerson(suggestion.address),
    ).resolves.toBeUndefined();
    await expect(api.clearContactedPeople()).resolves.toBeUndefined();
    await expect(api.contactedPeopleSettings()).resolves.toEqual({
      enabled: true,
    });
    await expect(api.setContactedPeopleSettings(false)).resolves.toEqual({
      enabled: false,
    });

    expect(apiMocks.invoke.mock.calls).toEqual([
      [
        "suggest_contacted_people",
        { prefix: "mar", accountId: suggestion.account_id, limit: 8 },
      ],
      ["hide_contacted_person", { address: suggestion.address }],
      ["clear_contacted_people"],
      ["get_autocomplete_settings"],
      ["set_autocomplete_settings", { enabled: false }],
    ]);
    expect(suggestion).toHaveProperty("account_id", suggestion.account_id);
    expect(suggestion).toHaveProperty("hidden", false);
    expect(suggestion).not.toHaveProperty("accountId");
  });

  it("sends an explicit null account for global contacted-people suggestions", async () => {
    apiMocks.invoke.mockResolvedValueOnce([]);
    const { api } = await import("./api");

    await api.suggestContactedPeople("");

    expect(apiMocks.invoke).toHaveBeenCalledWith("suggest_contacted_people", {
      prefix: "",
      accountId: null,
      limit: 8,
    });
  });

  it("loads selectable search mailboxes through the exact native command", async () => {
    const mailboxes = [
      { localPath: "Projects/Client A", selectable: true },
      { localPath: "Projects", selectable: false },
    ];
    apiMocks.invoke.mockResolvedValueOnce(mailboxes);
    const { api } = await import("./api");

    await expect(api.listSearchMailboxes()).resolves.toEqual(mailboxes);
    expect(apiMocks.invoke).toHaveBeenCalledWith("list_search_mailboxes");
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

  it("uses the mailbox action command with the exact permanent-delete locator", async () => {
    apiMocks.invoke.mockResolvedValue(undefined);
    const { api } = await import("./api");

    await api.action("account-1", "Archive::All Mail", 42, "delete");

    expect(apiMocks.invoke).toHaveBeenCalledWith("apply_mailbox_action", {
      accountId: "account-1",
      mailbox: "Archive::All Mail",
      uid: 42,
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
