import { MantineProvider } from "@mantine/core";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import "../i18n";
import { api } from "../api";
import type { ThreadContextBundle, ThreadContextRevision } from "../types";
import { ThreadContextPanel } from "./ThreadContextPanel";

const target = {
  accountId: "account-1",
  localMessageId: "message-1",
  threadId: "thread-1",
  mailbox: "INBOX",
};

const bundle: ThreadContextBundle = {
  conversation: {
    locator: { accountId: "account-1", conversationId: "conversation-1" },
    requiresReview: false,
  },
  notes: { content: "Call the insurer on Friday", revision: 3 },
  agentMemory: { content: "Claim number is 42", revision: 5 },
};

function renderPanel() {
  return render(
    <MantineProvider>
      <ThreadContextPanel target={target} />
    </MantineProvider>,
  );
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((next, fail) => {
    resolve = next;
    reject = fail;
  });
  return { promise, resolve, reject };
}

describe("ThreadContextPanel", () => {
  beforeEach(() => {
    vi.restoreAllMocks();
    vi.spyOn(api, "threadContext").mockResolvedValue(bundle);
    vi.spyOn(api, "updateThreadContext").mockResolvedValue({
      content: "Call the insurer on Friday",
      revision: 4,
    });
    vi.spyOn(api, "threadContextHistory").mockResolvedValue([]);
    vi.spyOn(api, "restoreThreadContext").mockResolvedValue({
      content: "Restored",
      revision: 4,
    });
  });

  it("loads both documents through an immutable selected-conversation target", async () => {
    renderPanel();
    fireEvent.click(
      screen.getByRole("button", { name: "Show conversation context" }),
    );

    expect(await screen.findByDisplayValue("Call the insurer on Friday")).toBeVisible();
    expect(api.threadContext).toHaveBeenCalledWith(target);

    fireEvent.click(screen.getByRole("tab", { name: "Agent memory" }));
    expect(screen.getByDisplayValue("Claim number is 42")).toBeVisible();
  });

  it("updates the visible note immediately and keeps an unsaved edit after a failure", async () => {
    vi.mocked(api.updateThreadContext).mockRejectedValueOnce(new Error("offline"));
    renderPanel();
    fireEvent.click(
      screen.getByRole("button", { name: "Show conversation context" }),
    );
    const editor = await screen.findByDisplayValue("Call the insurer on Friday");
    fireEvent.change(editor, { target: { value: "Call Monday instead" } });
    expect(screen.getByDisplayValue("Call Monday instead")).toBeVisible();

    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await screen.findByText("Could not save this context. Your changes are still here.");
    expect(screen.getByDisplayValue("Call Monday instead")).toBeVisible();
    expect(api.updateThreadContext).toHaveBeenCalledWith({
      target,
      kind: "notes",
      content: "Call Monday instead",
      expectedRevision: 3,
    });
  });

  it("does not replace an unsaved edit when the window regains focus", async () => {
    renderPanel();
    fireEvent.click(
      screen.getByRole("button", { name: "Show conversation context" }),
    );
    const editor = await screen.findByDisplayValue("Call the insurer on Friday");
    fireEvent.change(editor, { target: { value: "My unfinished local note" } });
    await waitFor(() => expect(api.threadContext).toHaveBeenCalledTimes(1));

    window.dispatchEvent(new Event("focus"));
    await new Promise((resolve) => window.setTimeout(resolve, 0));

    expect(api.threadContext).toHaveBeenCalledTimes(1);
    expect(screen.getByDisplayValue("My unfinished local note")).toBeVisible();
  });

  it("refreshes clean context when the window regains focus after an agent update", async () => {
    vi.mocked(api.threadContext)
      .mockResolvedValueOnce(bundle)
      .mockResolvedValueOnce({
        ...bundle,
        notes: { content: "The insurer requested another document", revision: 4 },
      });
    renderPanel();
    fireEvent.click(
      screen.getByRole("button", { name: "Show conversation context" }),
    );
    await screen.findByDisplayValue("Call the insurer on Friday");

    window.dispatchEvent(new Event("focus"));

    expect(
      await screen.findByDisplayValue("The insurer requested another document"),
    ).toBeVisible();
  });

  it("loads revision history and restores a selected version against the visible revision", async () => {
    vi.mocked(api.threadContextHistory).mockResolvedValueOnce([
      { id: "revision-2", content: "Earlier note", revision: 2 },
    ]);
    renderPanel();
    fireEvent.click(
      screen.getByRole("button", { name: "Show conversation context" }),
    );
    await screen.findByDisplayValue("Call the insurer on Friday");
    fireEvent.click(screen.getByRole("button", { name: "History" }));
    fireEvent.click(await screen.findByRole("button", { name: "Restore version 2" }));

    await waitFor(() =>
      expect(api.restoreThreadContext).toHaveBeenCalledWith(
        target,
        "notes",
        "revision-2",
        3,
      ),
    );
    expect(await screen.findByDisplayValue("Restored")).toBeVisible();
  });

  it("does not display note history after switching to agent memory", async () => {
    const notesHistory = deferred<ThreadContextRevision[]>();
    vi.mocked(api.threadContextHistory).mockReturnValueOnce(notesHistory.promise);
    renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Show conversation context" }));
    await screen.findByDisplayValue("Call the insurer on Friday");
    fireEvent.click(screen.getByRole("button", { name: "History" }));
    fireEvent.click(screen.getByRole("tab", { name: "Agent memory" }));

    notesHistory.resolve([
      { id: "notes-revision-2", content: "Old note", revision: 2 },
    ]);
    await new Promise((resolve) => window.setTimeout(resolve, 0));

    expect(screen.getByDisplayValue("Claim number is 42")).toBeVisible();
    expect(screen.queryByText("Old note")).toBeNull();
  });

  it("keeps each document busy while cross-tab context saves are pending", async () => {
    const notesSave = deferred<{ content: string; revision: number }>();
    const memorySave = deferred<{ content: string; revision: number }>();
    vi.mocked(api.updateThreadContext)
      .mockReturnValueOnce(notesSave.promise)
      .mockReturnValueOnce(memorySave.promise);
    renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Show conversation context" }));
    const notesEditor = await screen.findByDisplayValue("Call the insurer on Friday");
    fireEvent.change(notesEditor, { target: { value: "Pending notes" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    expect(screen.getByRole("button", { name: "Saving…" })).toBeDisabled();

    fireEvent.click(screen.getByRole("tab", { name: "Agent memory" }));
    const memoryEditor = screen.getByDisplayValue("Claim number is 42");
    fireEvent.change(memoryEditor, { target: { value: "Pending memory" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));

    fireEvent.click(screen.getByRole("tab", { name: "Your notes" }));
    expect(screen.getByRole("button", { name: "Saving…" })).toBeDisabled();
    fireEvent.click(screen.getByRole("button", { name: "Saving…" }));
    expect(api.updateThreadContext).toHaveBeenCalledTimes(2);

    notesSave.resolve({ content: "Pending notes", revision: 4 });
    memorySave.resolve({ content: "Pending memory", revision: 6 });
    await new Promise((resolve) => window.setTimeout(resolve, 0));
    expect(api.updateThreadContext).toHaveBeenCalledTimes(2);
  });

  it("ignores a completed save after the reader switches conversations", async () => {
    const write = deferred<{ content: string; revision: number }>();
    const nextTarget = { ...target, localMessageId: "message-2", threadId: "thread-2" };
    const nextBundle = {
      ...bundle,
      conversation: {
        locator: { accountId: "account-1", conversationId: "conversation-2" },
        requiresReview: false,
      },
      notes: { content: "Second conversation", revision: 9 },
    };
    vi.mocked(api.threadContext).mockImplementation(async (requested) =>
      requested.localMessageId === "message-2" ? nextBundle : bundle,
    );
    vi.mocked(api.updateThreadContext).mockReturnValueOnce(write.promise);
    const view = renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Show conversation context" }));
    const editor = await screen.findByDisplayValue("Call the insurer on Friday");
    fireEvent.change(editor, { target: { value: "First conversation edit" } });
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    view.rerender(
      <MantineProvider>
        <ThreadContextPanel target={nextTarget} />
      </MantineProvider>,
    );
    expect(await screen.findByDisplayValue("Second conversation")).toBeVisible();

    write.resolve({ content: "First conversation edit", revision: 4 });
    await new Promise((resolve) => window.setTimeout(resolve, 0));

    expect(screen.getByDisplayValue("Second conversation")).toBeVisible();
  });

  it("keeps typing made during an in-flight focus refresh", async () => {
    const refresh = deferred<ThreadContextBundle>();
    vi.mocked(api.threadContext)
      .mockResolvedValueOnce(bundle)
      .mockReturnValueOnce(refresh.promise);
    renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Show conversation context" }));
    const editor = await screen.findByDisplayValue("Call the insurer on Friday");
    window.dispatchEvent(new Event("focus"));
    fireEvent.change(editor, { target: { value: "Typed while refreshing" } });
    refresh.resolve({
      ...bundle,
      notes: { content: "Agent version", revision: 4 },
    });

    await new Promise((resolve) => window.setTimeout(resolve, 0));
    expect(screen.getByDisplayValue("Typed while refreshing")).toBeVisible();
  });

  it("keeps later typing when an optimistic restore finishes", async () => {
    const restored = deferred<{ content: string; revision: number }>();
    vi.mocked(api.threadContextHistory).mockResolvedValueOnce([
      { id: "revision-2", content: "Earlier note", revision: 2 },
    ]);
    vi.mocked(api.restoreThreadContext).mockReturnValueOnce(restored.promise);
    renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Show conversation context" }));
    const editor = await screen.findByDisplayValue("Call the insurer on Friday");
    fireEvent.click(screen.getByRole("button", { name: "History" }));
    fireEvent.click(await screen.findByRole("button", { name: "Restore version 2" }));
    expect(screen.getByDisplayValue("Earlier note")).toBeVisible();

    fireEvent.change(editor, { target: { value: "Typing after restore" } });
    restored.resolve({ content: "Earlier note", revision: 4 });

    await new Promise((resolve) => window.setTimeout(resolve, 0));
    expect(screen.getByDisplayValue("Typing after restore")).toBeVisible();
  });

  it("rolls back a rejected notes restore after agent memory is edited", async () => {
    const restored = deferred<{ content: string; revision: number }>();
    vi.mocked(api.threadContextHistory).mockResolvedValueOnce([
      { id: "revision-2", content: "Earlier note", revision: 2 },
    ]);
    vi.mocked(api.restoreThreadContext).mockReturnValueOnce(restored.promise);
    renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Show conversation context" }));
    await screen.findByDisplayValue("Call the insurer on Friday");
    fireEvent.click(screen.getByRole("button", { name: "History" }));
    fireEvent.click(await screen.findByRole("button", { name: "Restore version 2" }));
    fireEvent.click(screen.getByRole("tab", { name: "Agent memory" }));
    fireEvent.change(screen.getByDisplayValue("Claim number is 42"), {
      target: { value: "Agent memory edit" },
    });
    restored.reject(new Error("offline"));
    await new Promise((resolve) => window.setTimeout(resolve, 0));

    fireEvent.click(screen.getByRole("tab", { name: "Your notes" }));
    expect(screen.getByDisplayValue("Call the insurer on Friday")).toBeVisible();
  });

  it("keeps a successful notes restore clean after agent memory is edited", async () => {
    const restored = deferred<{ content: string; revision: number }>();
    vi.mocked(api.threadContextHistory).mockResolvedValueOnce([
      { id: "revision-2", content: "Earlier note", revision: 2 },
    ]);
    vi.mocked(api.restoreThreadContext).mockReturnValueOnce(restored.promise);
    renderPanel();
    fireEvent.click(screen.getByRole("button", { name: "Show conversation context" }));
    await screen.findByDisplayValue("Call the insurer on Friday");
    fireEvent.click(screen.getByRole("button", { name: "History" }));
    fireEvent.click(await screen.findByRole("button", { name: "Restore version 2" }));
    fireEvent.click(screen.getByRole("tab", { name: "Agent memory" }));
    fireEvent.change(screen.getByDisplayValue("Claim number is 42"), {
      target: { value: "Agent memory edit" },
    });
    restored.resolve({ content: "Earlier note", revision: 4 });
    await new Promise((resolve) => window.setTimeout(resolve, 0));

    fireEvent.click(screen.getByRole("tab", { name: "Your notes" }));
    expect(screen.getByDisplayValue("Earlier note")).toBeVisible();
    expect(screen.getByText("Saved")).toBeVisible();
  });
});
