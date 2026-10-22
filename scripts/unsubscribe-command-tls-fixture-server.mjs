import { readFile, writeFile } from "node:fs/promises";
import { createServer } from "node:https";
import { fileURLToPath } from "node:url";

export function parseArguments(args) {
  const values = new Map();
  for (let index = 0; index < args.length; index += 2) {
    const key = args[index];
    const value = args[index + 1];
    if (!key?.startsWith("--") || !value || values.has(key)) {
      throw new Error("invalid fictional unsubscribe TLS server arguments");
    }
    values.set(key, value);
  }
  const required = [
    "--good-cert",
    "--good-key",
    "--wrong-cert",
    "--wrong-key",
    "--port-file",
    "--receipt",
  ];
  if (
    values.size !== required.length ||
    required.some((key) => !values.has(key))
  ) {
    throw new Error("missing fictional unsubscribe TLS server argument");
  }
  return Object.fromEntries(values);
}

async function startServer(name, certPath, keyPath, receipt) {
  const server = createServer(
    {
      cert: await readFile(certPath),
      key: await readFile(keyPath),
      minVersion: "TLSv1.2",
    },
    async (request, response) => {
      const chunks = [];
      let length = 0;
      for await (const chunk of request) {
        length += chunk.length;
        if (length > 1024) {
          request.destroy();
          return;
        }
        chunks.push(chunk);
      }
      receipt.requests.push({
        server: name,
        method: request.method,
        path: request.url,
        contentType: request.headers["content-type"] ?? null,
        authorization: request.headers.authorization ?? null,
        cookie: request.headers.cookie ?? null,
        origin: request.headers.origin ?? null,
        referer: request.headers.referer ?? null,
        body: Buffer.concat(chunks).toString("utf8"),
      });
      await writeFile(receipt.path, `${JSON.stringify(receipt, null, 2)}\n`, {
        mode: 0o600,
      });
      response.writeHead(204, { "content-length": "0" });
      response.end();
    },
  );
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  if (!address || typeof address === "string")
    throw new Error("TLS listener did not bind TCP");
  return { server, port: address.port };
}

export async function startFixture(options) {
  const receipt = {
    path: options["--receipt"],
    schemaVersion: 1,
    requests: [],
  };
  await writeFile(receipt.path, `${JSON.stringify(receipt, null, 2)}\n`, {
    mode: 0o600,
  });
  const good = await startServer(
    "good",
    options["--good-cert"],
    options["--good-key"],
    receipt,
  );
  const wrong = await startServer(
    "wrong-host",
    options["--wrong-cert"],
    options["--wrong-key"],
    receipt,
  );
  await writeFile(
    options["--port-file"],
    `${JSON.stringify({ schemaVersion: 1, good: good.port, wrongHost: wrong.port })}\n`,
    { mode: 0o600 },
  );
  return async () => {
    await Promise.all(
      [good.server, wrong.server].map(
        (server) =>
          new Promise((resolve, reject) => {
            server.close((error) => (error ? reject(error) : resolve()));
          }),
      ),
    );
  };
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const options = parseArguments(process.argv.slice(2));
  const stop = await startFixture(options);
  const close = async () => {
    await stop();
    process.exit(0);
  };
  process.once("SIGTERM", close);
  process.once("SIGINT", close);
}
