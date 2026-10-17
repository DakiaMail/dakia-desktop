import { ModelRuntime } from "@earendil-works/pi-coding-agent";
import { InMemoryCredentialStore } from "@earendil-works/pi-ai";
import { createServer } from "node:http";
import { Type } from "typebox";
import { describe, expect, it } from "vitest";
import {
  credentialWithCustomProviderConfig,
  customProviderConfig,
} from "./providers.js";

describe("custom Anthropic-compatible provider wire protocol", () => {
  it("uses the configured path, vault key, model, and tools through the real runtime", async () => {
    let requestedPath = "";
    let apiKey = "";
    let anthropicVersion = "";
    let requestBody: Record<string, unknown> = {};

    const server = createServer((request, response) => {
      requestedPath = request.url ?? "";
      apiKey = String(request.headers["x-api-key"] ?? "");
      anthropicVersion = String(request.headers["anthropic-version"] ?? "");

      let body = "";
      request.on("data", (chunk) => {
        body += String(chunk);
      });
      request.on("end", () => {
        requestBody = JSON.parse(body) as Record<string, unknown>;
        response.writeHead(200, { "content-type": "text/event-stream" });
        response.write(
          'event: message_start\ndata: {"type":"message_start","message":{"id":"msg_test","type":"message","role":"assistant","model":"mail-claude","content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":4,"output_tokens":1}}}\n\n',
        );
        response.write(
          'event: content_block_start\ndata: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"tool_1","name":"search_mail","input":{}}}\n\n',
        );
        response.write(
          'event: content_block_delta\ndata: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\\"query\\":\\"home insurance\\"}"}}\n\n',
        );
        response.write(
          'event: content_block_stop\ndata: {"type":"content_block_stop","index":0}\n\n',
        );
        response.write(
          'event: message_delta\ndata: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"output_tokens":5}}\n\n',
        );
        response.end('event: message_stop\ndata: {"type":"message_stop"}\n\n');
      });
    });

    await new Promise<void>((resolve) =>
      server.listen(0, "127.0.0.1", resolve),
    );
    try {
      const address = server.address();
      if (!address || typeof address === "string")
        throw new Error("test server did not bind");

      const credentials = new InMemoryCredentialStore();
      const provider = {
        id: "private-anthropic",
        name: "Private Anthropic",
        api: "anthropic-messages" as const,
        baseUrl: `http://127.0.0.1:${address.port}`,
        model: "mail-claude",
      };
      await credentials.modify(provider.id, async () =>
        credentialWithCustomProviderConfig(
          { type: "api_key", key: "synthetic-vault-key" },
          provider,
        ),
      );
      const runtime = await ModelRuntime.create({
        credentials,
        modelsPath: null,
        refreshOnCreate: false,
      });
      runtime.registerProvider(provider.id, customProviderConfig(provider));
      await runtime.setRuntimeApiKey(provider.id, "synthetic-vault-key");

      const result = await runtime.completeSimple(
        runtime.getModel(provider.id, provider.model)!,
        {
          messages: [
            {
              role: "user",
              content: "Find my insurance update",
              timestamp: Date.now(),
            },
          ],
          tools: [
            {
              name: "search_mail",
              description: "Search indexed mail",
              parameters: Type.Object({ query: Type.String() }),
            },
          ],
        },
      );

      expect(new URL(requestedPath, provider.baseUrl).pathname).toBe(
        "/v1/messages",
      );
      expect(apiKey).toBe("synthetic-vault-key");
      expect(anthropicVersion).not.toBe("");
      expect(requestBody.model).toBe("mail-claude");
      expect(requestBody.tools).toEqual(
        expect.arrayContaining([
          expect.objectContaining({
            name: "search_mail",
            input_schema: expect.objectContaining({ type: "object" }),
          }),
        ]),
      );
      expect(result.content).toContainEqual({
        type: "toolCall",
        id: "tool_1",
        name: "search_mail",
        arguments: { query: "home insurance" },
      });
      expect(result.stopReason).toBe("toolUse");
    } finally {
      await new Promise<void>((resolve, reject) =>
        server.close((error) => (error ? reject(error) : resolve())),
      );
    }
  });
});
