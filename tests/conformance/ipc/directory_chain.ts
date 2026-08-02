import assert from "node:assert/strict";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { create } from "@bufbuild/protobuf";

import {
  CallerIdentitySchema,
  CapabilityHandleSchema,
  EnvelopeSchema,
  ExchangeRequestSchema,
  SabiRequestContextSchema,
  SchemaIdentitySchema,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import { validateResponseContext } from "../../../sdk/typescript/src/common.ts";
import { ServiceDirectoryClient } from "../../../sdk/typescript/src/service_directory.ts";

function endpoint(label: string): string {
  const unique = `${process.pid}-${Date.now()}-${label}`;
  return process.platform === "win32"
    ? `\\\\.\\pipe\\nlos-directory-${unique}`
    : join(tmpdir(), `nlos-directory-${unique}.sock`);
}

async function startServer(
  directoryEndpoint: string,
  businessEndpoint: string,
): Promise<ChildProcessWithoutNullStreams> {
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
      "nlos-directory-chain",
      "--",
      directoryEndpoint,
      businessEndpoint,
    ],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("directory chain server did not become ready")),
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
      reject(new Error(`directory chain exited early (${code}): ${stderr}`));
    });
    server.once("error", reject);
  });
  return server;
}

const directoryEndpoint = endpoint("bootstrap");
const businessEndpoint = endpoint("business");
const server = await startServer(directoryEndpoint, businessEndpoint);
try {
  const connected = await ServiceDirectoryClient.negotiateAndConnect(
    directoryEndpoint,
    {
      service: "operation",
      schemaName: "nlos.sabi.Envelope",
      major: 1,
      minimumMinor: 1,
    },
    {
      connectTimeoutMs: 2_000,
      readTimeoutMs: 2_000,
      writeTimeoutMs: 2_000,
    },
  );
  assert.equal(connected.binding.endpoint?.address, businessEndpoint);
  assert.equal(connected.binding.candidate?.generation, 7n);

  const response = await connected.client.exchange(
    create(ExchangeRequestSchema, {
      envelope: create(EnvelopeSchema, {
        schema: create(SchemaIdentitySchema, {
          name: "nlos.sabi.Envelope",
          major: 1,
          minor: 1,
        }),
        requestId: new Uint8Array(16).fill(9),
        service: "operation",
        method: "cancel",
        commonContext: {
          case: "requestContext",
          value: create(SabiRequestContextSchema, {
            caller: create(CallerIdentitySchema, {
              principalId: new Uint8Array(16).fill(1),
              applicationId: new Uint8Array(16).fill(2),
              processId: new Uint8Array(16).fill(3),
              processGeneration: 7n,
            }),
            correlationId: new Uint8Array(16).fill(5),
            idempotencyKey: new Uint8Array(16).fill(6),
            deadlineMonotonicNs: 123_456n,
            capabilityHandles: [
              create(CapabilityHandleSchema, { slot: 11n, generation: 2n }),
            ],
          }),
        },
        payload: new Uint8Array([4, 5, 6]),
      }),
    }),
  );
  assert.deepEqual(
    Uint8Array.from(response.envelope?.requestId ?? []),
    new Uint8Array(16).fill(9),
  );
  assert.deepEqual(
    Uint8Array.from(response.envelope?.payload ?? []),
    new Uint8Array([4, 5, 6]),
  );
  assert.ok(response.envelope);
  const responseContext = validateResponseContext(response.envelope, {
    sideEffecting: true,
    longRunning: true,
  });
  assert.deepEqual(
    Uint8Array.from(responseContext.correlationId),
    new Uint8Array(16).fill(5),
  );
  assert.equal(responseContext.operation?.generation, 4n);
  assert.deepEqual(
    Uint8Array.from(responseContext.receipts[0]?.receiptId ?? []),
    new Uint8Array(16).fill(9),
  );
  connected.client.close();
  const code = await new Promise<number | null>((resolve, reject) => {
    server.once("exit", resolve);
    server.once("error", reject);
  });
  assert.equal(code, 0);
} catch (error) {
  server.kill();
  throw error;
}
