import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createAgentSession, DefaultResourceLoader, ModelRuntime, SessionManager } from "@earendil-works/pi-coding-agent";
import { fauxAssistantMessage, fauxProvider, fauxText, fauxToolCall, InMemoryCredentialStore, type Context } from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import type { BridgeTransport } from "./bridge.js";
import { AgentProfile } from "./profile.js";
import { createDakiaTools } from "./tools.js";

class MailFixtureBridge implements BridgeTransport {
  readonly calls: Array<{ method: string; params: Record<string, unknown> }> = [];
  async call<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    this.calls.push({ method, params });
    if (method === "mail.search") return {
      messages: [{ id: "insurance-1", account_id: "home", subject: "Claim update", from: "insurer@example.test", received_at: "2026-10-16T09:00:00Z", snippet: "Assessment booked for 21 October." }],
      incomplete: true,
    } as T;
    if (method === "draft.save") return { id: "draft-john", account_id: "work", revision: 0 } as T;
    return {} as T;
  }
  close(): void {}
}

const directories: string[] = [];
afterEach(async () => { await Promise.all(directories.splice(0).map((directory) => rm(directory, { recursive: true, force: true }))); });

async function runFlow(response: (context: Context) => ReturnType<typeof fauxAssistantMessage>, prompt: string, bridge: MailFixtureBridge) {
  const directory = await mkdtemp(join(tmpdir(), "dakia-agent-flow-"));
  directories.push(directory);
  const profile = new AgentProfile(directory);
  await profile.ensure();
  const faux = fauxProvider({ provider: "dakia-test", models: [{ id: "mail", contextWindow: 8000, maxTokens: 1000 }] });
  faux.setResponses([
    response,
    fauxAssistantMessage(fauxText("Completed using Dakia mail tools.")),
  ]);
  try {
    const loader = new DefaultResourceLoader({ cwd: profile.agentDir, agentDir: profile.agentDir, noExtensions: true, noSkills: true, noPromptTemplates: true, noThemes: true, noContextFiles: true, systemPrompt: "Use only Dakia tools." });
    await loader.reload();
    const runtime = await ModelRuntime.create({ credentials: new InMemoryCredentialStore(), modelsPath: null, refreshOnCreate: false });
    runtime.registerNativeProvider(faux.provider);
    const { session } = await createAgentSession({
      cwd: profile.agentDir,
      modelRuntime: runtime,
      model: faux.getModel(),
      sessionManager: SessionManager.inMemory(profile.agentDir),
      resourceLoader: loader,
      noTools: "builtin",
      customTools: createDakiaTools({ bridge, profile, approvals: { interactive: false, confirm: async () => false } }),
    });
    await session.prompt(prompt);
    return session.state.messages;
  } finally {
  }
}

describe("Pi SDK mail-agent flows", () => {
  it("passes dated home-insurance search results and the incomplete-search flag through the real tool-call loop", async () => {
    const bridge = new MailFixtureBridge();
    const messages = await runFlow(() => fauxAssistantMessage(fauxToolCall("search_mail", { query: "home insurance case", limit: 10 })), "What's the latest status with my home insurance case?", bridge);
    expect(bridge.calls).toEqual([{ method: "mail.search", params: { query: "home insurance case", limit: 10 } }]);
    const serialized = JSON.stringify(messages);
    expect(serialized).toContain("2026-10-16T09:00:00Z");
    expect(serialized).toContain("incomplete");
  });

  it("creates a John reply draft through the actual tool path without sending it", async () => {
    const bridge = new MailFixtureBridge();
    await runFlow(() => fauxAssistantMessage(fauxToolCall("save_draft", {
      account_id: "work",
      to: [{ email: "john@example.test", name: "John" }],
      subject: "Today",
      body_text: "Hi John, I can't make it today.",
    })), "Draft a reply to John telling him I can't make it today", bridge);
    expect(bridge.calls).toEqual([{ method: "draft.save", params: {
      account_id: "work", to: [{ email: "john@example.test", name: "John" }], subject: "Today", body_text: "Hi John, I can't make it today.",
    } }]);
    expect(bridge.calls.some((call) => call.method === "draft.send")).toBe(false);
  });
});
