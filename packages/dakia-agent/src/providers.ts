import type { ProviderConfig } from "@earendil-works/pi-coding-agent";
import type { Credential, ProviderEnv } from "@earendil-works/pi-ai";
import type { CustomProvider } from "./profile.js";

export const BUILTIN_PROVIDER_IDS = new Set([
  "anthropic", "openai", "openai-codex", "google", "openrouter", "groq", "mistral",
  "deepseek", "xai", "cerebras", "ollama", "azure-openai", "bedrock", "vertex",
  "chatgpt", "claude",
]);

export function providerAlias(providerId: string): string {
  return providerId === "chatgpt" ? "openai-codex" : providerId === "claude" ? "anthropic" : providerId;
}

/**
 * Callers register this only after confirming an encrypted-vault credential.
 * The actual key is supplied as a ModelRuntime runtime key, never as a
 * placeholder that could accidentally become a network authorization value.
 */
export function customProviderConfig(provider: CustomProvider): ProviderConfig {
  return {
    name: provider.name,
    api: provider.api,
    baseUrl: provider.baseUrl,
    authHeader: true,
    models: [{
      id: provider.model,
      name: provider.model,
      api: provider.api,
      reasoning: false,
      input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      contextWindow: provider.contextWindow ?? 128000,
      maxTokens: provider.maxTokens ?? 8192,
    }],
  };
}

const CUSTOM_PROVIDER_CONFIG_KEY = "DAKIA_CUSTOM_PROVIDER_CONFIG_V1";

/**
 * Keep the endpoint and its credential in the encrypted vault as one record.
 * providers.json is only legacy display metadata and is never consulted when
 * constructing a runtime request.
 */
export function credentialWithCustomProviderConfig(credential: Credential, provider: CustomProvider): Credential {
  const existing = credential.type === "api_key" ? credential.env : undefined;
  return {
    ...credential,
    env: {
      ...existing,
      [CUSTOM_PROVIDER_CONFIG_KEY]: JSON.stringify(provider),
    } as ProviderEnv,
  };
}

export function customProviderFromCredential(credential: Credential | undefined): CustomProvider | undefined {
  const encoded = credential?.type === "api_key" ? credential.env?.[CUSTOM_PROVIDER_CONFIG_KEY] : undefined;
  if (typeof encoded !== "string") return undefined;
  try {
    const value = JSON.parse(encoded) as Partial<CustomProvider>;
    if (
      typeof value.id !== "string" || typeof value.name !== "string" || typeof value.baseUrl !== "string" ||
      typeof value.model !== "string" ||
      (value.api !== "openai-responses" && value.api !== "openai-completions" && value.api !== "anthropic-messages")
    ) return undefined;
    return {
      id: value.id,
      name: value.name,
      api: value.api,
      baseUrl: value.baseUrl,
      model: value.model,
      contextWindow: typeof value.contextWindow === "number" ? value.contextWindow : undefined,
      maxTokens: typeof value.maxTokens === "number" ? value.maxTokens : undefined,
    };
  } catch { return undefined; }
}
