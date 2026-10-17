import { describe, expect, it } from "vitest";
import type { BridgeTransport } from "./bridge.js";
import { DakiaCredentialStore } from "./auth.js";

class VaultBridge implements BridgeTransport {
  readonly data: Record<string, unknown> = { anthropic: { type: "oauth", access: "old", refresh: "r", expires: 0 } };
  readonly writes: string[] = [];
  async call<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    if (method === "credential.list") return Object.entries(this.data).map(([providerId, credential]) => ({ providerId, type: (credential as { type: string }).type })) as T;
    if (method === "credential.read") return (this.data[params.provider_id as string] ?? null) as T;
    if (method === "credential.acquire") return { lease_token: "lease" } as T;
    if (method === "credential.write") { this.writes.push(params.provider_id as string); this.data[params.provider_id as string] = params.credential; return {} as T; }
    if (method === "credential.delete" || method === "credential.release") return {} as T;
    throw new Error(method);
  }
  close(): void {}
}

describe("DakiaCredentialStore", () => {
  it("re-reads under the lease and preserves a concurrently added credential", async () => {
    const bridge = new VaultBridge(), store = new DakiaCredentialStore(bridge);
    bridge.data.openai = { type: "oauth", access: "fresh-other-agent", refresh: "other", expires: 99 };
    await store.modify("anthropic", async (current) => ({ ...current!, access: "rotated", refresh: "r2", expires: 100 }));
    expect(bridge.data.openai).toMatchObject({ access: "fresh-other-agent" });
    expect(bridge.data.anthropic).toMatchObject({ access: "rotated" });
    expect(bridge.writes).toEqual(["anthropic"]);
  });

  it("returns the current credential when the Pi callback intentionally makes no change", async () => {
    const bridge = new VaultBridge(), store = new DakiaCredentialStore(bridge);
    const result = await store.modify("anthropic", async () => undefined);
    expect(result).toMatchObject({ access: "old" });
    expect(bridge.writes).toEqual([]);
  });
});
