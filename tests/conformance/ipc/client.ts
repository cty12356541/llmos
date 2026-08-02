import assert from "node:assert/strict";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { create } from "@bufbuild/protobuf";

import {
  EnvelopeSchema,
  ExchangeRequestSchema,
  SchemaIdentitySchema,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import {
  IpcError,
  LocalRpcClient,
} from "../../../sdk/typescript/src/local_rpc.ts";

const endpoint =
  process.platform === "win32"
    ? `\\\\.\\pipe\\nlos-ipc-ts-${process.pid}-${Date.now()}`
    : join(tmpdir(), `nlos-ipc-ts-${process.pid}-${Date.now()}.sock`);

function request(id: number) {
  return create(ExchangeRequestSchema, {
    envelope: create(EnvelopeSchema, {
      schema: create(SchemaIdentitySchema, {
        name: "nlos.sabi.Envelope",
        major: 1,
        minor: 0,
      }),
      requestId: new Uint8Array(16).fill(id),
      service: "operation",
      method: "get",
      payload: new Uint8Array([1, 2, 3]),
    }),
  });
}

async function startServer(delayMs: number): Promise<ChildProcessWithoutNullStreams> {
  const server = spawn(
    "cargo",
    [
      "run",
      "--quiet",
      "-p",
      "nlos-ipc",
      "--features",
      "conformance-server",
      "--bin",
      "nlos-ipc-echo",
      "--",
      endpoint,
      String(delayMs),
    ],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("Rust IPC conformance server did not become ready")),
      30_000,
    );
    let stdout = "";
    let stderr = "";
    server.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
      if (stdout.includes("READY\n") || stdout.includes("READY\r\n")) {
        clearTimeout(timer);
        resolve();
      }
    });
    server.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    server.once("exit", (code) => {
      clearTimeout(timer);
      reject(
        new Error(
          `Rust IPC conformance server exited early (${code}): ${stderr}`,
        ),
      );
    });
    server.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
  return server;
}

async function waitForSuccess(server: ChildProcessWithoutNullStreams): Promise<void> {
  const code = await new Promise<number | null>((resolve, reject) => {
    server.once("exit", resolve);
    server.once("error", reject);
  });
  assert.equal(code, 0);
}

const server = await startServer(100);
try {
  const client = await LocalRpcClient.connect(endpoint, {
    connectTimeoutMs: 2_000,
    readTimeoutMs: 2_000,
    writeTimeoutMs: 2_000,
  });
  const incompatible = request(6);
  incompatible.envelope!.schema!.major = 2;
  await assert.rejects(client.exchange(incompatible), (error: unknown) => {
    return error instanceof IpcError && error.code === "COMPATIBILITY";
  });
  const first = client.exchange(request(7));
  await assert.rejects(client.exchange(request(8)), (error: unknown) => {
    return error instanceof IpcError && error.code === "BACKPRESSURE";
  });
  const response = await first;
  assert.deepEqual(
    Uint8Array.from(response.envelope?.requestId ?? []),
    new Uint8Array(16).fill(7),
  );
  assert.deepEqual(
    Uint8Array.from(response.envelope?.payload ?? []),
    new Uint8Array([1, 2, 3]),
  );
  client.close();
  await waitForSuccess(server);
} catch (error) {
  server.kill();
  throw error;
}

const unavailable =
  process.platform === "win32"
    ? `\\\\.\\pipe\\nlos-ipc-ts-missing-${process.pid}-${Date.now()}`
    : join(tmpdir(), `nlos-ipc-ts-missing-${process.pid}-${Date.now()}.sock`);
await assert.rejects(
  LocalRpcClient.connect(unavailable, { connectTimeoutMs: 100 }),
  (error: unknown) => {
    return (
      error instanceof IpcError &&
      (error.code === "CONNECT" || error.code === "TIMEOUT")
    );
  },
);
