import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";

import {
  CommonSemanticsError,
  validateRequestContext,
  validateResponseContext,
} from "../../../sdk/typescript/src/common.ts";
import {
  EnvelopeSchema,
  ExchangeRequestSchema,
  ExchangeResponseSchema,
  LocalRpcService,
  ReceiptReferenceSchema,
  RetryDirective,
  SabiErrorCode,
  SchemaIdentitySchema,
  type Envelope,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import {
  LocalTransportKind,
  ResolveServiceRequestSchema,
} from "../../../gen/typescript/nlos/sabi/v1/service_directory_pb.ts";
import {
  ArtifactRecoveryAlertStatusSchema,
  ArtifactRecoveryMetricsSchema,
  ArtifactRecoveryOperationsSnapshotSchema,
  RecoveryFailureAuthority,
  RecoveryFailureSummarySchema,
  RecoveryWorkerLifecycleState,
} from "../../../gen/typescript/nlos/sabi/v1/system_control_pb.ts";

const schemaName = "nlos.sabi.Envelope";
const goldenPath = fileURLToPath(
  new URL("../../../schema/golden/nlos.sabi.Envelope-v1.hex", import.meta.url),
);
const goldenHex = readFileSync(goldenPath, "utf8").trim();
assert.equal(goldenHex.length % 2, 0);
const golden = Uint8Array.from(
  Array.from({ length: goldenHex.length / 2 }, (_, index) =>
    Number.parseInt(goldenHex.slice(index * 2, index * 2 + 2), 16),
  ),
);

function validate(envelope: Envelope): void {
  const { schema } = envelope;
  assert.ok(schema, "schema identity is required");
  assert.equal(schema.name, schemaName);
  assert.equal(schema.major, 1, "unknown major must fail closed");
  assert.deepEqual(
    schema.criticalExtensionIds,
    [],
    "unknown critical extensions must fail closed",
  );
  assert.equal(envelope.requestId.length, 16);
  assert.notEqual(envelope.service, "");
  assert.notEqual(envelope.method, "");
}

const decoded = fromBinary(EnvelopeSchema, golden);
validate(decoded);
assert.equal(decoded.schema?.minor, 0);
assert.deepEqual(decoded.schema?.nonCriticalExtensionIds, [42]);
assert.equal(decoded.service, "operation");
assert.equal(decoded.method, "get");
assert.equal(new TextDecoder().decode(decoded.payload), "abc");
assert.deepEqual(toBinary(EnvelopeSchema, decoded), golden);

const compatible = fromBinary(EnvelopeSchema, golden);
compatible.schema!.minor = 99;
compatible.schema!.nonCriticalExtensionIds.push(7_001);
validate(compatible);

const wrongMajor = fromBinary(EnvelopeSchema, golden);
wrongMajor.schema!.major = 2;
assert.throws(() => validate(wrongMajor), /unknown major/);

const unknownCritical = fromBinary(EnvelopeSchema, golden);
unknownCritical.schema!.criticalExtensionIds.push(7_001);
assert.throws(() => validate(unknownCritical), /unknown critical/);

const withUnknownField = new Uint8Array([...golden, 0xa0, 0x06, 0x07]);
assert.deepEqual(
  toBinary(EnvelopeSchema, fromBinary(EnvelopeSchema, withUnknownField)),
  withUnknownField,
);

assert.equal(LocalRpcService.typeName, "nlos.sabi.v1.LocalRpcService");
assert.equal(LocalRpcService.method.exchange.methodKind, "unary");
assert.equal(
  LocalRpcService.method.exchange.input.typeName,
  ExchangeRequestSchema.typeName,
);
assert.equal(
  LocalRpcService.method.exchange.output.typeName,
  ExchangeResponseSchema.typeName,
);

const directoryGoldenPath = fileURLToPath(
  new URL(
    "../../../schema/golden/nlos.sabi.ServiceDirectory.ResolveRequest-v1.hex",
    import.meta.url,
  ),
);
const directoryGolden = Uint8Array.from(
  Buffer.from(readFileSync(directoryGoldenPath, "utf8").trim(), "hex"),
);
const resolveRequest = fromBinary(ResolveServiceRequestSchema, directoryGolden);
assert.equal(resolveRequest.schema?.name, "nlos.sabi.ServiceDirectory");
assert.equal(resolveRequest.schema?.major, 1);
assert.equal(resolveRequest.service, "operation");
assert.deepEqual(
  toBinary(ResolveServiceRequestSchema, resolveRequest),
  directoryGolden,
);
assert.equal(LocalTransportKind.UNIX_SOCKET, 1);
assert.equal(LocalTransportKind.WINDOWS_NAMED_PIPE, 2);

const commonRequestGolden = Uint8Array.from(
  Buffer.from(
    readFileSync(
      fileURLToPath(
        new URL(
          "../../../schema/golden/nlos.sabi.Envelope-common-request-v1.hex",
          import.meta.url,
        ),
      ),
      "utf8",
    ).trim(),
    "hex",
  ),
);
const commonRequest = fromBinary(EnvelopeSchema, commonRequestGolden);
const requestContext = validateRequestContext(
  commonRequest,
  { sideEffecting: true, longRunning: true },
  123_455n,
);
assert.equal(commonRequest.schema?.minor, 1);
assert.equal(requestContext.caller?.processGeneration, 7n);
assert.deepEqual(requestContext.idempotencyKey, new Uint8Array(16).fill(6));
assert.deepEqual(toBinary(EnvelopeSchema, commonRequest), commonRequestGolden);

requestContext.idempotencyKey = new Uint8Array();
assert.throws(
  () =>
    validateRequestContext(
      commonRequest,
      { sideEffecting: true, longRunning: false },
      0n,
    ),
  (error: unknown) =>
    error instanceof CommonSemanticsError &&
    error.code === "MISSING_IDEMPOTENCY_KEY",
);

const uncertainGolden = Uint8Array.from(
  Buffer.from(
    readFileSync(
      fileURLToPath(
        new URL(
          "../../../schema/golden/nlos.sabi.Envelope-common-uncertain-v1.hex",
          import.meta.url,
        ),
      ),
      "utf8",
    ).trim(),
    "hex",
  ),
);
const uncertain = fromBinary(EnvelopeSchema, uncertainGolden);
const responseContext = validateResponseContext(uncertain, {
  sideEffecting: true,
  longRunning: true,
});
assert.equal(responseContext.operation?.generation, 4n);
assert.equal(responseContext.failure?.code, 13);
assert.equal(responseContext.failure?.retry, 3);
assert.deepEqual(toBinary(EnvelopeSchema, uncertain), uncertainGolden);

const terminalRejection = fromBinary(
  EnvelopeSchema,
  toBinary(EnvelopeSchema, uncertain),
);
assert.equal(terminalRejection.commonContext.case, "responseContext");
if (terminalRejection.commonContext.case !== "responseContext") {
  throw new Error("terminal rejection must carry a response context");
}
terminalRejection.commonContext.value.operation = undefined;
terminalRejection.commonContext.value.receipts = [];
assert.ok(terminalRejection.commonContext.value.failure);
terminalRejection.commonContext.value.failure.code = SabiErrorCode.RIGHTS;
terminalRejection.commonContext.value.failure.retry = RetryDirective.DO_NOT_RETRY;
terminalRejection.commonContext.value.failure.safeMessage = "authorization denied";
validateResponseContext(terminalRejection, {
  sideEffecting: true,
  longRunning: false,
});

responseContext.failure!.retry = 2;
assert.throws(
  () =>
    validateResponseContext(uncertain, {
      sideEffecting: true,
      longRunning: true,
    }),
  (error: unknown) =>
    error instanceof CommonSemanticsError && error.code === "UNSAFE_RETRY",
);

const artifactRecoverySnapshotGoldenHex =
  "0a1b0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001125708" +
  "0310111817200b280230f40338044003480150095a140a101111111111111111" +
  "111111111111111110015a140a10222222222222222222222222222222221002" +
  "5a140a106666666666666666666666666666666610031a330a10333333333333" +
  "333333333333333333331004180320e80728b00930940a3a120a104444444444" +
  "44444444444444444444441a1f0a105555555555555555555555555555555510" +
  "01180420d00f28b4103098112001";
const artifactRecoverySnapshotGolden = Uint8Array.from(
  Buffer.from(artifactRecoverySnapshotGoldenHex, "hex"),
);

const artifactRecoverySnapshot = create(
  ArtifactRecoveryOperationsSnapshotSchema,
  {
    schema: create(SchemaIdentitySchema, {
      name: "nlos.sabi.SystemControl",
      major: 1,
      minor: 0,
      criticalExtensionIds: [],
      nonCriticalExtensionIds: [],
    }),
    metrics: create(ArtifactRecoveryMetricsSchema, {
      workerState: RecoveryWorkerLifecycleState.BACKING_OFF,
      completedCycles: 17n,
      totalInspected: 23n,
      totalFinalized: 11n,
      consecutiveFailedCycles: 2n,
      retryDelayMs: 500n,
      durableRetrying: 4n,
      durableEscalated: 3n,
      durableUnacknowledgedEscalated: 1n,
      durableResolved: 9n,
      lastFailures: [
        create(RecoveryFailureSummarySchema, {
          planId: new Uint8Array(16).fill(0x11),
          authority: RecoveryFailureAuthority.TASK,
        }),
        create(RecoveryFailureSummarySchema, {
          planId: new Uint8Array(16).fill(0x22),
          authority: RecoveryFailureAuthority.ARTIFACT,
        }),
        create(RecoveryFailureSummarySchema, {
          planId: new Uint8Array(16).fill(0x66),
          authority: RecoveryFailureAuthority.COORDINATOR,
        }),
      ],
    }),
    alerts: [
      create(ArtifactRecoveryAlertStatusSchema, {
        planId: new Uint8Array(16).fill(0x33),
        totalFailures: 4n,
        lastFailureAuthority: RecoveryFailureAuthority.COORDINATOR,
        firstFailedAtMs: 1000n,
        lastFailedAtMs: 1200n,
        escalatedAtMs: 1300n,
        acknowledgementReceipt: create(ReceiptReferenceSchema, {
          receiptId: new Uint8Array(16).fill(0x44),
        }),
      }),
      create(ArtifactRecoveryAlertStatusSchema, {
        planId: new Uint8Array(16).fill(0x55),
        totalFailures: 1n,
        lastFailureAuthority: RecoveryFailureAuthority.WORKER,
        firstFailedAtMs: 2000n,
        lastFailedAtMs: 2100n,
        escalatedAtMs: 2200n,
      }),
    ],
    alertsTruncated: true,
  },
);
assert.deepEqual(
  toBinary(ArtifactRecoveryOperationsSnapshotSchema, artifactRecoverySnapshot),
  artifactRecoverySnapshotGolden,
);

const decodedArtifactRecoverySnapshot = fromBinary(
  ArtifactRecoveryOperationsSnapshotSchema,
  artifactRecoverySnapshotGolden,
);
assert.equal(decodedArtifactRecoverySnapshot.schema?.name, "nlos.sabi.SystemControl");
assert.equal(
  decodedArtifactRecoverySnapshot.metrics?.workerState,
  RecoveryWorkerLifecycleState.BACKING_OFF,
);
assert.equal(decodedArtifactRecoverySnapshot.metrics?.retryDelayMs, 500n);
assert.deepEqual(
  decodedArtifactRecoverySnapshot.metrics?.lastFailures.map(
    (failure) => failure.authority,
  ),
  [
    RecoveryFailureAuthority.TASK,
    RecoveryFailureAuthority.ARTIFACT,
    RecoveryFailureAuthority.COORDINATOR,
  ],
);
assert.deepEqual(
  decodedArtifactRecoverySnapshot.metrics?.lastFailures[2]?.planId,
  new Uint8Array(16).fill(0x66),
);
assert.ok(decodedArtifactRecoverySnapshot.alerts[0]?.acknowledgementReceipt);
assert.equal(
  decodedArtifactRecoverySnapshot.alerts[1]?.acknowledgementReceipt,
  undefined,
);
assert.equal(decodedArtifactRecoverySnapshot.alertsTruncated, true);
assert.deepEqual(
  toBinary(
    ArtifactRecoveryOperationsSnapshotSchema,
    decodedArtifactRecoverySnapshot,
  ),
  artifactRecoverySnapshotGolden,
);

const withoutRetryDelay = fromBinary(
  ArtifactRecoveryOperationsSnapshotSchema,
  artifactRecoverySnapshotGolden,
);
withoutRetryDelay.metrics!.retryDelayMs = undefined;
assert.equal(withoutRetryDelay.metrics?.retryDelayMs, undefined);
assert.notDeepEqual(
  toBinary(ArtifactRecoveryOperationsSnapshotSchema, withoutRetryDelay),
  artifactRecoverySnapshotGolden,
);
