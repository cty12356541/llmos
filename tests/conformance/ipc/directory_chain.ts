import assert from "node:assert/strict";
import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";

import {
  CallerIdentitySchema,
  CapabilityHandleSchema,
  EnvelopeSchema,
  ExchangeRequestSchema,
  OperationReferenceSchema,
  RetryDirective,
  SabiRequestContextSchema,
  SabiErrorCode,
  SchemaIdentitySchema,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import {
  CancelOperationRequestSchema,
  OperationLifecycleState,
  OperationStatusSchema,
  QueryOperationRequestSchema,
} from "../../../gen/typescript/nlos/sabi/v1/operation_control_pb.ts";
import { validateResponseContext } from "../../../sdk/typescript/src/common.ts";
import {
  IpcError,
  LocalRpcClient,
} from "../../../sdk/typescript/src/local_rpc.ts";
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
  authorityPath: string,
  phase: "commit" | "recover",
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
      authorityPath,
      phase,
    ],
    { stdio: ["pipe", "pipe", "pipe"] },
  );
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("directory chain server did not become ready")),
      60_000,
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

function waitForExit(
  server: ChildProcessWithoutNullStreams,
): Promise<number | null> {
  if (server.exitCode !== null || server.signalCode !== null) {
    return Promise.resolve(server.exitCode);
  }
  return new Promise((resolve, reject) => {
    server.once("exit", resolve);
    server.once("error", reject);
  });
}

function businessRequest(
  requestSeed: number,
  correlationSeed: number,
  method = "cancel",
  keySeed = 6,
  payload: readonly number[] = [4, 5, 6],
) {
  return create(ExchangeRequestSchema, {
    envelope: create(EnvelopeSchema, {
      schema: create(SchemaIdentitySchema, {
        name: "nlos.sabi.Envelope",
        major: 1,
        minor: 1,
      }),
      requestId: new Uint8Array(16).fill(requestSeed),
      service: "operation",
      method,
      commonContext: {
        case: "requestContext",
        value: create(SabiRequestContextSchema, {
          caller: create(CallerIdentitySchema, {
            principalId: new Uint8Array(16).fill(1),
            applicationId: new Uint8Array(16).fill(2),
            processId: new Uint8Array(16).fill(3),
            processGeneration: 7n,
          }),
          correlationId: new Uint8Array(16).fill(correlationSeed),
          idempotencyKey: new Uint8Array(16).fill(keySeed),
          deadlineMonotonicNs: 123_456n,
          capabilityHandles: [
            create(CapabilityHandleSchema, { slot: 11n, generation: 2n }),
          ],
        }),
      },
      payload: new Uint8Array(payload),
    }),
  });
}

function operationControlRequest(
  requestSeed: number,
  correlationSeed: number,
  method: "query" | "cancel",
  operationId: Uint8Array,
  generation: bigint,
  expectedCancelEpoch = 0n,
) {
  const operation = create(OperationReferenceSchema, {
    operationId,
    generation,
  });
  const payload =
    method === "query"
      ? toBinary(
          QueryOperationRequestSchema,
          create(QueryOperationRequestSchema, {
            schema: create(SchemaIdentitySchema, {
              name: "nlos.sabi.OperationControl",
              major: 1,
              minor: 0,
            }),
            operation,
          }),
        )
      : toBinary(
          CancelOperationRequestSchema,
          create(CancelOperationRequestSchema, {
            schema: create(SchemaIdentitySchema, {
              name: "nlos.sabi.OperationControl",
              major: 1,
              minor: 0,
            }),
            operation,
            expectedCancelEpoch,
          }),
        );
  return create(ExchangeRequestSchema, {
    envelope: create(EnvelopeSchema, {
      schema: create(SchemaIdentitySchema, {
        name: "nlos.sabi.Envelope",
        major: 1,
        minor: 1,
      }),
      requestId: new Uint8Array(16).fill(requestSeed),
      service: "operation_control",
      method,
      commonContext: {
        case: "requestContext",
        value: create(SabiRequestContextSchema, {
          caller: create(CallerIdentitySchema, {
            principalId: new Uint8Array(16).fill(1),
            applicationId: new Uint8Array(16).fill(2),
            processId: new Uint8Array(16).fill(3),
            processGeneration: 7n,
          }),
          correlationId: new Uint8Array(16).fill(correlationSeed),
          idempotencyKey:
            method === "cancel" ? new Uint8Array(16).fill(0x71) : new Uint8Array(),
          capabilityHandles: [
            create(CapabilityHandleSchema, { slot: 11n, generation: 2n }),
          ],
        }),
      },
      payload,
    }),
  });
}

function operationStatus(responsePayload: Uint8Array | undefined) {
  assert.ok(responsePayload);
  return fromBinary(OperationStatusSchema, responsePayload);
}

const directoryEndpoint = endpoint("bootstrap");
const businessEndpoint = endpoint("business");
const authorityPath = join(
  tmpdir(),
  `nlos-directory-${process.pid}-${Date.now()}-authority.sqlite3`,
);
let server = await startServer(
  directoryEndpoint,
  businessEndpoint,
  authorityPath,
  "commit",
);
try {
  const transportConfig = {
    connectTimeoutMs: 2_000,
    readTimeoutMs: 2_000,
    writeTimeoutMs: 2_000,
  };
  const connected = await ServiceDirectoryClient.negotiateAndConnect(
    directoryEndpoint,
    {
      service: "operation",
      schemaName: "nlos.sabi.Envelope",
      major: 1,
      minimumMinor: 1,
    },
    transportConfig,
  );
  assert.equal(connected.binding.endpoint?.address, businessEndpoint);
  assert.equal(connected.binding.candidate?.generation, 7n);

  await assert.rejects(
    connected.client.exchange(businessRequest(9, 5)),
    (error: unknown) => error instanceof IpcError && error.code === "READ",
  );
  connected.client.close();

  const commitCode = await waitForExit(server);
  assert.equal(commitCode, 0);

  server = await startServer(
    directoryEndpoint,
    businessEndpoint,
    authorityPath,
    "recover",
  );
  const recovered = await ServiceDirectoryClient.negotiateAndConnect(
    directoryEndpoint,
    {
      service: "operation",
      schemaName: "nlos.sabi.Envelope",
      major: 1,
      minimumMinor: 1,
    },
    transportConfig,
  );
  assert.equal(recovered.binding.endpoint?.address, businessEndpoint);
  const retryClient = recovered.client;
  const response = await retryClient.exchange(businessRequest(10, 7));
  assert.deepEqual(
    Uint8Array.from(response.envelope?.requestId ?? []),
    new Uint8Array(16).fill(10),
  );
  assert.deepEqual(
    Uint8Array.from(response.envelope?.payload ?? []),
    new Uint8Array([4, 5, 6, 0xd0]),
  );
  assert.ok(response.envelope);
  const responseContext = validateResponseContext(response.envelope, {
    sideEffecting: true,
    longRunning: true,
  });
  assert.deepEqual(
    Uint8Array.from(responseContext.correlationId),
    new Uint8Array(16).fill(7),
  );
  assert.equal(responseContext.operation?.generation, 1n);
  assert.deepEqual(
    Uint8Array.from(responseContext.receipts[0]?.receiptId ?? []),
    new Uint8Array(16).fill(0x99),
  );
  retryClient.close();

  const conflictClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const conflict = await conflictClient.exchange(
    businessRequest(11, 8, "cancel", 6, [4, 5, 7]),
  );
  assert.ok(conflict.envelope);
  const conflictContext = validateResponseContext(conflict.envelope, {
    sideEffecting: true,
    longRunning: true,
  });
  assert.equal(conflictContext.failure?.code, SabiErrorCode.CONFLICT);
  assert.equal(conflictContext.failure?.retry, RetryDirective.DO_NOT_RETRY);
  assert.deepEqual(
    Uint8Array.from(conflictContext.operation?.operationId ?? []),
    Uint8Array.from(responseContext.operation?.operationId ?? []),
  );
  conflictClient.close();

  const pendingClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const pending = await pendingClient.exchange(
    businessRequest(12, 9, "pending", 7, [1]),
  );
  assert.ok(pending.envelope);
  const pendingContext = validateResponseContext(pending.envelope, {
    sideEffecting: true,
    longRunning: true,
  });
  assert.equal(pendingContext.failure?.code, SabiErrorCode.UNCERTAIN);
  assert.equal(
    pendingContext.failure?.retry,
    RetryDirective.QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY,
  );
  assert.ok(pendingContext.operation);
  pendingClient.close();

  const pendingOperation = pendingContext.operation;
  const queryPendingClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const queriedPending = await queryPendingClient.exchange(
    operationControlRequest(
      18,
      15,
      "query",
      pendingOperation.operationId,
      pendingOperation.generation,
    ),
  );
  assert.ok(queriedPending.envelope);
  validateResponseContext(queriedPending.envelope, {
    sideEffecting: false,
    longRunning: false,
  });
  const queriedPendingStatus = operationStatus(queriedPending.envelope.payload);
  assert.equal(queriedPendingStatus.state, OperationLifecycleState.DISPATCHED);
  assert.equal(queriedPendingStatus.cancelEpoch, 0n);
  queryPendingClient.close();

  const cancelControlClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const cancelledPending = await cancelControlClient.exchange(
    operationControlRequest(
      19,
      16,
      "cancel",
      pendingOperation.operationId,
      pendingOperation.generation,
    ),
  );
  assert.ok(cancelledPending.envelope);
  validateResponseContext(cancelledPending.envelope, {
    sideEffecting: true,
    longRunning: false,
  });
  const cancelledPendingStatus = operationStatus(
    cancelledPending.envelope.payload,
  );
  assert.equal(
    cancelledPendingStatus.state,
    OperationLifecycleState.CANCEL_REQUESTED,
  );
  assert.equal(cancelledPendingStatus.cancelEpoch, 1n);
  cancelControlClient.close();

  const cancelReplayClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const cancelReplay = await cancelReplayClient.exchange(
    operationControlRequest(
      20,
      17,
      "cancel",
      pendingOperation.operationId,
      pendingOperation.generation,
    ),
  );
  assert.ok(cancelReplay.envelope);
  const cancelReplayStatus = operationStatus(cancelReplay.envelope.payload);
  assert.equal(cancelReplayStatus.cancelEpoch, 1n);
  assert.equal(
    cancelReplayStatus.state,
    OperationLifecycleState.CANCEL_REQUESTED,
  );
  cancelReplayClient.close();

  const queryCancelledClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const queryCancelled = await queryCancelledClient.exchange(
    operationControlRequest(
      21,
      18,
      "query",
      pendingOperation.operationId,
      pendingOperation.generation,
    ),
  );
  assert.ok(queryCancelled.envelope);
  const queryCancelledStatus = operationStatus(queryCancelled.envelope.payload);
  assert.equal(queryCancelledStatus.cancelEpoch, 1n);
  assert.equal(
    queryCancelledStatus.state,
    OperationLifecycleState.CANCEL_REQUESTED,
  );
  queryCancelledClient.close();

  const workerDeadlineClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const workerDeadline = await workerDeadlineClient.exchange(
    businessRequest(22, 19, "worker_deadline", 12, [6]),
  );
  assert.ok(workerDeadline.envelope);
  const workerDeadlineContext = validateResponseContext(workerDeadline.envelope, {
    sideEffecting: true,
    longRunning: true,
  });
  assert.equal(workerDeadlineContext.failure?.code, SabiErrorCode.UNCERTAIN);
  assert.ok(workerDeadlineContext.operation);
  workerDeadlineClient.close();

  const workerOperation = workerDeadlineContext.operation;
  const queryQueuedClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const queryQueued = await queryQueuedClient.exchange(
    operationControlRequest(
      23,
      20,
      "query",
      workerOperation.operationId,
      workerOperation.generation,
    ),
  );
  assert.ok(queryQueued.envelope);
  const queryQueuedStatus = operationStatus(queryQueued.envelope.payload);
  assert.equal(queryQueuedStatus.state, OperationLifecycleState.REGISTERED);
  assert.equal(queryQueuedStatus.cancelEpoch, 0n);
  queryQueuedClient.close();

  await new Promise((resolve) => setTimeout(resolve, 650));
  const queryDeadlineClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const queryDeadline = await queryDeadlineClient.exchange(
    operationControlRequest(
      24,
      21,
      "query",
      workerOperation.operationId,
      workerOperation.generation,
    ),
  );
  assert.ok(queryDeadline.envelope);
  const queryDeadlineStatus = operationStatus(queryDeadline.envelope.payload);
  assert.equal(
    queryDeadlineStatus.state,
    OperationLifecycleState.CANCELLED_BEFORE_EFFECT,
  );
  assert.equal(queryDeadlineStatus.cancelEpoch, 1n);
  assert.deepEqual(
    Uint8Array.from(queryDeadlineStatus.receipt?.receiptId ?? []),
    new Uint8Array(16).fill(0xa7),
  );
  queryDeadlineClient.close();

  const deadlineBeforeClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const deadlineBefore = await deadlineBeforeClient.exchange(
    businessRequest(13, 10, "deadline_before_dispatch", 8, [2]),
  );
  assert.ok(deadlineBefore.envelope);
  const deadlineBeforeContext = validateResponseContext(deadlineBefore.envelope, {
    sideEffecting: true,
    longRunning: true,
  });
  assert.equal(deadlineBeforeContext.failure?.code, SabiErrorCode.DEADLINE);
  assert.equal(
    deadlineBeforeContext.failure?.retry,
    RetryDirective.DO_NOT_RETRY,
  );
  assert.deepEqual(
    Uint8Array.from(deadlineBeforeContext.receipts[0]?.receiptId ?? []),
    new Uint8Array(16).fill(0xa1),
  );
  deadlineBeforeClient.close();

  const deadlineReplayClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const deadlineReplay = await deadlineReplayClient.exchange(
    businessRequest(14, 11, "deadline_before_dispatch", 8, [2]),
  );
  assert.ok(deadlineReplay.envelope);
  const deadlineReplayContext = validateResponseContext(deadlineReplay.envelope, {
    sideEffecting: true,
    longRunning: true,
  });
  assert.equal(deadlineReplayContext.failure?.code, SabiErrorCode.DEADLINE);
  assert.deepEqual(
    Uint8Array.from(deadlineReplayContext.operation?.operationId ?? []),
    Uint8Array.from(deadlineBeforeContext.operation?.operationId ?? []),
  );
  assert.deepEqual(
    Uint8Array.from(deadlineReplayContext.correlationId),
    new Uint8Array(16).fill(11),
  );
  deadlineReplayClient.close();

  const cancelBeforeClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const cancelBefore = await cancelBeforeClient.exchange(
    businessRequest(15, 12, "cancel_before_dispatch", 9, [3]),
  );
  assert.ok(cancelBefore.envelope);
  const cancelBeforeContext = validateResponseContext(cancelBefore.envelope, {
    sideEffecting: true,
    longRunning: true,
  });
  assert.equal(cancelBeforeContext.failure?.code, SabiErrorCode.CANCELLED);
  assert.equal(cancelBeforeContext.failure?.retry, RetryDirective.DO_NOT_RETRY);
  assert.deepEqual(
    Uint8Array.from(cancelBeforeContext.receipts[0]?.receiptId ?? []),
    new Uint8Array(16).fill(0xa2),
  );
  cancelBeforeClient.close();

  const cancelAfterClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const cancelAfter = await cancelAfterClient.exchange(
    businessRequest(16, 13, "cancel_after_dispatch", 10, [4]),
  );
  assert.ok(cancelAfter.envelope);
  const cancelAfterContext = validateResponseContext(cancelAfter.envelope, {
    sideEffecting: true,
    longRunning: true,
  });
  assert.equal(cancelAfterContext.failure?.code, SabiErrorCode.PARTIAL);
  assert.equal(cancelAfterContext.failure?.retry, RetryDirective.DO_NOT_RETRY);
  assert.deepEqual(
    Uint8Array.from(cancelAfterContext.receipts[0]?.receiptId ?? []),
    new Uint8Array(16).fill(0xa4),
  );
  cancelAfterClient.close();

  const deadlineAfterClient = await LocalRpcClient.connect(
    businessEndpoint,
    transportConfig,
  );
  const deadlineAfter = await deadlineAfterClient.exchange(
    businessRequest(17, 14, "deadline_after_dispatch", 11, [5]),
  );
  assert.ok(deadlineAfter.envelope);
  const deadlineAfterContext = validateResponseContext(deadlineAfter.envelope, {
    sideEffecting: true,
    longRunning: true,
  });
  assert.equal(
    deadlineAfterContext.failure?.code,
    SabiErrorCode.EFFECT_UNKNOWN,
  );
  assert.equal(
    deadlineAfterContext.failure?.retry,
    RetryDirective.QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY,
  );
  assert.deepEqual(
    Uint8Array.from(deadlineAfterContext.receipts[0]?.receiptId ?? []),
    new Uint8Array(16).fill(0xa6),
  );
  deadlineAfterClient.close();

  const code = await waitForExit(server);
  assert.equal(code, 0);
} catch (error) {
  server.kill();
  throw error;
}
