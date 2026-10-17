#!/usr/bin/env node
import { createInterface } from "node:readline/promises";
import { stdin as input, stdout as output } from "node:process";
import { rm } from "node:fs/promises";
import { relative, resolve, sep } from "node:path";
import { createAgentSession, DefaultResourceLoader, ModelRuntime, SessionManager } from "@earendil-works/pi-coding-agent";
import type { AuthEvent, AuthInteraction, AuthPrompt, Credential, Model } from "@earendil-works/pi-ai";
import { DakiaBridge, type BridgeTransport } from "./bridge.js";
import { DakiaCredentialStore } from "./auth.js";
import { AgentProfile, type CustomProvider } from "./profile.js";
import { createDakiaTools, type ApprovalGate } from "./tools.js";
import { dakiaSystemPrompt } from "./prompt.js";
import { BUILTIN_PROVIDER_IDS, credentialWithCustomProviderConfig, customProviderConfig, customProviderFromCredential, providerAlias } from "./providers.js";

type Arguments = { command: string; rest: string[]; json: boolean };
const BUILTIN_PROVIDERS = [...BUILTIN_PROVIDER_IDS];

function parseArguments(argv: string[]): Arguments {
  const json = argv.includes("--json");
  const filtered = argv.filter((item) => item !== "--json");
  return { command: filtered[0] ?? "agent", rest: filtered.slice(1), json };
}
function flag(args: string[], name: string): string | undefined { const index = args.indexOf(name); return index >= 0 ? args[index + 1] : undefined; }
function hasFlag(args: string[], name: string): boolean { return args.includes(name); }
function required<T>(value: T | undefined, message: string): T { if (value === undefined || value === "") throw new Error(message); return value; }
function emit(value: unknown, json: boolean): void { output.write(`${json ? JSON.stringify(value) : JSON.stringify(value, null, 2)}\n`); }

async function readLine(question: string, signal?: AbortSignal): Promise<string> {
  const terminal = createInterface({ input, output });
  try { return await terminal.question(question, { signal }); } finally { terminal.close(); }
}
async function readStdin(): Promise<string> {
  const chunks: Buffer[] = [];
  for await (const chunk of input) chunks.push(Buffer.from(chunk));
  return Buffer.concat(chunks).toString("utf8").trim();
}
export function parseModelSpecifier(specifier: string): { provider: string; modelId: string } {
  const slash = specifier.indexOf("/");
  if (slash <= 0 || !specifier.slice(slash + 1)) throw new Error("Model must be written as provider/model.");
  return { provider: providerAlias(specifier.slice(0, slash)), modelId: specifier.slice(slash + 1) };
}

type CustomProviderSnapshot = { provider: CustomProvider; credential: Credential };

async function configuredCustomProviders(credentials: DakiaCredentialStore): Promise<CustomProviderSnapshot[]> {
  const configured: CustomProviderSnapshot[] = [];
  for (const summary of await credentials.list()) {
    // One read gives a matching endpoint/key snapshot. Do not re-read below:
    // a concurrent reconnect could otherwise pair endpoint A with key B.
    const credential = await credentials.read(summary.providerId);
    const provider = customProviderFromCredential(credential);
    if (provider && credential && provider.id === summary.providerId && (!BUILTIN_PROVIDER_IDS.has(provider.id) || provider.id === "ollama")) configured.push({ provider, credential });
  }
  return configured;
}

/** A custom endpoint and its key are one encrypted vault record, never a two-file snapshot. */
async function createRuntime(bridge: BridgeTransport): Promise<{ runtime: ModelRuntime; credentials: DakiaCredentialStore }> {
  const credentials = new DakiaCredentialStore(bridge);
  const runtime = await ModelRuntime.create({ credentials, modelsPath: null, refreshOnCreate: false });
  const snapshots = await configuredCustomProviders(credentials);
  for (const { provider, credential } of snapshots) {
    if (provider.id === "ollama") continue;
    if (credential?.type !== "api_key" || !credential.key) continue;
    runtime.registerProvider(provider.id, customProviderConfig(provider));
    await runtime.setRuntimeApiKey(provider.id, credential.key);
  }
  const ollama = snapshots.find((entry) => entry.provider.id === "ollama")?.provider;
  if (ollama) {
    runtime.registerProvider("ollama", customProviderConfig(ollama));
    await runtime.setRuntimeApiKey("ollama", "local-ollama");
  }
  return { runtime, credentials };
}

async function selectedModel(runtime: ModelRuntime, specifier: string): Promise<Model<any>> {
  const { provider, modelId } = parseModelSpecifier(specifier);
  const catalogModel = runtime.getModel(provider, modelId);
  if (!catalogModel) throw new Error(`Unknown model ${provider}/${modelId}. Run \`dakia provider models ${provider}\` to inspect the current catalog.`);
  if (!(await runtime.getAvailable(provider)).some((model) => model.id === modelId)) {
    throw new Error(`Model ${provider}/${modelId} is not available for the connected account. Choose an available model with \`dakia provider models ${provider}\` and save it using \`dakia provider default provider/model\`.`);
  }
  return catalogModel;
}

async function openSession(profile: AgentProfile, id: string): Promise<SessionManager> {
  if (id.includes("/") || id.includes("\\") || id.includes("..")) throw new Error("Session ID must be an ID shown by `dakia agent sessions list`.");
  const session = (await SessionManager.list(profile.agentDir, profile.sessionsDir)).find((item) => item.id === id);
  if (!session) throw new Error("Saved session was not found.");
  const root = resolve(profile.sessionsDir);
  const candidate = resolve(session.path);
  if (relative(root, candidate).startsWith("..") || !candidate.startsWith(`${root}${sep}`)) throw new Error("Session path is outside the Dakia profile.");
  return SessionManager.open(candidate);
}
function flagValues(args: string[], name: string): string[] {
  const values: string[] = [];
  for (let index = 0; index < args.length - 1; index += 1) if (args[index] === name) values.push(args[index + 1]);
  return values;
}
function positionalPrompt(args: string[]): string {
  const valueFlags = new Set(["--model", "--session", "--approve-action", "--approve-send"]);
  const values: string[] = [];
  for (let index = 0; index < args.length; index += 1) {
    if (valueFlags.has(args[index])) { index += 1; continue; }
    if (args[index] !== "--resume" && args[index] !== "--non-interactive") values.push(args[index]);
  }
  return values.join(" ").trim();
}

async function agentRuntime(profile: AgentProfile, bridge: BridgeTransport, args: string[], json: boolean) {
  const { runtime } = await createRuntime(bridge);
  const modelSpecifier = flag(args, "--model") ?? await profile.defaultModel();
  if (!modelSpecifier) throw new Error("Choose a model with `--model provider/model`, then save it with `dakia provider default provider/model`.");
  const model = await selectedModel(runtime, modelSpecifier);
  const interactive = !json && !hasFlag(args, "--non-interactive") && Boolean(input.isTTY && output.isTTY);
  const approvals: ApprovalGate = {
    interactive,
    approvedHashes: new Set(flagValues(args, "--approve-action").concat(flagValues(args, "--approve-send"))),
    confirm: async (binding) => {
      const action = binding.kind === "delete_messages" ? "permanently delete these exact messages" : "send this exact email";
      output.write(`\nDakia needs approval to ${action}:\n${JSON.stringify(binding, null, 2)}\n`);
      return (await readLine(`Approve ${action}? [y/N] `)).trim().toLowerCase() === "y";
    },
  };
  const loader = new DefaultResourceLoader({
    cwd: profile.agentDir, agentDir: profile.agentDir, noExtensions: true, noSkills: true,
    noPromptTemplates: true, noThemes: true, noContextFiles: true,
    systemPrompt: dakiaSystemPrompt(await profile.readInstructions(), await profile.readMemory()),
  });
  await loader.reload();
  const memorySource = { session_id: "pending", user_prompt: "" };
  const sessionId = flag(args, "--session");
  const sessions = hasFlag(args, "--resume") ? SessionManager.continueRecent(profile.agentDir, profile.sessionsDir) : sessionId ? await openSession(profile, sessionId) : SessionManager.create(profile.agentDir, profile.sessionsDir);
  const { session } = await createAgentSession({
    cwd: profile.agentDir, modelRuntime: runtime, model, sessionManager: sessions, resourceLoader: loader,
    noTools: "builtin", customTools: createDakiaTools({
      bridge, profile, approvals, memorySource: () => memorySource,
      approvalRequired: (approval) => {
        if (json) output.write(`${JSON.stringify({ type: "approval_required", approval })}\n`);
        else output.write(`\nApproval required. Review this exact action and rerun with --approve-action ${approval.hash}:\n${JSON.stringify(approval, null, 2)}\n`);
      },
    }),
  });
  memorySource.session_id = session.sessionManager.getSessionId();
  session.subscribe((event) => {
    if (event.type === "message_update" && event.assistantMessageEvent.type === "text_delta") {
      if (json) output.write(`${JSON.stringify({ type: "text_delta", delta: event.assistantMessageEvent.delta })}\n`); else output.write(event.assistantMessageEvent.delta);
    }
    if (event.type === "tool_execution_start") {
      if (json) output.write(`${JSON.stringify({ type: "tool_start", name: event.toolName })}\n`); else output.write(`\n[Using ${event.toolName}]\n`);
    }
  });
  return { session, memorySource };
}

async function runAgent(profile: AgentProfile, bridge: BridgeTransport, args: string[], json: boolean): Promise<void> {
  const mode = args[0] === "exec" ? "exec" : "interactive";
  const runtimeArgs = mode === "exec" ? args.slice(1) : args;
  const prompt = mode === "exec" ? positionalPrompt(runtimeArgs) : undefined;
  if (mode === "interactive" && !(input.isTTY && output.isTTY)) throw new Error("Interactive Dakia agent requires a terminal. Use `dakia agent exec` for noninteractive use.");
  const { session, memorySource } = await agentRuntime(profile, bridge, runtimeArgs, json);
  try {
    if (mode === "exec") {
      if (!prompt) throw new Error("Use `dakia agent exec \"your request\"`.");
      memorySource.user_prompt = prompt;
      await session.prompt(prompt);
      if (session.state.errorMessage) throw new Error(session.state.errorMessage);
      if (!json) output.write("\n");
      return;
    }
    output.write("Dakia agent. Type /quit to leave. Your email stays controlled by Dakia.\n\n");
    while (true) {
      const line = (await readLine("you> ")).trim();
      if (!line) continue;
      if (line === "/quit" || line === "/exit") break;
      memorySource.user_prompt = line;
      await session.prompt(line);
      if (session.state.errorMessage) throw new Error(session.state.errorMessage);
      output.write("\n");
    }
  } finally { session.dispose(); }
}

function loginInteraction(): AuthInteraction {
  const wholeLogin = new AbortController();
  return {
    signal: wholeLogin.signal,
    notify(event: AuthEvent) {
      if (event.type === "auth_url") output.write(`Open this address in your browser:\n${event.url}\n${event.instructions ?? ""}\n`);
      else if (event.type === "device_code") output.write(`Open ${event.verificationUri} and enter code ${event.userCode}.\n`);
      else output.write(`${event.message}\n`);
    },
    async prompt(prompt: AuthPrompt): Promise<string> {
      const controller = new AbortController();
      const abort = () => controller.abort();
      wholeLogin.signal.addEventListener("abort", abort, { once: true });
      prompt.signal?.addEventListener("abort", abort, { once: true });
      try {
        if (prompt.type === "select") {
          output.write(`${prompt.message}\n${prompt.options.map((option, index) => `${index + 1}. ${option.label}`).join("\n")}\n`);
          const selected = prompt.options[Number(await readLine("Choose: ", controller.signal)) - 1]?.id;
          if (!selected) throw new Error("A listed option must be selected.");
          return selected;
        }
        return readLine(`${prompt.message} `, controller.signal);
      } finally {
        wholeLogin.signal.removeEventListener("abort", abort);
        prompt.signal?.removeEventListener("abort", abort);
      }
    },
  };
}

async function setDefaultAfterConnect(profile: AgentProfile, runtime: ModelRuntime, provider: string, requested: string | undefined, json: boolean): Promise<string | undefined> {
  if (requested) {
    const parsed = parseModelSpecifier(requested.includes("/") ? requested : `${provider}/${requested}`);
    const chosen = `${parsed.provider}/${parsed.modelId}`;
    await selectedModel(runtime, chosen);
    await profile.setDefaultModel(chosen);
    return chosen;
  }
  const available = await runtime.getAvailable(provider);
  if (available.length === 1) {
    const chosen = `${provider}/${available[0].id}`;
    await profile.setDefaultModel(chosen);
    return chosen;
  }
  if (!json && input.isTTY && output.isTTY && available.length > 1) {
    output.write(`Choose a default ${provider} model:\n${available.map((model, index) => `${index + 1}. ${model.id}`).join("\n")}\n`);
    const choice = available[Number(await readLine("Choose: ")) - 1];
    if (!choice) throw new Error("A listed model must be selected.");
    const chosen = `${provider}/${choice.id}`;
    await profile.setDefaultModel(chosen);
    return chosen;
  }
  return undefined;
}

async function writeCustomProvider(bridge: BridgeTransport, provider: CustomProvider, apiKey: string | undefined): Promise<void> {
  const { lease_token } = await bridge.call<{ lease_token: string }>("credential.acquire", { provider_id: provider.id });
  try {
    const prior = await bridge.call<Credential | null>("credential.read", { provider_id: provider.id, lease_token });
    const credential: Credential = provider.id === "ollama" ? { type: "api_key", env: {} } : { type: "api_key", key: apiKey, env: prior?.type === "api_key" ? prior.env : undefined };
    await bridge.call("credential.write", { provider_id: provider.id, credential: credentialWithCustomProviderConfig(credential, provider), lease_token });
  } finally { await bridge.call("credential.release", { lease_token }); }
}

async function providerCommand(profile: AgentProfile, bridge: BridgeTransport, args: string[], json: boolean): Promise<void> {
  const subcommand = args[0] ?? "list";
  if (subcommand === "list") {
    const credentials = new DakiaCredentialStore(bridge);
    emit({ providers: BUILTIN_PROVIDERS, connected: await credentials.list(), custom: (await configuredCustomProviders(credentials)).map((entry) => entry.provider) }, json);
    return;
  }
  if (subcommand === "models") {
    const { runtime } = await createRuntime(bridge);
    const provider = args[1] ? providerAlias(args[1]) : undefined;
    emit({ models: runtime.getModels(provider).map((model) => ({ provider: model.provider, id: model.id, name: model.name })) }, json);
    return;
  }
  if (subcommand === "default") {
    const parsed = parseModelSpecifier(required(args[1], "Use `dakia provider default <provider/model>`."));
    const { runtime } = await createRuntime(bridge);
    const chosen = `${parsed.provider}/${parsed.modelId}`;
    await selectedModel(runtime, chosen);
    await profile.setDefaultModel(chosen);
    emit({ default_model: chosen }, json);
    return;
  }
  if (subcommand === "disconnect") {
    const entered = required(args[1], "Use `dakia provider disconnect <provider>`."), provider = providerAlias(entered);
    await new DakiaCredentialStore(bridge).delete(provider);
    await profile.audit({ type: "provider_disconnected", provider_id: provider });
    emit({ disconnected: entered, provider_id: provider }, json);
    return;
  }
  if (subcommand === "connect") {
    const entered = required(args[1], "Use `dakia provider connect <provider>`."), provider = providerAlias(entered);
    const apiKey = hasFlag(args, "--api-key-stdin") ? await readStdin() : (flag(args, "--api-key-env") ? process.env[flag(args, "--api-key-env")!] : undefined);
    if (entered === "chatgpt" || entered === "claude") {
      if (!(input.isTTY && output.isTTY) || json) throw new Error("Subscription login requires an interactive terminal.");
      const { runtime } = await createRuntime(bridge);
      await runtime.login(provider, "oauth", loginInteraction(), { agentName: "Dakia" });
      const defaultModel = await setDefaultAfterConnect(profile, runtime, provider, flag(args, "--model"), json);
      await profile.audit({ type: "provider_connected", provider_id: provider, credential_type: "subscription" });
      emit({ connected: entered, provider_id: provider, type: "subscription", default_model: defaultModel }, json);
      return;
    }
    if (entered === "custom" || entered === "ollama") {
      const id = entered === "ollama" ? "ollama" : required(flag(args, "--id"), "Custom providers require --id.");
      if (entered === "custom" && BUILTIN_PROVIDER_IDS.has(id)) throw new Error("Custom provider IDs cannot replace a built-in provider.");
      const api = (flag(args, "--api") as CustomProvider["api"] | undefined) ?? "openai-completions";
      if (api !== "openai-responses" && api !== "openai-completions" && api !== "anthropic-messages") throw new Error("Custom provider --api must be openai-responses, openai-completions, or anthropic-messages.");
      const custom: CustomProvider = { id, name: flag(args, "--name") ?? (entered === "ollama" ? "Ollama" : id), api, baseUrl: flag(args, "--base-url") ?? (entered === "ollama" ? "http://127.0.0.1:11434/v1" : required<string>(undefined, "Custom providers require --base-url.")), model: flag(args, "--model") ?? required<string>(undefined, "Providers require --model.") };
      validateProviderUrl(custom.baseUrl);
      if (entered === "custom" && !apiKey) throw new Error("Custom providers require --api-key-stdin or --api-key-env NAME.");
      await writeCustomProvider(bridge, custom, apiKey);
      const { runtime } = await createRuntime(bridge);
      await setDefaultAfterConnect(profile, runtime, id, `${id}/${custom.model}`, json);
      await profile.audit({ type: "provider_connected", provider_id: id, credential_type: entered });
      emit({ connected: id, type: entered, default_model: `${id}/${custom.model}` }, json);
      return;
    }
    if (!BUILTIN_PROVIDER_IDS.has(provider) || provider === "ollama") throw new Error(`Unsupported provider: ${entered}`);
    if (!apiKey) throw new Error("Use --api-key-stdin or --api-key-env NAME to provide an API key without placing it in shell history.");
    const credentials = new DakiaCredentialStore(bridge);
    await credentials.modify(provider, async () => ({ type: "api_key", key: apiKey }));
    const { runtime } = await createRuntime(bridge);
    const defaultModel = await setDefaultAfterConnect(profile, runtime, provider, flag(args, "--model"), json);
    await profile.audit({ type: "provider_connected", provider_id: provider, credential_type: "api_key" });
    emit({ connected: provider, type: "api_key", default_model: defaultModel }, json);
    return;
  }
  if (subcommand === "test") {
    const entered = required(args[1], "Use `dakia provider test <provider> --model <provider/model>`."), provider = providerAlias(entered);
    const requested = required(flag(args, "--model"), "Provider test requires an explicit --model provider/model so Dakia never chooses a catalog entry silently.");
    const parsed = parseModelSpecifier(requested.includes("/") ? requested : `${provider}/${requested}`);
    if (parsed.provider !== provider) throw new Error("The --model provider must match the provider being tested.");
    await runAgent(profile, bridge, ["exec", "Reply with exactly: Dakia provider test passed.", "--non-interactive", "--model", `${parsed.provider}/${parsed.modelId}`], json);
    return;
  }
  throw new Error(`Unknown provider command: ${subcommand}`);
}

async function sessionsCommand(profile: AgentProfile, args: string[], json: boolean): Promise<void> {
  const action = args[0] ?? "list", sessions = await SessionManager.list(profile.agentDir, profile.sessionsDir);
  if (action === "list") { emit({ sessions: sessions.map((session) => ({ id: session.id, name: session.name, first_message: session.firstMessage, modified_at: session.modified.toISOString() })) }, json); return; }
  if (action === "delete") {
    const id = required(args[1], "Use `dakia agent sessions delete <session-id>`."), matches = sessions.filter((session) => session.id === id);
    if (matches.length !== 1) throw new Error("Session ID must match exactly one saved session.");
    await rm(matches[0].path);
    emit({ deleted: id }, json);
    return;
  }
  throw new Error(`Unknown sessions command: ${action}`);
}
async function memoryCommand(profile: AgentProfile, args: string[], json: boolean): Promise<void> {
  const action = args[0] ?? "show";
  if (action === "show") { emit({ body: await profile.readMemory(), learning_enabled: await profile.learningEnabled() }, json); return; }
  if (action === "history") { emit({ history: await profile.memoryHistory() }, json); return; }
  if (action === "set") { const body = hasFlag(args, "--stdin") ? await readStdin() : required(args.slice(1).filter((item) => item !== "--stdin").join(" "), "Use `dakia agent memory set --stdin` or provide text."); emit(await profile.replaceMemory(body), json); return; }
  if (action === "delete") { emit(await profile.deleteMemory(), json); return; }
  if (action === "restore") { emit(await profile.restoreMemory(required(args[1], "Use `dakia agent memory restore <revision>`.")), json); return; }
  if (action === "learning") { const value = required(args[1], "Use `dakia agent memory learning on|off`."); if (value !== "on" && value !== "off") throw new Error("Learning must be on or off."); await profile.setLearningEnabled(value === "on"); emit({ learning_enabled: value === "on" }, json); return; }
  throw new Error(`Unknown memory command: ${action}`);
}
function validateProviderUrl(value: string): void {
  let url: URL;
  try { url = new URL(value); } catch { throw new Error("Provider base URL must be a valid HTTPS URL or local HTTP URL."); }
  if (url.username || url.password) throw new Error("Provider base URL must not contain credentials.");
  const local = url.hostname === "localhost" || url.hostname === "::1" || /^127\./.test(url.hostname);
  if (!(url.protocol === "https:" || (local && url.protocol === "http:"))) throw new Error("Remote provider URLs must use HTTPS.");
}
async function main(): Promise<void> {
  const { command, rest, json } = parseArguments(process.argv.slice(2));
  const profileDir = process.env.DAKIA_PROFILE_DIR;
  if (!profileDir) throw new Error("DAKIA_PROFILE_DIR must be set by Dakia.");
  const bridge = DakiaBridge.fromEnvironment();
  const profile = new AgentProfile(profileDir, async () => {
    const { lease_token } = await bridge.call<{ lease_token: string }>("credential.acquire", { provider_id: "dakia.agent.profile" });
    return async () => { await bridge.call("credential.release", { lease_token }); };
  });
  try {
    await profile.ensure();
    if (command === "agent") {
      if (rest[0] === "sessions") await sessionsCommand(profile, rest.slice(1), json);
      else if (rest[0] === "memory") await memoryCommand(profile, rest.slice(1), json);
      else await runAgent(profile, bridge, rest, json);
    } else if (command === "provider") await providerCommand(profile, bridge, rest, json);
    else throw new Error(`Unknown Dakia agent command: ${command}`);
  } finally { bridge.close(); }
}
main().catch((error: unknown) => { const message = error instanceof Error ? error.message : String(error); output.write(`${JSON.stringify({ error: { code: "agent_error", message } })}\n`); process.exitCode = 1; });
