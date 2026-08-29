import assert from "node:assert/strict";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import {
  CancelWaitRequestSchema,
  CancelWaitResultSchema,
  InspectWaitRequestSchema,
  InspectWaitResultSchema,
  ListWaitsRequestSchema,
  ListWaitsResultSchema,
  NotifyCommitsRequestSchema,
  RegisterWaitRequestSchema,
  RegisterWaitResultSchema,
  WakeReportSchema,
  WaitStateCode,
  type WaitRecord,
} from "../../../gen/typescript/nlos/sabi/v1/wait_control_pb.ts";
import {
  CallerIdentitySchema,
  CapabilityHandleSchema,
  EnvelopeSchema,
  ExchangeRequestSchema,
  ExchangeResponseSchema,
  RetryDirective,
  SabiErrorCode,
  SabiRequestContextSchema,
  SchemaIdentitySchema,
  type CapabilityHandle,
  type ExchangeRequest,
  type ExchangeResponse,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import { LocalRpcClient } from "../../../sdk/typescript/src/local_rpc.ts";
import { validateResponseContext } from "../../../sdk/typescript/src/common.ts";

type Fixture = Record<string, string>;
type StartedServer = {
  server: ChildProcessWithoutNullStreams;
  fixture: Promise<Fixture>;
};

const CONNECTIONS_ENV = "NLOS_WAIT_CONTROL_CONNECTIONS";
const ROUNDS_ENV = "NLOS_WAIT_CONTROL_ROUNDS";
const SCENE_ENV = "NLOS_WAIT_CONTROL_SCENE";
const CAPABILITY_SLOT = 9n;
const CAPABILITY_GENERATION = 1n;
const CLIENT_CONFIG = {
  connectTimeoutMs: 2_000,
  readTimeoutMs: 5_000,
  writeTimeoutMs: 5_000,
};
const PAYLOAD_SCHEMA = create(SchemaIdentitySchema, {
  name: "nlos.sabi.WaitControl",
  major: 1,
  minor: 0,
});

function endpoint(label: string): string {
  const unique = `${process.pid}-${Date.now()}-${label}`;
  return process.platform === "win32"
    ? `\\\\.\\pipe\\nlos-wait-${unique}`
    : join(tmpdir(), `nlos-wait-${unique}.sock`);
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

function filled(seed: number): Uint8Array {
  return new Uint8Array(16).fill(seed & 0xff);
}

function waitForExit(server: ChildProcessWithoutNullStreams): Promise<number | null> {
  if (server.exitCode !== null || server.signalCode !== null) {
    return Promise.resolve(server.exitCode);
  }
  return new Promise((resolve, reject) => {
    server.once("exit", (code) => resolve(code));
    server.once("error", reject);
  });
}

function startServer(
  socket: string,
  authorityRoot: string,
  environment: NodeJS.ProcessEnv = {},
): StartedServer {
  const server = spawn(
    "cargo",
    [
      "run",
      "--quiet",
      "-p",
      "nlos-wait-control",
      "--features",
      "conformance-server",
      "--bin",
      "wait-control-conformance",
      "--",
      socket,
      authorityRoot,
    ],
    {
      cwd: process.cwd(),
      env: (() => {
        const serverEnvironment = { ...process.env };
        delete serverEnvironment[CONNECTIONS_ENV];
        delete serverEnvironment[ROUNDS_ENV];
        delete serverEnvironment[SCENE_ENV];
        return { ...serverEnvironment, ...environment };
      })(),
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  const fixture = new Promise<Fixture>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("WaitControl conformance server did not become ready")),
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
      reject(new Error(`WaitControl server exited before ready (${code}): ${stderr}`));
    });
    server.once("error", (error) => {
      clearTimeout(timer);
      reject(error);
    });
  });
  return { server, fixture };
}

async function stopServer(server: ChildProcessWithoutNullStreams): Promise<void> {
  if (server.exitCode === null && server.signalCode === null) {
    server.kill();
    await waitForExit(server);
  }
}

/// One connection serves exactly one request, so every exchange opens a
/// fresh client exactly like the takeover conformance clients do.
async function exchange(socket: string, request: ExchangeRequest): Promise<ExchangeResponse> {
  const client = await LocalRpcClient.connect(socket, CLIENT_CONFIG);
  try {
    return await client.exchange(request);
  } finally {
    client.close();
  }
}

type RequestOptions = {
  requestSeed: number;
  method: string;
  service?: string;
  idempotencyKey?: Uint8Array;
  payload: Uint8Array;
  /// `undefined` sends the authorizing handle, a bigint sends a wrong slot,
  /// and `null` sends no capability handle at all.
  capabilitySlot?: bigint | null;
};

function buildExchange(options: RequestOptions): ExchangeRequest {
  const capabilityHandles: CapabilityHandle[] =
    options.capabilitySlot === undefined
      ? [
        create(CapabilityHandleSchema, {
          slot: CAPABILITY_SLOT,
          generation: CAPABILITY_GENERATION,
        }),
      ]
      : options.capabilitySlot === null
      ? []
      : [
        create(CapabilityHandleSchema, {
          slot: options.capabilitySlot,
          generation: CAPABILITY_GENERATION,
        }),
      ];
  return create(ExchangeRequestSchema, {
    envelope: create(EnvelopeSchema, {
      schema: create(SchemaIdentitySchema, {
        name: "nlos.sabi.Envelope",
        major: 1,
        minor: 1,
      }),
      requestId: filled(options.requestSeed),
      service: options.service ?? "wait_control",
      method: options.method,
      commonContext: {
        case: "requestContext",
        value: create(SabiRequestContextSchema, {
          caller: create(CallerIdentitySchema, {
            principalId: filled(0x31),
            applicationId: filled(0x32),
            processId: filled(0x33),
            processGeneration: 1n,
          }),
          correlationId: filled(options.requestSeed + 1),
          idempotencyKey: options.idempotencyKey ?? new Uint8Array(),
          deadlineMonotonicNs: 1_000n,
          capabilityHandles,
        }),
      },
      payload: options.payload,
    }),
  });
}

function registerPayload(
  channelId: Uint8Array,
  bindingSeed: number,
  targetSequence: bigint,
  keySeed: number,
): Uint8Array {
  return toBinary(
    RegisterWaitRequestSchema,
    create(RegisterWaitRequestSchema, {
      schema: PAYLOAD_SCHEMA,
      binding: filled(bindingSeed),
      channelId,
      targetSequence,
      idempotencyKey: filled(keySeed),
      registeredAtMs: 1_000n,
    }),
  );
}

function notifyPayload(
  channelId: Uint8Array,
  upToSequence: bigint,
  keySeed: number,
): Uint8Array {
  return toBinary(
    NotifyCommitsRequestSchema,
    create(NotifyCommitsRequestSchema, {
      schema: PAYLOAD_SCHEMA,
      channelId,
      upToSequence,
      notifiedAtMs: 2_000n,
      idempotencyKey: filled(keySeed),
    }),
  );
}

function cancelPayload(waitId: Uint8Array, keySeed: number): Uint8Array {
  return toBinary(
    CancelWaitRequestSchema,
    create(CancelWaitRequestSchema, {
      schema: PAYLOAD_SCHEMA,
      waitId,
      cancelledAtMs: 3_000n,
      idempotencyKey: filled(keySeed),
    }),
  );
}

function listPayload(): Uint8Array {
  return toBinary(
    ListWaitsRequestSchema,
    create(ListWaitsRequestSchema, {
      schema: PAYLOAD_SCHEMA,
      filterChannelId: new Uint8Array(),
    }),
  );
}

function inspectPayload(waitId: Uint8Array): Uint8Array {
  return toBinary(
    InspectWaitRequestSchema,
    create(InspectWaitRequestSchema, {
      schema: PAYLOAD_SCHEMA,
      waitId,
    }),
  );
}

/// Asserts the bounded success envelope shape and returns the validated
/// response context for receipt assertions.
function assertSuccess(
  response: ExchangeResponse,
  requestSeed: number,
  sideEffecting: boolean,
) {
  assert.ok(response.envelope);
  const context = validateResponseContext(response.envelope, {
    sideEffecting,
    longRunning: false,
  });
  assert.equal(context.failure, undefined);
  assert.deepEqual(Uint8Array.from(context.correlationId), filled(requestSeed + 1));
  assert.deepEqual(Uint8Array.from(response.envelope.requestId), filled(requestSeed));
  assert.equal(response.envelope.service, "wait_control");
  return context;
}

/// Asserts the bounded failure envelope shape: payload and all evidence
/// cleared, request identity retained, typed code/retry, correlation echoed.
function assertFailure(
  response: ExchangeResponse,
  request: ExchangeRequest,
  code: SabiErrorCode,
  retry: RetryDirective,
): void {
  assert.ok(response.envelope);
  assert.ok(request.envelope);
  assert.equal(response.envelope.payload.length, 0);
  assert.deepEqual(
    Uint8Array.from(response.envelope.requestId),
    Uint8Array.from(request.envelope.requestId),
  );
  assert.equal(response.envelope.service, request.envelope.service);
  assert.equal(response.envelope.method, request.envelope.method);
  const context = validateResponseContext(response.envelope, {
    sideEffecting: true,
    longRunning: false,
  });
  assert.ok(context.failure, "failure must be present");
  assert.equal(context.failure.code, code);
  assert.equal(context.failure.retry, retry);
  assert.equal(context.receipts.length, 0);
  assert.equal(context.operation, undefined);
  const requestContext = request.envelope.commonContext;
  assert.ok(requestContext.case === "requestContext");
  assert.deepEqual(
    Uint8Array.from(context.correlationId),
    Uint8Array.from(requestContext.value.correlationId),
  );
}

function receiptIds(context: { receipts: { receiptId: Uint8Array }[] }): Uint8Array[] {
  return context.receipts.map((receipt) => Uint8Array.from(receipt.receiptId));
}

/// The canonical `fresh`-scene script: all five methods roundtrip over real
/// IPC, every mutation replays durably, and every rejection class surfaces
/// the bounded failure shape without touching the registry.
async function runFreshScenario(authorityRoot: string): Promise<{
  register1: ExchangeRequest;
  record1: WaitRecord;
}> {
  const socket = endpoint("ts-fresh");
  let server: ChildProcessWithoutNullStreams | undefined;
  try {
    const started = startServer(socket, authorityRoot, { [CONNECTIONS_ENV]: "14" });
    server = started.server;
    const fixture = await started.fixture;
    const channelId = bytes(fixture, "channel_id");

    // 1. register_wait crosses real IPC and carries the durable row receipt.
    const register1 = buildExchange({
      requestSeed: 0xd1,
      method: "register_wait",
      idempotencyKey: filled(1),
      payload: registerPayload(channelId, 1, 5n, 1),
    });
    const registered = await exchange(socket, register1);
    const successContext = assertSuccess(registered, 0xd1, true);
    const result1 = fromBinary(RegisterWaitResultSchema, registered.envelope!.payload);
    assert.equal(result1.replayed, false);
    const record1 = result1.record;
    assert.ok(record1, "registration must carry the durable record");
    assert.equal(record1.state, WaitStateCode.PENDING);
    assert.equal(record1.targetSequence, 5n);
    assert.deepEqual(Uint8Array.from(record1.binding), filled(1));
    assert.deepEqual(Uint8Array.from(record1.channelId), channelId);
    assert.equal(record1.channelGeneration, 1n);
    assert.equal(record1.registeredAtMs, 1_000n);
    assert.equal(record1.channelFencingToken.length, 32);
    assert.deepEqual(receiptIds(successContext), [Uint8Array.from(record1.waitId)]);

    // 2. The exact same request replays the original durable row.
    const registerReplay = await exchange(socket, register1);
    assertSuccess(registerReplay, 0xd1, true);
    const replayResult1 = fromBinary(RegisterWaitResultSchema, registerReplay.envelope!.payload);
    assert.equal(replayResult1.replayed, true);
    assert.deepEqual(replayResult1.record, record1);

    // 3. notify_commits up to the target wakes the wait.
    const notify1 = buildExchange({
      requestSeed: 0xd2,
      method: "notify_commits",
      idempotencyKey: filled(9),
      payload: notifyPayload(channelId, 5n, 9),
    });
    const notified = await exchange(socket, notify1);
    const notifyContext = assertSuccess(notified, 0xd2, true);
    const report = fromBinary(WakeReportSchema, notified.envelope!.payload);
    assert.equal(report.woken.length, 1);
    assert.equal(report.woken[0].state, WaitStateCode.WOKEN);
    assert.equal(report.woken[0].targetSequence, 5n);
    assert.equal(report.woken[0].wokenUpToSequence, 5n);
    assert.equal(report.woken[0].wokenAtMs, 2_000n);
    // The notify receipt is keyed by the request idempotency key.
    assert.deepEqual(receiptIds(notifyContext), [filled(9)]);

    // 4. The notify replay returns the byte-identical durable report.
    const notifyReplay = await exchange(socket, notify1);
    assertSuccess(notifyReplay, 0xd2, true);
    assert.deepEqual(
      toBinary(ExchangeResponseSchema, notified),
      toBinary(ExchangeResponseSchema, notifyReplay),
    );

    // 5. inspect_wait returns the woken durable row.
    const inspect1 = buildExchange({
      requestSeed: 0xd4,
      method: "inspect_wait",
      payload: inspectPayload(record1.waitId),
    });
    const inspected = await exchange(socket, inspect1);
    assertSuccess(inspected, 0xd4, false);
    const inspectResult = fromBinary(InspectWaitResultSchema, inspected.envelope!.payload);
    assert.ok(inspectResult.record);
    assert.equal(inspectResult.record.state, WaitStateCode.WOKEN);
    assert.deepEqual(Uint8Array.from(inspectResult.record.waitId), Uint8Array.from(record1.waitId));
    assert.equal(inspectResult.record.wokenUpToSequence, 5n);

    // 6. list_waits enumerates the single woken row.
    const list1 = buildExchange({
      requestSeed: 0xd5,
      method: "list_waits",
      payload: listPayload(),
    });
    const listed = await exchange(socket, list1);
    assertSuccess(listed, 0xd5, false);
    const listResult = fromBinary(ListWaitsResultSchema, listed.envelope!.payload);
    assert.equal(listResult.waits.length, 1);
    assert.equal(listResult.waits[0].state, WaitStateCode.WOKEN);

    // 7. A second registration on the same channel.
    const register2 = buildExchange({
      requestSeed: 0xd6,
      method: "register_wait",
      idempotencyKey: filled(2),
      payload: registerPayload(channelId, 2, 7n, 2),
    });
    const registered2 = await exchange(socket, register2);
    assertSuccess(registered2, 0xd6, true);
    const result2 = fromBinary(RegisterWaitResultSchema, registered2.envelope!.payload);
    assert.equal(result2.record?.state, WaitStateCode.PENDING);

    // 8. cancel_wait flips the second wait to CANCELLED.
    const cancel1 = buildExchange({
      requestSeed: 0xd7,
      method: "cancel_wait",
      idempotencyKey: filled(3),
      payload: cancelPayload(result2.record!.waitId, 3),
    });
    const cancelled = await exchange(socket, cancel1);
    const cancelContext = assertSuccess(cancelled, 0xd7, true);
    const cancelResult = fromBinary(CancelWaitResultSchema, cancelled.envelope!.payload);
    assert.equal(cancelResult.replayed, false);
    assert.equal(cancelResult.record?.state, WaitStateCode.CANCELLED);
    assert.equal(cancelResult.record?.cancelledAtMs, 3_000n);
    assert.deepEqual(receiptIds(cancelContext), [filled(3)]);

    // 9. The cancellation replays durably.
    const cancelReplay = await exchange(socket, cancel1);
    assertSuccess(cancelReplay, 0xd7, true);
    const cancelReplayResult = fromBinary(CancelWaitResultSchema, cancelReplay.envelope!.payload);
    assert.equal(cancelReplayResult.replayed, true);
    assert.deepEqual(cancelReplayResult.record, cancelResult.record);

    // 10. Unknown method: bounded NOT_SUPPORTED, payload and evidence cleared.
    const unknownMethod = buildExchange({
      requestSeed: 0xd8,
      method: "frobnicate",
      payload: new Uint8Array([0xaa, 0xbb]),
    });
    assertFailure(
      await exchange(socket, unknownMethod),
      unknownMethod,
      SabiErrorCode.NOT_SUPPORTED,
      RetryDirective.DO_NOT_RETRY,
    );

    // 11. A foreign service name is equally NOT_SUPPORTED.
    const foreignService = buildExchange({
      requestSeed: 0xd9,
      service: "other_service",
      method: "list_waits",
      payload: listPayload(),
    });
    assertFailure(
      await exchange(socket, foreignService),
      foreignService,
      SabiErrorCode.NOT_SUPPORTED,
      RetryDirective.DO_NOT_RETRY,
    );

    // 12. A wrong capability slot is a policy denial: RIGHTS, no side effect.
    const denied = buildExchange({
      requestSeed: 0xda,
      method: "register_wait",
      idempotencyKey: filled(4),
      payload: registerPayload(channelId, 4, 9n, 4),
      capabilitySlot: 8n,
    });
    assertFailure(
      await exchange(socket, denied),
      denied,
      SabiErrorCode.RIGHTS,
      RetryDirective.DO_NOT_RETRY,
    );

    // 13. A payload key rebound against the context key is a CONFLICT.
    const mismatched = buildExchange({
      requestSeed: 0xdb,
      method: "register_wait",
      idempotencyKey: filled(4),
      payload: registerPayload(channelId, 5, 9n, 5),
    });
    assertFailure(
      await exchange(socket, mismatched),
      mismatched,
      SabiErrorCode.CONFLICT,
      RetryDirective.DO_NOT_RETRY,
    );

    // 14. The rejections above left no durable trace: exactly the two
    // canonical rows remain, in enumeration order.
    const list2 = buildExchange({
      requestSeed: 0xdc,
      method: "list_waits",
      payload: listPayload(),
    });
    const listed2 = await exchange(socket, list2);
    assertSuccess(listed2, 0xdc, false);
    const listResult2 = fromBinary(ListWaitsResultSchema, listed2.envelope!.payload);
    assert.equal(listResult2.waits.length, 2);
    assert.equal(listResult2.waits[0].state, WaitStateCode.WOKEN);
    assert.equal(listResult2.waits[0].targetSequence, 5n);
    assert.equal(listResult2.waits[1].state, WaitStateCode.CANCELLED);
    assert.equal(listResult2.waits[1].targetSequence, 7n);

    assert.equal(await waitForExit(server), 0);
    return { register1, record1 };
  } finally {
    if (server !== undefined) {
      await stopServer(server);
    }
    await rm(socket, { force: true });
  }
}

/// The `mixed`-scene script: preset rows covering every state are read back
/// through list/inspect and the preset pending row is cancelled.
async function runMixedScenario(): Promise<void> {
  const socket = endpoint("ts-mixed");
  const authorityRoot = `${join(tmpdir(), `nlos-wait-${process.pid}-${Date.now()}-mixed`)}`;
  let server: ChildProcessWithoutNullStreams | undefined;
  try {
    const started = startServer(socket, authorityRoot, {
      [SCENE_ENV]: "mixed",
      [CONNECTIONS_ENV]: "5",
    });
    server = started.server;
    const fixture = await started.fixture;

    // 1. list_waits enumerates every preset state in durable order.
    const list1 = buildExchange({
      requestSeed: 0xe1,
      method: "list_waits",
      payload: listPayload(),
    });
    const listed = await exchange(socket, list1);
    assertSuccess(listed, 0xe1, false);
    const listResult = fromBinary(ListWaitsResultSchema, listed.envelope!.payload);
    assert.equal(listResult.waits.length, 3);
    assert.deepEqual(
      listResult.waits.map((record) => record.state),
      [WaitStateCode.CANCELLED, WaitStateCode.WOKEN, WaitStateCode.PENDING],
    );
    assert.deepEqual(
      listResult.waits.map((record) => record.targetSequence),
      [1n, 2n, 3n],
    );
    assert.deepEqual(
      Uint8Array.from(listResult.waits[0].waitId),
      bytes(fixture, "cancelled_wait_id"),
    );
    assert.deepEqual(
      Uint8Array.from(listResult.waits[1].waitId),
      bytes(fixture, "woken_wait_id"),
    );
    assert.deepEqual(
      Uint8Array.from(listResult.waits[2].waitId),
      bytes(fixture, "pending_wait_id"),
    );

    // 2..4. inspect_wait returns each preset row with its durable facts.
    const inspectCases: [number, string, WaitStateCode, (record: WaitRecord) => void][] = [
      [
        0xe2,
        "cancelled_wait_id",
        WaitStateCode.CANCELLED,
        (record) => assert.equal(record.cancelledAtMs, 3_000n),
      ],
      [
        0xe3,
        "woken_wait_id",
        WaitStateCode.WOKEN,
        (record) => {
          assert.equal(record.wokenAtMs, 2_000n);
          assert.equal(record.wokenUpToSequence, 2n);
        },
      ],
      [0xe4, "pending_wait_id", WaitStateCode.PENDING, () => undefined],
    ];
    for (const [seed, field, state, extra] of inspectCases) {
      const inspect = buildExchange({
        requestSeed: seed,
        method: "inspect_wait",
        payload: inspectPayload(bytes(fixture, field)),
      });
      const inspected = await exchange(socket, inspect);
      assertSuccess(inspected, seed, false);
      const inspectResult = fromBinary(InspectWaitResultSchema, inspected.envelope!.payload);
      assert.equal(inspectResult.record?.state, state);
      extra(inspectResult.record!);
    }

    // 5. The preset pending row is cancellable: this is what lets the server
    // postcheck prove the client script really committed.
    const cancel = buildExchange({
      requestSeed: 0xe5,
      method: "cancel_wait",
      idempotencyKey: filled(0xf1),
      payload: cancelPayload(bytes(fixture, "pending_wait_id"), 0xf1),
    });
    const cancelled = await exchange(socket, cancel);
    assertSuccess(cancelled, 0xe5, true);
    const cancelResult = fromBinary(CancelWaitResultSchema, cancelled.envelope!.payload);
    assert.equal(cancelResult.replayed, false);
    assert.equal(cancelResult.record?.state, WaitStateCode.CANCELLED);

    assert.equal(await waitForExit(server), 0);
  } finally {
    if (server !== undefined) {
      await stopServer(server);
    }
    await Promise.all([
      rm(socket, { force: true }),
      rm(authorityRoot, { recursive: true, force: true }),
    ]);
  }
}

/// A restarted server on the same authority root replays the original
/// registration and still enumerates both canonical rows.
async function runRestartScenario(
  authorityRoot: string,
  register1: ExchangeRequest,
  record1: WaitRecord,
): Promise<void> {
  const socket = endpoint("ts-restart");
  let server: ChildProcessWithoutNullStreams | undefined;
  try {
    const started = startServer(socket, authorityRoot, { [CONNECTIONS_ENV]: "2" });
    server = started.server;
    await started.fixture;

    const replayed = await exchange(socket, register1);
    assertSuccess(replayed, 0xd1, true);
    const replayResult = fromBinary(RegisterWaitResultSchema, replayed.envelope!.payload);
    assert.equal(replayResult.replayed, true);
    // The replay resolves the same durable row (identical id/binding/target),
    // and its state reflects every later transition: after the fresh script
    // it surfaces as WOKEN, not as the original PENDING snapshot.
    assert.ok(replayResult.record);
    assert.deepEqual(
      Uint8Array.from(replayResult.record.waitId),
      Uint8Array.from(record1.waitId),
    );
    assert.deepEqual(
      Uint8Array.from(replayResult.record.binding),
      Uint8Array.from(record1.binding),
    );
    assert.deepEqual(
      Uint8Array.from(replayResult.record.channelId),
      Uint8Array.from(record1.channelId),
    );
    assert.equal(replayResult.record.targetSequence, record1.targetSequence);
    assert.equal(replayResult.record.state, WaitStateCode.WOKEN);
    assert.equal(replayResult.record.wokenUpToSequence, 5n);

    const list = buildExchange({
      requestSeed: 0xdd,
      method: "list_waits",
      payload: listPayload(),
    });
    const listed = await exchange(socket, list);
    assertSuccess(listed, 0xdd, false);
    const listResult = fromBinary(ListWaitsResultSchema, listed.envelope!.payload);
    assert.equal(listResult.waits.length, 2);
    assert.equal(listResult.waits[0].state, WaitStateCode.WOKEN);
    assert.equal(listResult.waits[1].state, WaitStateCode.CANCELLED);

    assert.equal(await waitForExit(server), 0);
  } finally {
    if (server !== undefined) {
      await stopServer(server);
    }
    await rm(socket, { force: true });
  }
}

const authorityRoot = join(tmpdir(), `nlos-wait-${process.pid}-${Date.now()}-root`);
try {
  const fresh = await runFreshScenario(authorityRoot);
  await runMixedScenario();
  await runRestartScenario(authorityRoot, fresh.register1, fresh.record1);
} finally {
  await rm(authorityRoot, { recursive: true, force: true });
}
