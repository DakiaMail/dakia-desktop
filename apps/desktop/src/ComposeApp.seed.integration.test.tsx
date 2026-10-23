import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { StrictMode } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "./i18n";
import { ComposeApp } from "./ComposeApp";
import type { Account } from "./types";

const events = vi.hoisted(() => ({
  emitTo: vi.fn(),
  listener: undefined as ((event: { payload: unknown }) => void) | undefined,
  listen: vi.fn(),
}));
const apiMocks = vi.hoisted(() => ({
  accounts: vi.fn(),
  aiAvailable: vi.fn(),
  acknowledgeSendRecoverySeed: vi.fn(),
  draft: vi.fn(),
  forwardAttachments: vi.fn(),
  readSendRecoverySeed: vi.fn(),
  readDroppedFiles: vi.fn(),
  send: vi.fn(),
}));
const feedback = vi.hoisted(() => ({ showNativeMessage: vi.fn() }));
const close = vi.fn();
let currentLabel = "compose-unset";

vi.mock("@tauri-apps/api/event", () => events);
vi.mock("@tauri-apps/api/window", () => ({
  getCurrentWindow: () => ({ label: currentLabel, close }),
}));
vi.mock("@tauri-apps/api/webview", () => ({
  getCurrentWebview: () => ({
    onDragDropEvent: vi.fn(async () => () => undefined),
    listen: vi.fn(async () => () => undefined),
  }),
}));
vi.mock("./api", () => ({ api: apiMocks }));
vi.mock("./features", () => ({ AI_FEATURES_VISIBLE: false }));
vi.mock("./nativeFeedback", () => feedback);

const account: Account = {
  id: "account-1",
  email: "me@example.test",
  account_name: "Personal",
  display_name: "Me",
  provider_id: "fastmail",
  auth: { type: "password", username: "me@example.test" },
  imap_host: "imap.example.test",
  imap_port: 993,
  imap_security: "tls",
  smtp_host: "smtp.example.test",
  smtp_port: 465,
  smtp_security: "tls",
  archive_mailbox: "Archive",
  spam_mailbox: "Spam",
  enabled: true,
};

function installChildIndexedDb() {
  const records = new Map<string, unknown>();
  const events = { puts: 0 };
  const database = {
    objectStoreNames: { contains: () => true },
    close: vi.fn(),
    createObjectStore: vi.fn(),
    transaction: vi.fn(() => {
      const transaction: {
        oncomplete?: () => void;
        onerror?: () => void;
        onabort?: () => void;
        error?: DOMException | null;
        objectStore: () => {
          get: (key: string) => IDBRequest;
          put: (value: unknown, key: string) => void;
        };
      } = {
        objectStore: () => ({
          get: (key) => {
            const request: Partial<IDBRequest> = {};
            queueMicrotask(() => {
              Object.assign(request, { result: records.get(key) });
              (request.onsuccess as ((event: Event) => void) | null)?.(
                new Event("success"),
              );
              transaction.oncomplete?.();
            });
            return request as IDBRequest;
          },
          put: (value, key) => {
            events.puts += 1;
            queueMicrotask(() => {
              records.set(key, value);
              transaction.oncomplete?.();
            });
          },
        }),
      };
      return transaction as unknown as IDBTransaction;
    }),
  };
  Object.defineProperty(globalThis, "indexedDB", {
    configurable: true,
    value: {
      open: vi.fn(() => {
        const request: Partial<IDBOpenDBRequest> = {
          result: database as unknown as IDBDatabase,
        };
        queueMicrotask(() =>
          (request.onsuccess as ((event: Event) => void) | null)?.(
            new Event("success"),
          ),
        );
        return request as IDBOpenDBRequest;
      }),
    },
  });
  return { events, records };
}

describe("ComposeApp native seed handoff", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    localStorage.clear();
    sessionStorage.clear();
    close.mockResolvedValue(undefined);
    Object.defineProperty(window, "__TAURI_INTERNALS__", {
      configurable: true,
      value: {},
    });
    events.listen.mockImplementation(async (event, handler) => {
      if (event === "compose-target") events.listener = handler;
      return () => {
        if (event === "compose-target" && events.listener === handler) {
          events.listener = undefined;
        }
      };
    });
    apiMocks.accounts.mockResolvedValue([account]);
    apiMocks.aiAvailable.mockResolvedValue(false);
    apiMocks.draft.mockResolvedValue("");
    apiMocks.forwardAttachments.mockResolvedValue([]);
    apiMocks.readDroppedFiles.mockResolvedValue([]);
    apiMocks.acknowledgeSendRecoverySeed.mockResolvedValue(undefined);
  });

  it("uses the real child listener and storage under StrictMode, then restores the same exact draft on reload", async () => {
    const token = "b3d2f6fc-b887-4efc-84fb-5fea99e45bd6";
    currentLabel = "compose-11111111-1111-4111-8111-111111111111";
    window.history.replaceState(
      {},
      "",
      `/?view=compose&seedDbKey=${token}&opener=main`,
    );
    // The fresh child has no existing local draft. Install its isolated store
    // only after readiness, matching a new WebView that receives the target.
    Object.defineProperty(globalThis, "indexedDB", {
      configurable: true,
      value: undefined,
    });
    const first = render(
      <StrictMode>
        <ComposeApp />
      </StrictMode>,
    );

    await waitFor(() => expect(events.listener).toEqual(expect.any(Function)));
    await waitFor(() =>
      expect(events.emitTo).toHaveBeenCalledWith("main", "compose-ready", {
        label: currentLabel,
        seedToken: token,
      }),
    );
    const indexed = installChildIndexedDb();
    await act(async () =>
      events.listener?.({
        payload: {
          label: currentLabel,
          seedToken: token,
          seed: {
            sendAttemptId: "attempt-a",
            accountId: account.id,
            to: "cr@example.test",
            subject: "Re: exact handoff",
            body: "The body stays private to this child.",
            attachments: [
              {
                filename: "résumé.pdf",
                mime_type: "application/pdf",
                content_base64: "AA==",
                size_bytes: 1,
              },
            ],
          },
        },
      }),
    );
    expect(await screen.findByLabelText("To")).toHaveValue("cr@example.test");
    expect(screen.getByLabelText("Subject")).toHaveValue("Re: exact handoff");
    expect(screen.getByLabelText("From")).toHaveValue(account.id);
    expect(indexed.events.puts).toBeGreaterThanOrEqual(1);
    expect(indexed.records.get(token)).toEqual(
      expect.objectContaining({ to: "cr@example.test" }),
    );
    expect(localStorage.getItem(`dakia.compose-seed.${token}`)).toBeNull();
    expect(sessionStorage.getItem("dakia.compose-active-seed")).toContain(
      currentLabel,
    );

    first.unmount();
    render(
      <StrictMode>
        <ComposeApp />
      </StrictMode>,
    );
    expect(await screen.findByLabelText("To")).toHaveValue("cr@example.test");
    expect(screen.getByLabelText("Subject")).toHaveValue("Re: exact handoff");
    expect(close).not.toHaveBeenCalled();
    expect(feedback.showNativeMessage).not.toHaveBeenCalled();
  });

  it("recovers the child-local draft when session storage rejects after delivery", async () => {
    const token = "c6a795de-4cda-47ed-bd00-0fa8ad18265a";
    currentLabel = "compose-33333333-3333-4333-8333-333333333333";
    window.history.replaceState(
      {},
      "",
      `/?view=compose&seedDbKey=${token}&opener=main`,
    );
    Object.defineProperty(globalThis, "indexedDB", {
      configurable: true,
      value: undefined,
    });
    const originalSetItem = Storage.prototype.setItem;
    const deniedSession = vi
      .spyOn(Storage.prototype, "setItem")
      .mockImplementation(function (this: Storage, key, value) {
        if (this === sessionStorage && key === "dakia.compose-active-seed") {
          throw new DOMException("Blocked", "SecurityError");
        }
        return originalSetItem.call(this, key, value);
      });
    const first = render(<ComposeApp />);
    await waitFor(() => expect(events.listener).toEqual(expect.any(Function)));
    await waitFor(() => expect(events.emitTo).toHaveBeenCalled());
    const indexed = installChildIndexedDb();
    await act(async () =>
      events.listener?.({
        payload: {
          label: currentLabel,
          seedToken: token,
          seed: { accountId: account.id, to: "recover@example.test" },
        },
      }),
    );
    expect(await screen.findByLabelText("To")).toHaveValue(
      "recover@example.test",
    );
    expect(indexed.records.get(token)).toEqual(
      expect.objectContaining({ to: "recover@example.test" }),
    );
    first.unmount();
    deniedSession.mockRestore();

    render(<ComposeApp />);
    expect(await screen.findByLabelText("To")).toHaveValue(
      "recover@example.test",
    );
    expect(close).not.toHaveBeenCalled();
  });

  it("loads a recovery only into its child store before acknowledgement and keeps an uncertain send disabled", async () => {
    const recoveryKey = "c6a795de-4cda-47ed-bd00-0fa8ad18265a";
    const recoveryAccountId = "b9557475-508f-48b0-b492-d36b65fb2ecc";
    currentLabel = "compose-99999999-9999-4999-8999-999999999999";
    window.history.replaceState(
      {},
      "",
      `/?view=compose&recoveryKey=${recoveryKey}&opener=main`,
    );
    const indexed = installChildIndexedDb();
    apiMocks.acknowledgeSendRecoverySeed.mockImplementation(async () => {
      expect(indexed.records.get(recoveryKey)).toEqual(
        expect.objectContaining({
          sendAttemptId: "12c7a145-2a9c-4660-961d-387ba3a3bc45",
        }),
      );
    });
    apiMocks.accounts.mockResolvedValue([
      { ...account, id: recoveryAccountId },
    ]);
    apiMocks.readSendRecoverySeed.mockResolvedValue({
      accountId: recoveryAccountId,
      attemptId: "12c7a145-2a9c-4660-961d-387ba3a3bc45",
      status: "delivery_uncertain",
      message: {
        accountId: recoveryAccountId,
        to: ["recover@example.test"],
        cc: [],
        bcc: [],
        subject: "Keep this exact message",
        bodyText: "Plain body",
        bodyHtml: null,
        inReplyTo: null,
        references: null,
        attachments: [
          {
            filename: "message.txt",
            mimeType: "text/plain",
            contentBase64: "AQI=",
            sizeBytes: 2,
          },
        ],
      },
    });
    indexed.records.set(recoveryKey, {
      sendAttemptId: "a4f7753e-6b3f-406d-b422-5167a0318f33",
      accountId: recoveryAccountId,
      to: "forged-cache@example.test",
      subject: "Forged child cache",
    });

    render(<ComposeApp />);

    expect(await screen.findByLabelText("To")).toHaveValue(
      "recover@example.test",
    );
    expect(screen.getByLabelText("Subject")).toHaveValue(
      "Keep this exact message",
    );
    expect(
      screen.getByRole("button", { name: /Delivery status unknown/ }),
    ).toBeDisabled();
    expect(indexed.records.get(recoveryKey)).toEqual(
      expect.objectContaining({
        sendAttemptId: "12c7a145-2a9c-4660-961d-387ba3a3bc45",
        bodyHtml: null,
        attachments: [
          expect.objectContaining({
            filename: "message.txt",
            content_base64: "AQI=",
            size_bytes: 2,
          }),
        ],
      }),
    );
    expect(apiMocks.acknowledgeSendRecoverySeed).toHaveBeenCalledWith(
      recoveryKey,
    );
    expect(apiMocks.readSendRecoverySeed).toHaveBeenCalledWith(recoveryKey);
  });

  it("fails closed for a malformed recovery URL even when a matching session seed exists", async () => {
    const recoveryKey = "ae011751-fbc5-4a7e-bd2f-eb19a2c3c93a";
    currentLabel = "compose-55555555-5555-4555-8555-555555555555";
    window.history.replaceState(
      {},
      "",
      `/?view=compose&recoveryKey=${recoveryKey}&seedDbKey=ambiguous`,
    );
    sessionStorage.setItem(
      "dakia.compose-active-seed",
      JSON.stringify({
        label: currentLabel,
        recoveryKey,
        seed: {
          sendAttemptId: "cd72e292-9a3c-41c8-a4b6-45fcda4791e7",
          accountId: account.id,
          to: "session-cache@example.test",
        },
      }),
    );

    render(<ComposeApp />);

    await waitFor(() => expect(feedback.showNativeMessage).toHaveBeenCalled());
    expect(close).toHaveBeenCalled();
    expect(apiMocks.readSendRecoverySeed).not.toHaveBeenCalled();
    expect(apiMocks.acknowledgeSendRecoverySeed).not.toHaveBeenCalled();
    expect(screen.queryByLabelText("To")).not.toBeInTheDocument();
  });

  it("keeps an untouched recovered message byte-for-field exact through persistence and retry", async () => {
    const recoveryKey = "5e637b1e-34ca-4fa4-9d4b-9bd5f031e653";
    const recoveryAccountId = "e57418e3-b923-41ee-b14e-b3c8389831f2";
    const attemptId = "8f93cc28-1050-49df-9d3a-826c68df694c";
    const recoveredHtml =
      "<p>Rendered text<script>window.untrusted = true</script></p>";
    currentLabel = "compose-88888888-8888-4888-8888-888888888888";
    window.history.replaceState(
      {},
      "",
      `/?view=compose&recoveryKey=${recoveryKey}&opener=main`,
    );
    const indexed = installChildIndexedDb();
    apiMocks.accounts.mockResolvedValue([
      { ...account, id: recoveryAccountId },
      { ...account, id: "a0afad8d-624a-425b-b902-ed43df0c7c50" },
    ]);
    apiMocks.readSendRecoverySeed.mockResolvedValue({
      accountId: recoveryAccountId,
      attemptId,
      status: "not_submitted",
      message: {
        accountId: recoveryAccountId,
        to: ["First Person <first@example.test>", "second@example.test"],
        cc: ["copy@example.test"],
        bcc: ["hidden@example.test"],
        subject: "Exact saved subject",
        bodyText: "The original canonical plain body.",
        bodyHtml: recoveredHtml,
        inReplyTo: "<reply@example.test>",
        references: "<first@example.test> <reply@example.test>",
        attachments: [
          {
            filename: "exact.bin",
            mimeType: "application/octet-stream",
            contentBase64: "AAE=",
            sizeBytes: 2,
          },
        ],
      },
    });
    apiMocks.send.mockImplementation(() => new Promise(() => undefined));

    render(<ComposeApp />);

    expect(await screen.findByLabelText("Subject")).toHaveValue(
      "Exact saved subject",
    );
    const from = screen.getByLabelText("From");
    expect(from).toHaveValue(recoveryAccountId);
    expect(from).toBeDisabled();
    fireEvent.change(from, {
      target: { value: "a0afad8d-624a-425b-b902-ed43df0c7c50" },
    });
    await waitFor(() =>
      expect(indexed.records.get(recoveryKey)).toEqual(
        expect.objectContaining({
          recoveryMessage: {
            to: ["First Person <first@example.test>", "second@example.test"],
            cc: ["copy@example.test"],
            bcc: ["hidden@example.test"],
            subject: "Exact saved subject",
            bodyText: "The original canonical plain body.",
            bodyHtml: recoveredHtml,
            inReplyTo: "<reply@example.test>",
            references: "<first@example.test> <reply@example.test>",
            attachments: [
              {
                filename: "exact.bin",
                mime_type: "application/octet-stream",
                content_base64: "AAE=",
                size_bytes: 2,
              },
            ],
          },
        }),
      ),
    );

    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    await waitFor(() =>
      expect(apiMocks.send).toHaveBeenCalledWith(
        expect.objectContaining({
          send_attempt_id: attemptId,
          account_id: recoveryAccountId,
          to: ["First Person <first@example.test>", "second@example.test"],
          cc: ["copy@example.test"],
          bcc: ["hidden@example.test"],
          subject: "Exact saved subject",
          body_text: "The original canonical plain body.",
          body_html: recoveredHtml,
          in_reply_to: "<reply@example.test>",
          references: "<first@example.test> <reply@example.test>",
          attachments: [
            {
              filename: "exact.bin",
              mime_type: "application/octet-stream",
              content_base64: "AAE=",
              size_bytes: 2,
            },
          ],
        }),
      ),
    );
  });

  it("switches to the sanitized editor content only after the recovered body is edited", async () => {
    const recoveryKey = "92345391-a5db-4404-b1fa-3186e67b3f82";
    const recoveryAccountId = "265d1b8e-8e26-4b01-85f7-a4d3fc6a4917";
    const attemptId = "83c2d5ef-a9fb-4de4-9c80-b97e1a47dfbc";
    currentLabel = "compose-77777777-7777-4777-8777-777777777777";
    window.history.replaceState(
      {},
      "",
      `/?view=compose&recoveryKey=${recoveryKey}&opener=main`,
    );
    const indexed = installChildIndexedDb();
    apiMocks.accounts.mockResolvedValue([
      { ...account, id: recoveryAccountId },
    ]);
    apiMocks.readSendRecoverySeed.mockResolvedValue({
      accountId: recoveryAccountId,
      attemptId,
      status: "not_submitted",
      message: {
        accountId: recoveryAccountId,
        to: ["recover@example.test"],
        cc: [],
        bcc: [],
        subject: "Edit body",
        bodyText: "Canonical source text",
        bodyHtml: "<p>Canonical <script>discard()</script>HTML</p>",
        inReplyTo: null,
        references: null,
        attachments: [],
      },
    });

    render(<ComposeApp />);

    const editor = await screen.findByLabelText("Write your message…");
    (editor as HTMLDivElement).innerHTML = "<p>Edited body</p>";
    fireEvent.input(editor);

    await waitFor(() =>
      expect(indexed.records.get(recoveryKey)).toEqual(
        expect.objectContaining({
          recoveryMessage: expect.objectContaining({
            bodyText: "Edited body",
            bodyHtml: "<p>Edited body</p>",
          }),
        }),
      ),
    );
    apiMocks.send.mockResolvedValue(undefined);
    fireEvent.click(screen.getByRole("button", { name: /^Send/ }));
    await waitFor(() =>
      expect(apiMocks.send).toHaveBeenCalledWith(
        expect.objectContaining({
          send_attempt_id: attemptId,
          body_text: "Edited body",
          body_html: "<p>Edited body</p>",
        }),
      ),
    );
  });
});
