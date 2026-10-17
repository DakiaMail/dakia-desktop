import type { Credential, CredentialInfo, CredentialStore } from "@earendil-works/pi-ai";
import type { BridgeTransport } from "./bridge.js";

/** Rust owns the encrypted vault and cross-process lease. */
export class DakiaCredentialStore implements CredentialStore {
  constructor(private readonly bridge: BridgeTransport) {}
  async read(providerId: string): Promise<Credential | undefined> {
    return (await this.bridge.call<Credential | null>("credential.read", { provider_id: providerId })) ?? undefined;
  }
  async list(): Promise<readonly CredentialInfo[]> { return this.bridge.call<CredentialInfo[]>("credential.list"); }
  async modify(providerId: string, fn: (current: Credential | undefined) => Promise<Credential | undefined>): Promise<Credential | undefined> {
    const { lease_token } = await this.bridge.call<{ lease_token: string }>("credential.acquire", { provider_id: providerId });
    try {
      // Read after acquiring the Rust lease. OAuth refresh-token rotation must
      // never start from a stale in-memory copy in another process.
      const current = (await this.bridge.call<Credential | null>("credential.read", { provider_id: providerId, lease_token })) ?? undefined;
      const next = await fn(current);
      // CredentialStore deliberately uses undefined as "leave unchanged".
      // Deletion is only performed through delete(), which has its own lease.
      if (next) await this.bridge.call("credential.write", { provider_id: providerId, credential: next, lease_token });
      return next ?? current;
    } finally { await this.bridge.call("credential.release", { lease_token }); }
  }
  async delete(providerId: string): Promise<void> {
    const { lease_token } = await this.bridge.call<{ lease_token: string }>("credential.acquire", { provider_id: providerId });
    try { await this.bridge.call("credential.delete", { provider_id: providerId, lease_token }); }
    finally { await this.bridge.call("credential.release", { lease_token }); }
  }
}
