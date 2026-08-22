/**
 * External-timing process-crash conformance for TakeoverControl.
 *
 * The supervisor is this client process, not the feature-gated server. It
 * deliberately does not use any server crash hook: a short OS-CSPRNG delay
 * is sampled after the request is submitted, the child is terminated, and the
 * same authority/identity plus idempotency key is used after restart.
 *
 * This proves process-level external termination and replay convergence only.
 * It does not claim a particular commit phase, power loss, time-window
 * anti-replay, or production Capability/peer-attestation enforcement.
 */

import assert from "node:assert/strict";
import { randomInt } from "node:crypto";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import {
  BarrierObservationRecordSchema,
  BarrierObservationTargetSchema,
  BarrierObservationEvidenceSchema,
  BarrierObservationSignatureSchema,
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
import { LocalRpcClient } from "../../../sdk/typescript/src/local_rpc.ts";
import { validateResponseContext } from "../../../sdk/typescript/src/common.ts";

type Fixture = Record<string, string>;
type StartedServer = {
  server: ChildProcessWithoutNullStreams;
  fixture: Fixture;
};

const MAX_EXTERNAL_DELAY_MS = 64;
const HOOK_ENVIRONMENT_KEYS = [
  "NLOS_TAKEOVER_CONTROL_HOLD_BEFORE_COMMIT",
  "NLOS_TAKEOVER_CONTROL_HOLD_AFTER_COMMIT",
  "NLOS_TAKEOVER_CONTROL_TRUNCATE_WAL_AFTER_COMMIT",
  "NLOS_TAKEOVER_CONTROL_RANDOM_CRASH_PHASE",
  "NLOS_TAKEOVER_CONTROL_RANDOM_CRASH_SEED",
  "NLOS_TAKEOVER_CONTROL_CONNECTIONS",
  "NLOS_TAKEOVER_CONTROL_ROUNDS",
] as const;

function parseTrials(): number {
  if (process.argv.length === 2) return 8;
  assert.equal(process.argv.length, 4, "usage: --trials N");
  assert.equal(process.argv[2], "--trials", "usage: --trials N");
  const trials = Number(process.argv[3]);
  assert.ok(Number.isSafeInteger(trials) && trials > 0 && trials <= 32);
  return trials;
}

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

function fixtureBytes(fixture: Fixture, key: string): Uint8Array {
  const value = fixture[key];
  assert.ok(value, `missing fixture field ${key}`);
  assert.equal(value.length % 2, 0, `${key} must be hex`);
  return Uint8Array.from(Buffer.from(value, "hex"));
}

function waitForExit(
  server: ChildProcessWithoutNullStreams,
): Promise<{ code: number | null; signal: NodeJS.Signals | null }> {
  if (server.exitCode !== null || server.signalCode !== null) {
    return Promise.resolve({ code: server.exitCode, signal: server.signalCode });
  }
  return new Promise((resolve, reject) => {
    server.once("exit", (code, signal) => resolve({ code, signal }));
    server.once("error", reject);
  });
}

async function startServer(
  socket: string,
  authorityPath: string,
  identityPath: string,
): Promise<StartedServer> {
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
    {
      cwd: process.cwd(),
      env: (() => {
        const environment = { ...process.env };
        for (const key of HOOK_ENVIRONMENT_KEYS) delete environment[key];
        return environment;
      })(),
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  const fixture = await new Promise<Fixture>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("external-crash server did not become ready")),
      60_000,
    );
    let stdout = "";
    let stderr = "";
    const inspect = (): void => {
      const lines = stdout.split(/\r?\n/);
      const fixtureLine = lines.find((line) => line.startsWith("FIXTURE "));
      if (lines.includes("READY") && fixtureLine !== undefined) {
        clearTimeout(timer);
        resolve(parseFixture(fixtureLine));
      }
    };
    server.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
      inspect();
    });
    server.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    server.once("exit", (code, signal) => {
      clearTimeout(timer);
      reject(
        new Error(
          `external-crash server exited before ready (${code ?? signal ?? "unknown"}): ${stderr}`,
        ),
      );
    });
    server.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
  return { server, fixture };
}

function bytes(fixture: Fixture, key: string): Uint8Array {
  return fixtureBytes(fixture, key);
}

function submitRequest(fixture: Fixture, requestSeed: number) {
  const byte = (value: number): number => value & 0xff;
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
      requestId: new Uint8Array(16).fill(byte(requestSeed)),
      service: "takeover_control",
      method: "submit_barrier_observation",
      commonContext: {
        case: "requestContext",
        value: create(SabiRequestContextSchema, {
          caller: create(CallerIdentitySchema, {
            principalId: new Uint8Array(16).fill(byte(requestSeed + 3)),
            applicationId: new Uint8Array(16).fill(byte(requestSeed + 4)),
            processId: new Uint8Array(16).fill(byte(requestSeed + 5)),
            processGeneration: 1n,
          }),
          correlationId: new Uint8Array(16).fill(byte(requestSeed + 1)),
          idempotencyKey: new Uint8Array(16).fill(0xd3),
          deadlineMonotonicNs: 1_000n,
          capabilityHandles: [
            create(CapabilityHandleSchema, { slot: 5n, generation: 1n }),
          ],
        }),
      },
      payload,
    }),
  });
}

function assertSuccess(
  response: Awaited<ReturnType<LocalRpcClient["exchange"]>>,
  fixture: Fixture,
): void {
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

function terminateServer(
  server: ChildProcessWithoutNullStreams,
  force: boolean,
): string {
  if (process.platform === "win32") {
    server.kill();
    return "TerminateProcess";
  }
  const signal = force ? "SIGKILL" : "SIGTERM";
  server.kill(signal);
  return signal;
}

async function runTrial(trial: number, trials: number): Promise<void> {
  const crashSocket = endpoint("ec");
  const restartSocket = endpoint("er");
  const suffix = `${process.pid}-${Date.now()}-${trial}`;
  const authorityPath = join(tmpdir(), `nlos-takeover-${suffix}-external.sqlite3`);
  const identityPath = join(tmpdir(), `nlos-takeover-${suffix}-external-identity`);
  let server: ChildProcessWithoutNullStreams | undefined;
  let client: LocalRpcClient | undefined;
  const delayMs = randomInt(0, MAX_EXTERNAL_DELAY_MS + 1);
  const force = process.platform !== "win32" && trial % 2 === 1;
  let initialState: "success" | "transport_error" = "transport_error";
  try {
    const started = await startServer(crashSocket, authorityPath, identityPath);
    server = started.server;
    const request = submitRequest(started.fixture, 0xe1 + trial);
    client = await LocalRpcClient.connect(crashSocket, {
      connectTimeoutMs: 2_000,
      readTimeoutMs: 2_000,
      writeTimeoutMs: 2_000,
    });
    const initialExchange = client.exchange(request);
    const terminationTimer = setTimeout(() => {
      if (server !== undefined && server.exitCode === null && server.signalCode === null) {
        terminateServer(server, force);
      }
    }, delayMs);
    let initialResponse: Awaited<ReturnType<LocalRpcClient["exchange"]>> | undefined;
    try {
      initialResponse = await initialExchange;
    } catch {
      initialState = "transport_error";
    }
    if (initialResponse !== undefined) {
      assertSuccess(initialResponse, started.fixture);
      initialState = "success";
    }
    const crashExit = await waitForExit(server);
    clearTimeout(terminationTimer);
    assert.ok(
      crashExit.code !== 0 || crashExit.signal !== null,
      `trial ${trial} server unexpectedly exited cleanly`,
    );
    client.close();
    client = undefined;

    const recovered = await startServer(restartSocket, authorityPath, identityPath);
    server = recovered.server;
    assert.deepEqual(recovered.fixture, started.fixture);
    const config = {
      connectTimeoutMs: 2_000,
      readTimeoutMs: 2_000,
      writeTimeoutMs: 2_000,
    };
    const recoveryClient = await LocalRpcClient.connect(restartSocket, config);
    const recoveredResponse = await recoveryClient.exchange(request);
    assertSuccess(recoveredResponse, recovered.fixture);
    recoveryClient.close();

    const replayClient = await LocalRpcClient.connect(restartSocket, config);
    const replay = await replayClient.exchange(request);
    assertSuccess(replay, recovered.fixture);
    assert.deepEqual(
      toBinary(ExchangeResponseSchema, recoveredResponse),
      toBinary(ExchangeResponseSchema, replay),
    );
    replayClient.close();
    assert.equal((await waitForExit(server)).code, 0);
    const exitLabel = crashExit.signal ?? `code:${crashExit.code ?? "null"}`;
    console.log(
      `EXTERNAL_CRASH trial=${trial + 1}/${trials} delay_ms=${delayMs} ` +
        `termination=${process.platform === "win32" ? "TerminateProcess" : force ? "SIGKILL" : "SIGTERM"} ` +
        `initial=${initialState} exit=${exitLabel} recovery=success replay=byte_equal`,
    );
  } finally {
    client?.close();
    if (server !== undefined && server.exitCode === null && server.signalCode === null) {
      terminateServer(server, force);
      await waitForExit(server).catch(() => undefined);
    }
    await Promise.all([
      rm(crashSocket, { force: true }),
      rm(restartSocket, { force: true }),
      rm(authorityPath, { force: true }),
      rm(`${authorityPath}-wal`, { force: true }),
      rm(`${authorityPath}-shm`, { force: true }),
      rm(identityPath, { recursive: true, force: true }),
    ]);
  }
}

const trials = parseTrials();
for (let trial = 0; trial < trials; trial += 1) {
  await runTrial(trial, trials);
}
console.log(`EXTERNAL_CRASH_SUMMARY trials=${trials} platform=${process.platform}`);
