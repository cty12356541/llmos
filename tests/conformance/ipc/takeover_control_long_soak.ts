/**
 * Minute-scale cross-language TakeoverControl IPC soak.
 *
 * This is intentionally separate from the bounded conformance matrix.  It
 * keeps a configurable set of real Unix-socket/named-pipe connections open
 * before each unary call, repeats the same idempotent mutation over bounded
 * server rounds, and requires every response to carry the same durable record.
 * The conformance server performs the final one-row/LocallyCovered check when
 * it exits.
 */

import assert from "node:assert/strict";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
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
type SoakOptions = {
  durationMs: number;
  rounds: number;
  connections: number;
};

const DEFAULT_DURATION_MS = 60_000;
const DEFAULT_ROUNDS = 8;
const DEFAULT_CONNECTIONS = 32;
const MAX_TIMER_MS = 2_147_483_647;

function endpoint(): string {
  // Keep the Unix path deliberately short: macOS/Linux sockaddr_un paths have
  // a small SUN_LEN bound, and tmpdir() may already contain a long prefix.
  const unique = `${process.pid}-${Date.now()}`;
  return process.platform === "win32"
    ? `\\\\.\\pipe\\nlos-takeover-${unique}`
    : join(tmpdir(), `nlos-takeover-${unique}-ls.sock`);
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

function readServerManifest(
  server: ChildProcessWithoutNullStreams,
): Promise<Fixture> {
  return new Promise((resolve, reject) => {
    let stdout = "";
    let stderr = "";
    const timer = setTimeout(
      () => finish(new Error("TakeoverControl long-soak server did not become ready")),
      60_000,
    );
    const inspect = (): void => {
      const ready = stdout.split(/\r?\n/).includes("READY");
      const fixtureLine = stdout
        .split(/\r?\n/)
        .find((line) => line.startsWith("FIXTURE "));
      if (ready && fixtureLine !== undefined) {
        finish(undefined, parseFixture(fixtureLine));
      }
    };
    const onStdout = (chunk: Buffer): void => {
      stdout += chunk.toString("utf8");
      inspect();
    };
    const onStderr = (chunk: Buffer): void => {
      stderr += chunk.toString("utf8");
    };
    const onExit = (code: number | null, signal: NodeJS.Signals | null): void =>
      finish(
        new Error(
          `TakeoverControl long-soak server exited before ready (${code ?? signal ?? "unknown"}): ${stderr}`,
        ),
      );
    const onError = (error: Error): void => finish(error);
    const finish = (error?: Error, fixture?: Fixture): void => {
      clearTimeout(timer);
      server.stdout.off("data", onStdout);
      server.stderr.off("data", onStderr);
      server.off("exit", onExit);
      server.off("error", onError);
      if (error === undefined && fixture !== undefined) {
        resolve(fixture);
      } else {
        reject(error ?? new Error("server fixture manifest is missing"));
      }
    };
    server.stdout.on("data", onStdout);
    server.stderr.on("data", onStderr);
    server.once("exit", onExit);
    server.once("error", onError);
  });
}

async function startServer(
  socket: string,
  authorityPath: string,
  identityPath: string,
  options: SoakOptions,
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
        delete environment.NLOS_TAKEOVER_CONTROL_HOLD_BEFORE_COMMIT;
        delete environment.NLOS_TAKEOVER_CONTROL_HOLD_AFTER_COMMIT;
        delete environment.NLOS_TAKEOVER_CONTROL_TRUNCATE_WAL_AFTER_COMMIT;
        delete environment.NLOS_TAKEOVER_CONTROL_RANDOM_CRASH_PHASE;
        delete environment.NLOS_TAKEOVER_CONTROL_RANDOM_CRASH_SEED;
        environment.NLOS_TAKEOVER_CONTROL_CONNECTIONS = String(options.connections);
        environment.NLOS_TAKEOVER_CONTROL_ROUNDS = String(options.rounds);
        return environment;
      })(),
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  try {
    return { server, fixture: await readServerManifest(server) };
  } catch (error) {
    if (server.exitCode === null && server.signalCode === null) {
      server.kill();
      await waitForExit(server).catch(() => undefined);
    }
    throw error;
  }
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
  request: ReturnType<typeof submitRequest>,
  fixture: Fixture,
): Uint8Array {
  assert.ok(response.envelope);
  const context = validateResponseContext(response.envelope, {
    sideEffecting: true,
    longRunning: false,
  });
  assert.equal(context.failure, undefined);
  assert.ok(request.envelope);
  assert.equal(request.envelope.commonContext.case, "requestContext");
  assert.deepEqual(
    Uint8Array.from(context.correlationId),
    Uint8Array.from(request.envelope.commonContext.value.correlationId),
  );
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
  // Compare the exact payload bytes returned by the durable handler, not a
  // re-encoded projection that could discard unknown protobuf fields.
  return Uint8Array.from(response.envelope.payload);
}

function readInteger(
  args: string[],
  name: string,
  environmentName: string,
  fallback: number,
): number {
  const inline = args.find((argument) => argument.startsWith(`${name}=`));
  const index = args.indexOf(name);
  const raw = inline?.slice(name.length + 1) ?? (index >= 0 ? args[index + 1] : undefined);
  const value = raw ?? process.env[environmentName] ?? String(fallback);
  assert.match(value, /^\d+$/, `${name} must be a non-negative integer`);
  const parsed = Number(value);
  assert.ok(Number.isSafeInteger(parsed), `${name} is outside the safe integer range`);
  return parsed;
}

function parseOptions(): SoakOptions {
  const args = process.argv.slice(2);
  const names = new Set(["--duration-ms", "--rounds", "--connections"]);
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index]!;
    if (names.has(argument)) {
      const value = args[index + 1];
      if (value === undefined || value.startsWith("--")) {
        throw new Error(`${argument} requires a value`);
      }
      index += 1;
    } else if (![...names].some((name) => argument.startsWith(`${name}=`))) {
      throw new Error(
        `unknown argument ${argument}; use --duration-ms, --rounds, and --connections`,
      );
    }
  }
  const durationMs = readInteger(
    args,
    "--duration-ms",
    "NLOS_TAKEOVER_CONTROL_LONG_SOAK_DURATION_MS",
    DEFAULT_DURATION_MS,
  );
  const rounds = readInteger(
    args,
    "--rounds",
    "NLOS_TAKEOVER_CONTROL_LONG_SOAK_ROUNDS",
    DEFAULT_ROUNDS,
  );
  const connections = readInteger(
    args,
    "--connections",
    "NLOS_TAKEOVER_CONTROL_LONG_SOAK_CONNECTIONS",
    DEFAULT_CONNECTIONS,
  );
  assert.ok(durationMs > 0 && durationMs <= MAX_TIMER_MS, "duration must be within 1..=2^31-1 ms");
  assert.ok(rounds >= 1 && rounds <= 8, "rounds must be within 1..=8");
  assert.ok(connections >= 2 && connections <= 32, "connections must be within 2..=32");
  return { durationMs, rounds, connections };
}

function sleep(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

async function run(options: SoakOptions): Promise<void> {
  // The conformance server uses the normal bounded transport read timeout;
  // cap the idle connection hold below that timeout and repeat fresh fixture
  // sessions until the requested aggregate pressure duration is reached.
  const holdMs = Math.min(1_000, Math.max(1, Math.ceil(options.durationMs / options.rounds)));
  const config = {
    connectTimeoutMs: 5_000,
    readTimeoutMs: Math.max(5_000, holdMs + 5_000),
    writeTimeoutMs: 5_000,
  };
  let pressureMs = 0;
  let sessionCount = 0;
  let completedSessions = 0;
  const startedAt = performance.now();
  while (pressureMs < options.durationMs || sessionCount === 0) {
    const socket = endpoint();
    const stamp = `${process.pid}-${Date.now()}-${sessionCount}`;
    const authorityPath = join(tmpdir(), `nlos-takeover-${stamp}-long-soak.sqlite3`);
    const identityPath = join(tmpdir(), `nlos-takeover-${stamp}-long-soak-identity`);
    let started: StartedServer | undefined;
    const clients: LocalRpcClient[] = [];
    try {
      started = await startServer(socket, authorityPath, identityPath, options);
      let firstRecordWire: Uint8Array | undefined;
      for (let round = 0; round < options.rounds; round += 1) {
        const roundClients = await Promise.all(
          Array.from(
            { length: options.connections },
            () => LocalRpcClient.connect(socket, config),
          ),
        );
        clients.push(...roundClients);
        try {
          // Keep every real endpoint connection open simultaneously.  The
          // server is blocked in the same round until all unary handlers finish.
          await sleep(holdMs);
          const requests = roundClients.map((_, index) =>
            submitRequest(
              started!.fixture,
              0x20 + round * options.connections + index,
            ),
          );
          const responses = await Promise.all(
            roundClients.map((client, index) => client.exchange(requests[index]!)),
          );
          for (const [index, response] of responses.entries()) {
            const recordWire = assertSuccess(response, requests[index]!, started.fixture);
            if (firstRecordWire === undefined) {
              firstRecordWire = recordWire;
            } else {
              assert.deepEqual(
                recordWire,
                firstRecordWire,
                `session ${sessionCount} round ${round} connection ${index} durable record differs`,
              );
            }
            completedSessions += 1;
          }
        } finally {
          for (const client of roundClients) {
            client.close();
            const index = clients.indexOf(client);
            if (index >= 0) clients.splice(index, 1);
          }
        }
      }
      const exit = await waitForExit(started.server);
      assert.equal(
        exit.code,
        0,
        `long-soak server failed (${exit.code ?? exit.signal ?? "unknown"})`,
      );
      pressureMs += holdMs * options.rounds;
      sessionCount += 1;
    } finally {
      for (const client of clients) client.close();
      if (started?.server.exitCode === null && started.server.signalCode === null) {
        started.server.kill();
        await waitForExit(started.server).catch(() => undefined);
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
  const elapsedMs = Math.round(performance.now() - startedAt);
  assert.ok(
    pressureMs >= options.durationMs,
    `long-soak pressure ${pressureMs}ms is shorter than requested ${options.durationMs}ms`,
  );
  console.log(
    `LONG_SOAK duration_ms=${options.durationMs} elapsed_ms=${elapsedMs} pressure_ms=${pressureMs} rounds=${options.rounds} connections=${options.connections} sessions=${sessionCount} calls=${completedSessions} durable_rows=1 coverage=LocallyCovered`,
  );
}

await run(parseOptions());
