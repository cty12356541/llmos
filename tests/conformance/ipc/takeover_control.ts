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
  LocalRpcClient,
} from "../../../sdk/typescript/src/local_rpc.ts";
import { validateResponseContext } from "../../../sdk/typescript/src/common.ts";

type Fixture = Record<string, string>;
type StartedServer = {
  server: ChildProcessWithoutNullStreams;
  fixture: Fixture;
  waitForLine: (prefix: string) => Promise<string>;
};

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

function waitForServerLine(
  server: ChildProcessWithoutNullStreams,
  prefix: string,
): Promise<string> {
  return new Promise((resolve, reject) => {
    let output = "";
    const timer = setTimeout(
      () => finish(new Error(`server did not emit ${prefix} within 60 seconds`)),
      60_000,
    );
    const onData = (chunk: Buffer): void => {
      output += chunk.toString("utf8");
      const line = output.split(/\r?\n/).find((candidate) => candidate.startsWith(prefix));
      if (line !== undefined) {
        finish(undefined, line);
      }
    };
    const onExit = (code: number | null, signal: NodeJS.Signals | null): void =>
      finish(new Error(`server exited before ${prefix} (${code ?? signal ?? "unknown"})`));
    const onError = (error: Error): void => finish(error);
    const finish = (error?: Error, line?: string): void => {
      clearTimeout(timer);
      server.stdout.off("data", onData);
      server.off("exit", onExit);
      server.off("error", onError);
      if (error === undefined && line !== undefined) {
        resolve(line);
      } else {
        reject(error ?? new Error(`server line ${prefix} is missing`));
      }
    };
    server.stdout.on("data", onData);
    server.once("exit", onExit);
    server.once("error", onError);
  });
}

async function startServer(
  socket: string,
  authorityPath: string,
  identityPath: string,
  environment: NodeJS.ProcessEnv = {},
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
        const serverEnvironment = { ...process.env };
        delete serverEnvironment.NLOS_TAKEOVER_CONTROL_HOLD_AFTER_COMMIT;
        return { ...serverEnvironment, ...environment };
      })(),
      stdio: ["pipe", "pipe", "pipe"],
    },
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
  return {
    server,
    fixture: result,
    waitForLine: (prefix) => waitForServerLine(server, prefix),
  };
}

function submitRequest(fixture: Fixture, requestSeed = 0xd1) {
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

async function runCrashRestart(): Promise<void> {
  const crashSocket = endpoint("ts-crash");
  const restartSocket = endpoint("ts-restart");
  const authorityPath = join(tmpdir(), `nlos-takeover-${process.pid}-${Date.now()}-crash.sqlite3`);
  const identityPath = join(
    tmpdir(),
    `nlos-takeover-${process.pid}-${Date.now()}-crash-identity`,
  );
  let server: ChildProcessWithoutNullStreams | undefined;
  let client: LocalRpcClient | undefined;
  try {
    const crashed = await startServer(crashSocket, authorityPath, identityPath, {
      NLOS_TAKEOVER_CONTROL_HOLD_AFTER_COMMIT: "1",
    });
    server = crashed.server;
    const request = submitRequest(crashed.fixture);
    client = await LocalRpcClient.connect(crashSocket, {
      connectTimeoutMs: 2_000,
      readTimeoutMs: 2_000,
      writeTimeoutMs: 2_000,
    });
    const inFlight = assert.rejects(client.exchange(request));
    await crashed.waitForLine("COMMIT_READY");
    server.kill();
    const crashExit = await waitForExit(server);
    assert.notEqual(crashExit.code, 0);
    await inFlight;
    client.close();
    client = undefined;

    const recovered = await startServer(restartSocket, authorityPath, identityPath);
    server = recovered.server;
    assert.deepEqual(recovered.fixture, crashed.fixture);
    const recoveryRequest = submitRequest(crashed.fixture);
    const recoveryClient = await LocalRpcClient.connect(restartSocket, {
      connectTimeoutMs: 2_000,
      readTimeoutMs: 2_000,
      writeTimeoutMs: 2_000,
    });
    const recoveredResponse = await recoveryClient.exchange(recoveryRequest);
    assertSuccess(recoveredResponse, recovered.fixture);
    recoveryClient.close();

    const replayClient = await LocalRpcClient.connect(restartSocket, {
      connectTimeoutMs: 2_000,
      readTimeoutMs: 2_000,
      writeTimeoutMs: 2_000,
    });
    const replay = await replayClient.exchange(recoveryRequest);
    assertSuccess(replay, recovered.fixture);
    assert.deepEqual(
      toBinary(ExchangeResponseSchema, recoveredResponse),
      toBinary(ExchangeResponseSchema, replay),
    );
    replayClient.close();
    assert.equal((await waitForExit(server)).code, 0);
  } catch (error) {
    client?.close();
    if (server !== undefined && server.exitCode === null && server.signalCode === null) {
      server.kill();
      await waitForExit(server).catch(() => undefined);
    }
    throw error;
  } finally {
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

async function runConcurrentPressure(): Promise<void> {
  const socket = endpoint("tp");
  const authorityPath = join(
    tmpdir(),
    `nlos-takeover-${process.pid}-${Date.now()}-pressure.sqlite3`,
  );
  const identityPath = join(
    tmpdir(),
    `nlos-takeover-${process.pid}-${Date.now()}-pressure-identity`,
  );
  let server: ChildProcessWithoutNullStreams | undefined;
  const clients: LocalRpcClient[] = [];
  try {
    const started = await startServer(socket, authorityPath, identityPath, {
      NLOS_TAKEOVER_CONTROL_CONNECTIONS: "8",
    });
    server = started.server;
    const config = {
      connectTimeoutMs: 2_000,
      readTimeoutMs: 5_000,
      writeTimeoutMs: 5_000,
    };
    for (let index = 0; index < 8; index += 1) {
      clients.push(await LocalRpcClient.connect(socket, config));
    }
    const responses = await Promise.all(
      clients.map((client, index) => client.exchange(submitRequest(started.fixture, 0xd1 + index))),
    );
    const firstResponse = responses[0]!;
    assertSuccess(firstResponse, started.fixture);
    const firstRecordWire = toBinary(
      BarrierObservationRecordSchema,
      fromBinary(BarrierObservationRecordSchema, firstResponse.envelope!.payload),
    );
    for (const [index, response] of responses.entries()) {
      assertSuccess(response, started.fixture);
      assert.deepEqual(
        toBinary(
          BarrierObservationRecordSchema,
          fromBinary(BarrierObservationRecordSchema, response.envelope!.payload),
        ),
        firstRecordWire,
        `pressure durable record ${index} differs from the first replay`,
      );
    }
    for (const client of clients) {
      client.close();
    }
    assert.equal((await waitForExit(server)).code, 0);
  } catch (error) {
    if (server !== undefined && server.exitCode === null && server.signalCode === null) {
      server.kill();
      await waitForExit(server).catch(() => undefined);
    }
    throw error;
  } finally {
    for (const client of clients) {
      client.close();
    }
    await Promise.all([
      rm(socket, { force: true }),
      rm(authorityPath, { force: true }),
      rm(`${authorityPath}-wal`, { force: true }),
      rm(`${authorityPath}-shm`, { force: true }),
      rm(identityPath, { recursive: true, force: true }),
    ]);
  }
}

async function runTornWalRecovery(): Promise<void> {
  const tornSocket = endpoint("tw");
  const restartSocket = endpoint("tr");
  const authorityPath = join(tmpdir(), `nlos-takeover-${process.pid}-${Date.now()}-torn.sqlite3`);
  const identityPath = join(
    tmpdir(),
    `nlos-takeover-${process.pid}-${Date.now()}-torn-identity`,
  );
  let server: ChildProcessWithoutNullStreams | undefined;
  let client: LocalRpcClient | undefined;
  try {
    const torn = await startServer(tornSocket, authorityPath, identityPath, {
      NLOS_TAKEOVER_CONTROL_HOLD_AFTER_COMMIT: "1",
      NLOS_TAKEOVER_CONTROL_TRUNCATE_WAL_AFTER_COMMIT: "1",
    });
    server = torn.server;
    const request = submitRequest(torn.fixture);
    client = await LocalRpcClient.connect(tornSocket, {
      connectTimeoutMs: 2_000,
      readTimeoutMs: 2_000,
      writeTimeoutMs: 2_000,
    });
    const inFlight = assert.rejects(client.exchange(request));
    await torn.waitForLine("WAL_TORN_READY");
    server.kill();
    const crashExit = await waitForExit(server);
    assert.notEqual(crashExit.code, 0);
    await inFlight;
    client.close();
    client = undefined;
    await rm(`${authorityPath}-shm`, { force: true });

    const recovered = await startServer(restartSocket, authorityPath, identityPath);
    server = recovered.server;
    assert.deepEqual(recovered.fixture, torn.fixture);
    const recoveryClient = await LocalRpcClient.connect(restartSocket, {
      connectTimeoutMs: 2_000,
      readTimeoutMs: 2_000,
      writeTimeoutMs: 2_000,
    });
    const response = await recoveryClient.exchange(request);
    assertSuccess(response, recovered.fixture);
    recoveryClient.close();

    const replayClient = await LocalRpcClient.connect(restartSocket, {
      connectTimeoutMs: 2_000,
      readTimeoutMs: 2_000,
      writeTimeoutMs: 2_000,
    });
    const replay = await replayClient.exchange(request);
    assertSuccess(replay, recovered.fixture);
    assert.deepEqual(
      toBinary(ExchangeResponseSchema, response),
      toBinary(ExchangeResponseSchema, replay),
    );
    replayClient.close();
    assert.equal((await waitForExit(server)).code, 0);
  } catch (error) {
    client?.close();
    if (server !== undefined && server.exitCode === null && server.signalCode === null) {
      server.kill();
      await waitForExit(server).catch(() => undefined);
    }
    throw error;
  } finally {
    await Promise.all([
      rm(tornSocket, { force: true }),
      rm(restartSocket, { force: true }),
      rm(authorityPath, { force: true }),
      rm(`${authorityPath}-wal`, { force: true }),
      rm(`${authorityPath}-shm`, { force: true }),
      rm(identityPath, { recursive: true, force: true }),
    ]);
  }
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

await runCrashRestart();

await runConcurrentPressure();

await runTornWalRecovery();
