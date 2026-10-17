import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { AgentProfile } from "./profile.js";

const directories: string[] = [];
afterEach(async () => {
  await Promise.all(directories.splice(0).map((directory) => rm(directory, { recursive: true, force: true })));
});

async function profile(): Promise<AgentProfile> {
  const directory = await mkdtemp(join(tmpdir(), "dakia-agent-profile-"));
  directories.push(directory);
  const result = new AgentProfile(directory);
  await result.ensure();
  return result;
}

describe("AgentProfile memory", () => {
  it("keeps concurrent learned preferences and records restorable revisions", async () => {
    const value = await profile();
    await Promise.all(Array.from({ length: 12 }, (_, index) => value.appendMemory(`pref-${index}`)));
    const body = await value.readMemory();
    for (let index = 0; index < 12; index += 1) expect(body).toContain(`pref-${index}`);
    const history = await value.memoryHistory();
    expect(history).toHaveLength(12);
    await value.restoreMemory(history[0].id);
    expect(await value.readMemory()).toBe(history[0].body);
  });

  it("does not learn while learning is disabled", async () => {
    const value = await profile();
    await value.setLearningEnabled(false);
    expect(await value.appendMemory("do not store this")).toEqual({ saved: false });
    expect(await value.readMemory()).not.toContain("do not store this");
  });

  it("writes custom providers atomically as a replacement", async () => {
    const value = await profile();
    await value.saveCustomProviders([{ id: "local", name: "Local", api: "openai-completions", baseUrl: "http://127.0.0.1:11434/v1", model: "qwen" }]);
    expect(await value.customProviders()).toEqual([{ id: "local", name: "Local", api: "openai-completions", baseUrl: "http://127.0.0.1:11434/v1", model: "qwen" }]);
  });

  it("recovers an abandoned lock from a dead process", async () => {
    const value = await profile();
    await writeFile(join(value.agentDir, "MEMORY.md.lock"), JSON.stringify({ pid: 99999999, created_at: 0 }));
    await expect(value.appendMemory("recovered")).resolves.toMatchObject({ saved: true });
  });

  it("lets concurrent reopeners claim one stale lock without losing either update", async () => {
    const value = await profile();
    await writeFile(join(value.agentDir, "MEMORY.md.lock"), JSON.stringify({ pid: 99999999, nonce: "dead-owner", created_at: 0 }));
    const other = new AgentProfile(value.root);
    await Promise.all([value.appendMemory("first contender"), other.appendMemory("second contender")]);
    const body = await value.readMemory();
    expect(body).toContain("first contender");
    expect(body).toContain("second contender");
  });

  it("uses an injected cross-process lock service for profile mutations", async () => {
    const directory = await mkdtemp(join(tmpdir(), "dakia-agent-profile-service-lock-"));
    directories.push(directory);
    let active = false;
    const waiters: Array<() => void> = [];
    const lock = async () => {
      if (active) await new Promise<void>((resolve) => waiters.push(resolve));
      active = true;
      return async () => { active = false; waiters.shift()?.(); };
    };
    const first = new AgentProfile(directory, lock), second = new AgentProfile(directory, lock);
    await Promise.all([first.ensure(), second.ensure()]);
    await Promise.all([first.appendMemory("service first"), second.appendMemory("service second")]);
    const body = await first.readMemory();
    expect(body).toContain("service first");
    expect(body).toContain("service second");
  });

  it("recovers a committed memory revision when a crash leaves MEMORY.md on the prior mirror", async () => {
    const value = await profile();
    await value.appendMemory("old preference");
    const oldMirror = await value.readMemory();
    await value.appendMemory("new preference");
    // Simulate a process dying after memory-state.json committed but before its
    // MEMORY.md mirror rename. Reopening must keep both body and provenance.
    const statePath = join(value.agentDir, "memory-state.json");
    const state = JSON.parse(await readFile(statePath, "utf8")) as Record<string, unknown>;
    await writeFile(statePath, `${JSON.stringify({ ...state, pending_mirror: true })}\n`);
    await writeFile(join(value.agentDir, "MEMORY.md"), oldMirror);
    const reopened = new AgentProfile(value.root);
    await reopened.ensure();
    expect(await reopened.readMemory()).toContain("new preference");
    expect((await reopened.memoryHistory()).at(-1)?.body).toContain("new preference");
  });

  it("imports a user edit to MEMORY.md instead of overwriting it with a journal snapshot", async () => {
    const value = await profile();
    await value.appendMemory("automatic preference");
    await writeFile(join(value.agentDir, "MEMORY.md"), "# Dakia agent memory\n\n- user edited this\n");
    await value.ensure();
    expect(await value.readMemory()).toContain("user edited this");
    expect((await value.memoryHistory()).at(-1)?.action).toBe("manual_external");
  });

  it("keeps a completed-journal manual revert to an earlier body", async () => {
    const value = await profile();
    await value.appendMemory("first");
    const first = await value.readMemory();
    await value.appendMemory("second");
    await writeFile(join(value.agentDir, "MEMORY.md"), first);
    await value.ensure();
    expect(await value.readMemory()).toBe(first);
    expect((await value.memoryHistory()).at(-1)?.action).toBe("manual_external");
  });
});
