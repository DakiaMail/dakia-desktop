import { ModelRuntime } from "@earendil-works/pi-coding-agent";
import { InMemoryCredentialStore } from "@earendil-works/pi-ai";
import { createServer } from "node:http";
import { describe, expect, it } from "vitest";
import { BUILTIN_PROVIDER_IDS, credentialWithCustomProviderConfig, customProviderConfig, customProviderFromCredential, providerAlias } from "./providers.js";

describe("custom Pi provider registration", () => {
  it("registers a vault-backed custom OpenAI-compatible provider with the current Pi runtime", async () => {
    const credentials = new InMemoryCredentialStore();
    await credentials.modify("local", async () => credentialWithCustomProviderConfig({ type: "api_key", key: "vault-key" }, { id: "local", name: "Local", api: "openai-completions", baseUrl: "http://localhost:1234/v1", model: "qwen" }));
    const runtime = await ModelRuntime.create({ credentials, modelsPath: null, refreshOnCreate: false });
    runtime.registerProvider("local", customProviderConfig(customProviderFromCredential(await credentials.read("local"))!));
    await runtime.setRuntimeApiKey("local", "vault-key");
    expect(runtime.getModel("local", "qwen")?.id).toBe("qwen");
    expect((await runtime.getAvailable("local")).map((model) => model.id)).toContain("qwen");
  });

  it("registers local Ollama with an explicit local runtime key", async () => {
    const runtime = await ModelRuntime.create({ credentials: new InMemoryCredentialStore(), modelsPath: null, refreshOnCreate: false });
    runtime.registerProvider("ollama", customProviderConfig({ id: "ollama", name: "Ollama", api: "openai-completions", baseUrl: "http://127.0.0.1:11434/v1", model: "qwen" }));
    await runtime.setRuntimeApiKey("ollama", "local-ollama");
    expect(runtime.getModel("ollama", "qwen")?.id).toBe("qwen");
  });

  it("sends a custom OpenAI-compatible request with the vault key over the Pi runtime", async () => {
    let authorization = "";
    let requestedModel = "";
    const server = createServer((request, response) => {
      authorization = request.headers.authorization ?? "";
      let body = "";
      request.on("data", (chunk) => { body += String(chunk); });
      request.on("end", () => {
        requestedModel = (JSON.parse(body) as { model: string }).model;
        response.writeHead(200, { "content-type": "text/event-stream" });
        response.write('data: {"id":"local-1","choices":[{"delta":{"content":"ok"},"finish_reason":null}]}\n\n');
        response.write('data: {"id":"local-1","choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}\n\n');
        response.end("data: [DONE]\n\n");
      });
    });
    await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
    try {
      const address = server.address();
      if (!address || typeof address === "string") throw new Error("test server did not bind");
      const credentials = new InMemoryCredentialStore();
      const provider = { id: "local", name: "Local", api: "openai-completions" as const, baseUrl: `http://127.0.0.1:${address.port}/v1`, model: "mail-model" };
      await credentials.modify("local", async () => credentialWithCustomProviderConfig({ type: "api_key", key: "vault-key" }, provider));
      const runtime = await ModelRuntime.create({ credentials, modelsPath: null, refreshOnCreate: false });
      runtime.registerProvider("local", customProviderConfig(provider));
      await runtime.setRuntimeApiKey("local", "vault-key");
      const result = await runtime.completeSimple(runtime.getModel("local", "mail-model")!, { messages: [{ role: "user", content: "hello", timestamp: Date.now() }] });
      expect(result.content).toContainEqual({ type: "text", text: "ok" });
      expect(authorization).toBe("Bearer vault-key");
      expect(requestedModel).toBe("mail-model");
    } finally { await new Promise<void>((resolve, reject) => server.close((error) => error ? reject(error) : resolve())); }
  });

  it("keeps subscription aliases separate from custom provider IDs", () => {
    expect(providerAlias("chatgpt")).toBe("openai-codex");
    expect(providerAlias("claude")).toBe("anthropic");
    expect(BUILTIN_PROVIDER_IDS.has("openai")).toBe(true);
  });

  it("retains slashes in a custom model ID", () => {
    const config = customProviderConfig({ id: "local", name: "Local", api: "openai-completions", baseUrl: "http://localhost:1234/v1", model: "org/model" });
    expect(config.models?.[0]?.id).toBe("org/model");
  });
});
