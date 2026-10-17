import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  createAgentSession,
  DefaultResourceLoader,
  ModelRuntime,
  SessionManager,
} from "@earendil-works/pi-coding-agent";
import {
  fauxAssistantMessage,
  fauxProvider,
  fauxText,
  fauxToolCall,
  InMemoryCredentialStore,
} from "@earendil-works/pi-ai";
import { afterEach, describe, expect, it } from "vitest";
import type { BridgeTransport } from "./bridge.js";
import { AgentProfile } from "./profile.js";
import { createDakiaTools } from "./tools.js";

type Call = { method: string; params: Record<string, unknown> };

class AcceptanceFixtureBridge implements BridgeTransport {
  readonly calls: Call[] = [];

  async call<T>(
    method: string,
    params: Record<string, unknown> = {},
  ): Promise<T> {
    this.calls.push({ method, params });
    if (method === "mail.sync") {
      return {
        accounts: [{ account_id: "home", synced: 1 }],
        failures: [],
      } as T;
    }
    if (method === "mail.search" && params.query === "home insurance case") {
      return {
        messages: [
          {
            id: "insurance-1",
            account_id: "home",
            subject: "Claim update",
            from: "insurer@example.test",
            received_at: "2026-10-16T09:00:00Z",
            snippet: "The assessment is booked for 21 October.",
          },
        ],
        incomplete: true,
      } as T;
    }
    if (method === "mail.thread" && params.message_id === "insurance-1") {
      return {
        messages: [
          {
            id: "insurance-1",
            account_id: "home",
            subject: "Claim update",
            from: "insurer@example.test",
            received_at: "2026-10-16T09:00:00Z",
            body_text: "The assessment is booked for 21 October.",
          },
        ],
      } as T;
    }
    if (method === "mail.search" && params.query === "John") {
      return {
        messages: [
          {
            id: "john-1",
            account_id: "work",
            subject: "Plans today",
            from: "john@example.test",
            received_at: "2026-10-17T08:00:00Z",
          },
        ],
        incomplete: false,
      } as T;
    }
    if (method === "mail.thread" && params.message_id === "john-1") {
      return {
        messages: [
          {
            id: "john-1",
            account_id: "work",
            subject: "Plans today",
            from: "john@example.test",
            message_id: "<john-1@example.test>",
            body_text: "Can you still make it today?",
          },
        ],
      } as T;
    }
    if (method === "draft.save") {
      return {
        draft: { id: "draft-john", account_id: "work", revision: 0 },
      } as T;
    }
    throw new Error(`Unexpected bridge call: ${method}`);
  }

  close(): void {}
}

const directories: string[] = [];

afterEach(async () => {
  await Promise.all(
    directories
      .splice(0)
      .map((directory) => rm(directory, { recursive: true, force: true })),
  );
});

async function runScriptedFlow(
  prompt: string,
  bridge: AcceptanceFixtureBridge,
  responses: ReturnType<typeof fauxAssistantMessage>[],
) {
  const directory = await mkdtemp(join(tmpdir(), "dakia-agent-acceptance-"));
  directories.push(directory);
  const profile = new AgentProfile(directory);
  await profile.ensure();
  const faux = fauxProvider({
    provider: "dakia-acceptance",
    models: [{ id: "mail", contextWindow: 8000, maxTokens: 1000 }],
  });
  faux.setResponses(responses);
  const loader = new DefaultResourceLoader({
    cwd: profile.agentDir,
    agentDir: profile.agentDir,
    noExtensions: true,
    noSkills: true,
    noPromptTemplates: true,
    noThemes: true,
    noContextFiles: true,
    systemPrompt: "Use only Dakia tools.",
  });
  await loader.reload();
  const runtime = await ModelRuntime.create({
    credentials: new InMemoryCredentialStore(),
    modelsPath: null,
    refreshOnCreate: false,
  });
  runtime.registerNativeProvider(faux.provider);
  const { session } = await createAgentSession({
    cwd: profile.agentDir,
    modelRuntime: runtime,
    model: faux.getModel(),
    sessionManager: SessionManager.inMemory(profile.agentDir),
    resourceLoader: loader,
    noTools: "builtin",
    customTools: createDakiaTools({
      bridge,
      profile,
      approvals: { interactive: false, confirm: async () => false },
    }),
  });
  try {
    await session.prompt(prompt);
    return session.state.messages;
  } finally {
    session.dispose();
  }
}

describe("fictional Dakia agent acceptance flows", () => {
  it("refreshes and reads the home-insurance thread, then cites the dated message and incomplete result", async () => {
    const bridge = new AcceptanceFixtureBridge();
    const messages = await runScriptedFlow(
      "What's the latest status with my home insurance case?",
      bridge,
      [
        fauxAssistantMessage(
          fauxToolCall("sync_mail", { account_ids: ["home"], limit: 100 }),
        ),
        fauxAssistantMessage(
          fauxToolCall("search_mail", {
            query: "home insurance case",
            account_ids: ["home"],
            limit: 10,
          }),
        ),
        fauxAssistantMessage(
          fauxToolCall("read_thread", {
            account_id: "home",
            message_id: "insurance-1",
          }),
        ),
        fauxAssistantMessage(
          fauxText(
            "The latest update says the assessment is booked for 21 October (message insurance-1, received 2026-10-16). The search reported incomplete results, so there may be other matching mail.",
          ),
        ),
      ],
    );

    expect(bridge.calls).toEqual([
      { method: "mail.sync", params: { account_ids: ["home"], limit: 100 } },
      {
        method: "mail.search",
        params: {
          query: "home insurance case",
          account_ids: ["home"],
          limit: 10,
        },
      },
      {
        method: "mail.thread",
        params: { account_id: "home", message_id: "insurance-1" },
      },
    ]);
    const transcript = JSON.stringify(messages);
    expect(transcript).toContain("message insurance-1");
    expect(transcript).toContain("2026-10-16");
    expect(transcript).toContain("incomplete results");
  });

  it("finds John, reads the selected thread, and saves an account-bound reply draft without sending", async () => {
    const bridge = new AcceptanceFixtureBridge();
    await runScriptedFlow(
      "Draft a reply to John telling him I can't make it today",
      bridge,
      [
        fauxAssistantMessage(
          fauxToolCall("search_mail", {
            query: "John",
            account_ids: ["work"],
            limit: 10,
          }),
        ),
        fauxAssistantMessage(
          fauxToolCall("read_thread", {
            account_id: "work",
            message_id: "john-1",
          }),
        ),
        fauxAssistantMessage(
          fauxToolCall("save_draft", {
            account_id: "work",
            reply_to_message_id: "john-1",
            to: [{ email: "john@example.test", name: "John" }],
            subject: "Re: Plans today",
            body_text: "Hi John, I can't make it today.",
          }),
        ),
        fauxAssistantMessage(
          fauxText("I saved a reply draft to John for review."),
        ),
      ],
    );

    expect(bridge.calls).toEqual([
      {
        method: "mail.search",
        params: { query: "John", account_ids: ["work"], limit: 10 },
      },
      {
        method: "mail.thread",
        params: { account_id: "work", message_id: "john-1" },
      },
      {
        method: "draft.save",
        params: {
          account_id: "work",
          reply_to_message_id: "john-1",
          to: [{ email: "john@example.test", name: "John" }],
          subject: "Re: Plans today",
          body_text: "Hi John, I can't make it today.",
        },
      },
    ]);
    expect(bridge.calls.some((call) => call.method === "draft.send")).toBe(
      false,
    );
  });
});
