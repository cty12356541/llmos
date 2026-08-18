import assert from "node:assert/strict";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  create,
  fromBinary,
  toBinary,
} from "@bufbuild/protobuf";
import {
  BarrierObservationEvidenceSchema,
  BarrierObservationRecordSchema,
  BarrierObservationSignatureSchema,
  BarrierObservationTargetSchema,
  SubmitBarrierObservationRequestSchema,
} from "../../../gen/typescript/nlos/sabi/v1/takeover_control_pb.ts";
import {
  CallerIdentitySchema,
  CapabilityHandleSchema,
  EnvelopeSchema,
  ExchangeRequestSchema,
  ExchangeResponseSchema,
  SabiRequestContextSchema,
  SchemaIdentitySchema,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import {
  IpcError,
  LocalRpcClient,
} from "../../../sdk/typescript/src/local_rpc.ts";
import { validateResponseContext } from "../../../sdk/typescript/src/common.ts";

type Fixture = Record<string, string>;

function endpoint(label: string): string {
  const unique = `${process.pid}-${Date.now()}-${label}`;
  return process.platform === "win32"
    ? `\\\\.\\pipe\\nlos-takeover-${unique}`
    : join(tmpdir(), `nlos-takeover-${unique}.sock`);
}

function parseFixture(line: string): Fixture {
  assert.match(line, /^FIXTURE /);
  return Object.fromEntries(
    line
      .slice("FIXTURE ".length)
      .trim()
      .split(/\s+/)
      .map((field) => {
        const separator = field.indexOf("=");
        assert.ok(separator > 0, `invalid fixture field ${field}`);
        return [field.slice(0, separator), field.slice(separator + 1)];
      }),
  );
}

function bytes(fixture: Fixture, key: string): Uint8Array {
  const value = fixture[key];
  assert.ok(value, `missing fixture field ${key}`);
  assert.equal(value.length % 2, 0, `${key} must be hex`);
  return Uint8Array.from(Buffer.from(value, "hex"));
}

async function startServer(
  socket: string,
  authorityPath: string,
  identityPath: string,
): Promise<{ server: ChildProcessWithoutNullStreams; fixture: Fixture }> {
  const server = spawn(
    "cargo",
    [
      "run",
      "--quiet",
      "-p",
      "nlos-takeover-control",
      "--features",
      "conformance-server",
      "--bin",
      "takeover-control-conformance",
      "--",
      socket,
      authorityPath,
      identityPath,
    ],
    { cwd: process.cwd(), stdio: ["pipe", "pipe", "pipe"] },
  );
  const result = await new Promise<Fixture>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("TakeoverControl conformance server did not become ready")),
      60_000,
    );
    let stdout = "";
    let stderr = "";
    const inspect = (): void => {
      const match = stdout.match(/^FIXTURE .*$/m);
      if (stdout.includes("READY\n") && match) {
        clearTimeout(timer);
        resolve(parseFixture(match[0]));
      }
    };
    server.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
      inspect();
    });
    server.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    server.once("exit", (code) => {
      clearTimeout(timer);
      reject(new Error(`TakeoverControl server exited before ready (${code}): ${stderr}`));
    });
    server.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
  return { server, fixture: result };
}

function submitRequest(fixture: Fixture) {
  const payload = toBinary(
    SubmitBarrierObservationRequestSchema,
    create(SubmitBarrierObservationRequestSchema, {
      schema: create(SchemaIdentitySchema, {
        name: "nlos.sabi.TakeoverControl",
        major: 1,
        minor: 0,
      }),
      target: create(BarrierObservationTargetSchema, {
        takeoverReceiptId: bytes(fixture, "takeover_receipt_id"),
        participantType: Number(fixture.participant_type),
        participantId: bytes(fixture, "participant_id"),
        participantGeneration: BigInt(fixture.participant_generation),
        admissionReceiptId: bytes(fixture, "admission_receipt_id"),
      }),
      evidence: create(BarrierObservationEvidenceSchema, {
        remoteReceiptId: bytes(fixture, "remote_receipt_id"),
        barrierDigest: bytes(fixture, "barrier_digest"),
        observedAtMs: BigInt(fixture.observed_at_ms),
      }),
      signature: create(BarrierObservationSignatureSchema, {
        signerPrincipalId: bytes(fixture, "signer_principal_id"),
        signerControlDomainId: bytes(fixture, "signer_control_domain_id"),
        signerKeyId: bytes(fixture, "signer_key_id"),
        signature: bytes(fixture, "signature"),
      }),
    }),
  );
  return create(ExchangeRequestSchema, {
    envelope: create(EnvelopeSchema, {
      schema: create(SchemaIdentitySchema, {
        name: "nlos.sabi.Envelope",
        major: 1,
        minor: 1,
      }),
      requestId: new Uint8Array(16).fill(0xd1),
      service: "takeover_control",
      method: "submit_barrier_observation",
      commonContext: {
        case: "requestContext",
        value: create(SabiRequestContextSchema, {
          caller: create(CallerIdentitySchema, {
            principalId: new Uint8Array(16).fill(0xd4),
            applicationId: new Uint8Array(16).fill(0xd5),
            processId: new Uint8Array(16).fill(0xd6),
            processGeneration: 1n,
          }),
          correlationId: new Uint8Array(16).fill(0xd2),
          idempotencyKey: new Uint8Array(16).fill(0xd3),
          deadlineMonotonicNs: 1_000n,
          capabilityHandles: [
            create(CapabilityHandleSchema, {
              slot: 5n,
              generation: 1n,
            }),
          ],
        }),
      },
      payload,
    }),
  });
}

function assertSuccess(response: Awaited<ReturnType<LocalRpcClient["exchange"]>>, fixture: Fixture): void {
  assert.ok(response.envelope);
  const context = validateResponseContext(response.envelope, {
    sideEffecting: true,
    longRunning: false,
  });
  assert.equal(context.failure, undefined);
  assert.equal(context.receipts.length, 1);
  const record = fromBinary(BarrierObservationRecordSchema, response.envelope.payload);
  assert.equal(record.signed, true);
  assert.deepEqual(Uint8Array.from(record.participantId), bytes(fixture, "participant_id"));
  assert.deepEqual(Uint8Array.from(record.barrierDigest), bytes(fixture, "barrier_digest"));
  assert.equal(record.observedAtMs, BigInt(fixture.observed_at_ms));
  assert.deepEqual(
    Uint8Array.from(record.signerPrincipalId),
    bytes(fixture, "signer_principal_id"),
  );
  assert.deepEqual(Uint8Array.from(record.signerKeyId), bytes(fixture, "signer_key_id"));
  assert.equal(record.signerKeyGeneration, 1n);
  assert.deepEqual(
    Uint8Array.from(context.receipts[0]?.receiptId ?? []),
    Uint8Array.from(record.receiptId),
  );
}

const socket = endpoint("ts");
const authorityPath = join(tmpdir(), `nlos-takeover-${process.pid}-${Date.now()}.sqlite3`);
const identityPath = join(tmpdir(), `nlos-takeover-${process.pid}-${Date.now()}-identity`);
let server: ChildProcessWithoutNullStreams | undefined;
try {
  const started = await startServer(socket, authorityPath, identityPath);
  server = started.server;
  const request = submitRequest(started.fixture);
  const first = await LocalRpcClient.connect(socket, {
    connectTimeoutMs: 2_000,
    readTimeoutMs: 2_000,
    writeTimeoutMs: 2_000,
  });
  const response = await first.exchange(request);
  assertSuccess(response, started.fixture);
  first.close();

  const replayClient = await LocalRpcClient.connect(socket, {
    connectTimeoutMs: 2_000,
    readTimeoutMs: 2_000,
    writeTimeoutMs: 2_000,
  });
  const replay = await replayClient.exchange(request);
  assertSuccess(replay, started.fixture);
  assert.deepEqual(
    toBinary(ExchangeResponseSchema, response),
    toBinary(ExchangeResponseSchema, replay),
  );
  replayClient.close();

  const exitCode = await new Promise<number | null>((resolve, reject) => {
    server?.once("exit", resolve);
    server?.once("error", reject);
  });
  assert.equal(exitCode, 0);
} catch (error) {
  server?.kill();
  throw error;
} finally {
  await Promise.all([
    rm(socket, { force: true }),
    rm(authorityPath, { force: true }),
    rm(`${authorityPath}-wal`, { force: true }),
    rm(`${authorityPath}-shm`, { force: true }),
    rm(identityPath, { recursive: true, force: true }),
  ]);
}
