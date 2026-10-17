import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";

export type JsonObject = Record<string, unknown>;

export class BridgeError extends Error {
  constructor(readonly code: string, message: string) {
    super(message);
    this.name = "BridgeError";
  }
}

export interface BridgeTransport {
  call<T>(method: string, params?: JsonObject): Promise<T>;
  close(): void;
}

type BridgeResponse = {
  id: string;
  result?: unknown;
  error?: { code?: string; message?: string };
};

/**
 * The bridge is deliberately the only process that touches Dakia mail, context,
 * or encrypted credentials. JSONL makes the boundary easy to audit and keeps
 * model tools independent of desktop implementation details.
 */
export class DakiaBridge implements BridgeTransport {
  private readonly child: ChildProcessWithoutNullStreams;
  private readonly pending = new Map<
    string,
    { resolve: (value: unknown) => void; reject: (error: Error) => void }
  >();
  private buffer = "";
  private sequence = 0;
  private closed = false;

  static fromEnvironment(environment: NodeJS.ProcessEnv = process.env): DakiaBridge {
    const executable = environment.DAKIA_CLI_PATH;
    const profileDir = environment.DAKIA_PROFILE_DIR;
    if (!executable || !profileDir) {
      throw new Error("DAKIA_CLI_PATH and DAKIA_PROFILE_DIR must be set by Dakia");
    }
    return new DakiaBridge(executable, profileDir, environment);
  }

  constructor(executable: string, profileDir: string, environment: NodeJS.ProcessEnv = process.env) {
    this.child = spawn(executable, ["--data-dir", profileDir, "agent-bridge"], {
      stdio: ["pipe", "pipe", "pipe"],
      env: environment,
    });
    this.child.stdout.setEncoding("utf8");
    this.child.stdout.on("data", (chunk: string) => this.onData(chunk));
    this.child.stdout.once("error", (error) => this.failAll(error));
    // Rust diagnostics must not fill a pipe and deadlock JSONL requests. They
    // are deliberately not relayed because bridge stderr may contain provider detail.
    this.child.stderr.on("data", () => undefined);
    this.child.stderr.once("error", (error) => this.failAll(error));
    this.child.stdin.once("error", (error) => this.failAll(error));
    this.child.once("error", (error) => this.failAll(error));
    this.child.once("exit", (code, signal) => {
      this.failAll(new Error(`Dakia bridge exited (${code ?? "unknown"}${signal ? `, ${signal}` : ""})`));
    });
  }

  async call<T>(method: string, params: JsonObject = {}): Promise<T> {
    if (this.closed) throw new Error("Dakia bridge is closed");
    const id = `agent-${++this.sequence}`;
    const line = `${JSON.stringify({ id, method, params })}\n`;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: (value) => resolve(value as T), reject });
      this.child.stdin.write(line, "utf8", (error) => {
        if (!error) return;
        this.pending.delete(id);
        reject(error);
      });
    });
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.child.stdin.end();
    this.child.kill();
    this.failAll(new Error("Dakia bridge closed"));
  }

  private onData(chunk: string): void {
    this.buffer += chunk;
    let newline = this.buffer.indexOf("\n");
    while (newline >= 0) {
      const line = this.buffer.slice(0, newline).trim();
      this.buffer = this.buffer.slice(newline + 1);
      newline = this.buffer.indexOf("\n");
      if (!line) continue;
      let response: BridgeResponse;
      try {
        response = JSON.parse(line) as BridgeResponse;
      } catch {
        this.failAll(new Error("Dakia bridge returned invalid JSONL"));
        return;
      }
      const pending = this.pending.get(response.id);
      if (!pending) continue;
      this.pending.delete(response.id);
      if (response.error) {
        pending.reject(new BridgeError(response.error.code ?? "bridge_error", response.error.message ?? "Bridge request failed"));
      } else {
        pending.resolve(response.result);
      }
    }
  }

  private failAll(error: Error): void {
    for (const pending of this.pending.values()) pending.reject(error);
    this.pending.clear();
  }
}
