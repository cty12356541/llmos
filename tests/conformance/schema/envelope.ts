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
  AcknowledgeResourceRecoveryAlertCommandSchema,
  AcknowledgeSemanticRecoveryAlertCommandSchema,
  ArtifactRecoveryAlertStatusSchema,
  ArtifactRecoveryMetricsSchema,
  ArtifactRecoveryOperationsSnapshotSchema,
  CancelCommandSchema,
  ContextResidencyTier,
  ControlCommandSchema,
  ControlCommandSource,
  ControlScope,
  DurableOperationSnapshotSchema,
  DurableOperationState,
  DurableOperationStatusSchema,
  ExecutionFiberLifecycleState,
  ExecutionFiberOperationsSnapshotSchema,
  ExecutionFiberPhase,
  ExecutionFiberStatusSchema,
  GetSystemControlRequestSchema,
  KillCommandSchema,
  PauseCommandSchema,
  PlanNodeKind,
  PlanNodeLifecycleState,
  ReclaimCommandSchema,
  RecoveryFailureAuthority,
  RecoveryFailureSummarySchema,
  RecoveryWorkerLifecycleState,
  ResourceRecoveryAlertStatusSchema,
  ResourceRecoveryMetricsSchema,
  ResourceRecoveryOperationsSnapshotSchema,
  ResumeCommandSchema,
  ResumeResourceRecoveryCommandSchema,
  ResumeSemanticRecoveryCommandSchema,
  SemanticRecoveryAlertStatusSchema,
  SemanticRecoveryMetricsSchema,
  SemanticRecoveryOperationsSnapshotSchema,
  SubmitControlCommandRequestSchema,
  SystemControlView,
  TaskGroupLifecycleState,
  TaskGroupMemberStatusSchema,
  TaskGroupMemberType,
  TaskGroupMembershipState,
  TaskGroupOperationsSnapshotSchema,
  TaskGroupStatusSchema,
  TaskNodeOperationsSnapshotSchema,
  TaskNodeStatusSchema,
  ThrottleCommandSchema,
  TopicOperationsSnapshotSchema,
  TopicStatusSchema,
  type ControlCommand,
  type SubmitControlCommandRequest,
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

const semanticRecoverySnapshotGoldenHex =
  "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c10011801" +
  "1210080d10061802200428033001380940011a330a10717171717171717171" +
  "717171717171711008180520e80728f80a30dc0b3a120a1072727272727272" +
  "7272727272727272721a1f0a10737373737373737373737373737373731008" +
  "180320d00f28e01230c4132001";
const semanticRecoverySnapshotGolden = Uint8Array.from(
  Buffer.from(semanticRecoverySnapshotGoldenHex, "hex"),
);

const semanticRecoverySnapshot = create(
  SemanticRecoveryOperationsSnapshotSchema,
  {
    schema: create(SchemaIdentitySchema, {
      name: "nlos.sabi.SystemControl",
      major: 1,
      minor: 1,
      criticalExtensionIds: [],
      nonCriticalExtensionIds: [],
    }),
    metrics: create(SemanticRecoveryMetricsSchema, {
      totalInspected: 13n,
      totalFinalized: 6n,
      consecutiveFailedCycles: 2n,
      durableRetrying: 4n,
      durableEscalated: 3n,
      durableUnacknowledgedEscalated: 1n,
      durableResolved: 9n,
      domainFaulted: true,
    }),
    alerts: [
      create(SemanticRecoveryAlertStatusSchema, {
        planId: new Uint8Array(16).fill(0x71),
        totalFailures: 8n,
        lastFailureAuthority: RecoveryFailureAuthority.SEMANTIC,
        firstFailedAtMs: 1000n,
        lastFailedAtMs: 1400n,
        escalatedAtMs: 1500n,
        acknowledgementReceipt: create(ReceiptReferenceSchema, {
          receiptId: new Uint8Array(16).fill(0x72),
        }),
      }),
      create(SemanticRecoveryAlertStatusSchema, {
        planId: new Uint8Array(16).fill(0x73),
        totalFailures: 8n,
        lastFailureAuthority: RecoveryFailureAuthority.COORDINATOR,
        firstFailedAtMs: 2000n,
        lastFailedAtMs: 2400n,
        escalatedAtMs: 2500n,
      }),
    ],
    alertsTruncated: true,
  },
);
assert.deepEqual(
  toBinary(SemanticRecoveryOperationsSnapshotSchema, semanticRecoverySnapshot),
  semanticRecoverySnapshotGolden,
);

const decodedSemanticRecoverySnapshot = fromBinary(
  SemanticRecoveryOperationsSnapshotSchema,
  semanticRecoverySnapshotGolden,
);
assert.equal(decodedSemanticRecoverySnapshot.schema?.minor, 1);
assert.equal(decodedSemanticRecoverySnapshot.metrics?.totalInspected, 13n);
assert.equal(decodedSemanticRecoverySnapshot.metrics?.domainFaulted, true);
assert.equal(
  decodedSemanticRecoverySnapshot.metrics?.durableUnacknowledgedEscalated,
  1n,
);
assert.equal(
  decodedSemanticRecoverySnapshot.alerts[0]?.lastFailureAuthority,
  RecoveryFailureAuthority.SEMANTIC,
);
assert.ok(decodedSemanticRecoverySnapshot.alerts[0]?.acknowledgementReceipt);
assert.equal(
  decodedSemanticRecoverySnapshot.alerts[1]?.acknowledgementReceipt,
  undefined,
);
assert.equal(decodedSemanticRecoverySnapshot.alertsTruncated, true);
assert.deepEqual(
  toBinary(
    SemanticRecoveryOperationsSnapshotSchema,
    decodedSemanticRecoverySnapshot,
  ),
  semanticRecoverySnapshotGolden,
);

// ---------------------------------------------------------------------------
// Stage-B handover #13 (W34-A/W29-G deferred minor): pin the TS/Python
// conformance goldens for the SABI v1.2–v1.5 additive SystemControl surface
// against the Rust-derived golden bytes (crates/nlos-schema
// tests/compatibility.rs stays the byte source of truth).
//
// One documented divergence: prost emits ControlCommand fields in proto
// declaration order (oneof arm before `reason` field 8), while protobuf-es
// and the Python runtime emit field-number order (`reason` before arms 9+).
// Both are valid protobuf wire forms (field order is not significant), so
// each command golden is pinned twice: the literal prost-order bytes are the
// decode anchor (decode + re-encode must reach the canonical form), and the
// canonical TS/Python bytes are the encode anchor. The TS and Python
// canonical bytes are identical to each other.
// ---------------------------------------------------------------------------

function hexToBytes(hex: string): Uint8Array {
  return Uint8Array.from(Buffer.from(hex, "hex"));
}

assert.equal(SystemControlView.RESOURCE_COMMIT_RECOVERY, 3);
assert.equal(SystemControlView.TASK_GROUP, 4);
assert.equal(SystemControlView.TASK_NODE, 5);
assert.equal(SystemControlView.EXECUTION_FIBER, 6);
assert.equal(SystemControlView.TOPIC, 7);
assert.equal(SystemControlView.OPERATION, 8);
assert.equal(RecoveryFailureAuthority.RESOURCE, 6);

// Shared hex pieces mirror the Rust constants verbatim: the v1.2 W28-D
// submit prefix (schema identity + ControlCommand addressing through the
// CAS expectation), the v1.3 W29-D head/body split (per-arm command length
// varies), and the shared reason string.
const SUBMIT_PREFIX_V1_2_HEX =
  "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180212" +
  "670a106161616161616161616161616161616112103232323232323232323232" +
  "3232323232180320022a10818181818181818181818181818181813005";
const SUBMIT_HEAD_V1_3_HEX =
  "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180312";
const SUBMIT_BODY_V1_3_HEX =
  "0a106161616161616161616161616161616112103232323232323232323232" +
  "3232323232180320022a10818181818181818181818181818181813005";
const SUBMIT_REASON_HEX =
  "42276f70657261746f72207061757365732074686520657363616c6174656420" +
  "6f7065726174696f6e";

type ControlArm = Exclude<ControlCommand["command"], undefined>;

function operationLevelSubmit(
  minor: number,
  arm: ControlArm,
  controlCommandId: Uint8Array,
): SubmitControlCommandRequest {
  return create(SubmitControlCommandRequestSchema, {
    schema: create(SchemaIdentitySchema, {
      name: "nlos.sabi.SystemControl",
      major: 1,
      minor,
      criticalExtensionIds: [],
      nonCriticalExtensionIds: [],
    }),
    command: create(ControlCommandSchema, {
      controlCommandId,
      issuerPrincipalId: new Uint8Array(16).fill(0x32),
      source: ControlCommandSource.CLI,
      scope: ControlScope.OPERATION,
      targetId: new Uint8Array(16).fill(0x81),
      expectedGenerationOrRevision: 5n,
      command: arm,
      reason: "operator pauses the escalated operation",
    }),
  });
}

// W28-D (v1.2) pause/resume/cancel and W29-D (v1.3) kill/throttle/reclaim
// arms: canonical encode + literal prost-order golden decode/re-encode.
const commandArmCases: {
  label: string;
  minor: number;
  commandLenHex: string;
  armHex: string;
  armCase: ControlArm["case"];
  arm: ControlArm;
}[] = [
  {
    label: "pause",
    minor: 2,
    commandLenHex: "67",
    armHex: "5a00",
    armCase: "pauseOperation",
    arm: { case: "pauseOperation", value: create(PauseCommandSchema, {}) },
  },
  {
    label: "resume",
    minor: 2,
    commandLenHex: "67",
    armHex: "6200",
    armCase: "resumeOperation",
    arm: { case: "resumeOperation", value: create(ResumeCommandSchema, {}) },
  },
  {
    label: "cancel",
    minor: 2,
    commandLenHex: "67",
    armHex: "6a00",
    armCase: "cancelOperation",
    arm: { case: "cancelOperation", value: create(CancelCommandSchema, {}) },
  },
  {
    label: "kill",
    minor: 3,
    commandLenHex: "67",
    armHex: "7200",
    armCase: "killOperation",
    arm: { case: "killOperation", value: create(KillCommandSchema, {}) },
  },
  {
    label: "throttle",
    minor: 3,
    commandLenHex: "69",
    armHex: "7a020832",
    armCase: "throttleOperation",
    arm: {
      case: "throttleOperation",
      value: create(ThrottleCommandSchema, { throttlePercent: 50n }),
    },
  },
  {
    label: "reclaim",
    minor: 3,
    commandLenHex: "68",
    armHex: "820100",
    armCase: "reclaimOperation",
    arm: { case: "reclaimOperation", value: create(ReclaimCommandSchema, {}) },
  },
];

for (const { label, minor, commandLenHex, armHex, armCase, arm } of commandArmCases) {
  const request = operationLevelSubmit(minor, arm, new Uint8Array(16).fill(0x61));
  const prefixHex =
    minor === 2
      ? SUBMIT_PREFIX_V1_2_HEX
      : SUBMIT_HEAD_V1_3_HEX + commandLenHex + SUBMIT_BODY_V1_3_HEX;
  const canonical = hexToBytes(prefixHex + SUBMIT_REASON_HEX + armHex);
  const prostOrder = hexToBytes(prefixHex + armHex + SUBMIT_REASON_HEX);

  assert.deepEqual(
    toBinary(SubmitControlCommandRequestSchema, request),
    canonical,
    `${label} arm must encode to the canonical TS bytes`,
  );
  const decoded = fromBinary(SubmitControlCommandRequestSchema, prostOrder);
  assert.equal(decoded.schema?.minor, minor);
  assert.equal(decoded.command?.command?.case, armCase, `${label} oneof case`);
  assert.equal(
    decoded.command?.command?.case === "throttleOperation"
      ? decoded.command.command.value.throttlePercent
      : undefined,
    armCase === "throttleOperation" ? 50n : undefined,
  );
  assert.equal(
    decoded.command?.reason,
    "operator pauses the escalated operation",
  );
  assert.deepEqual(
    decoded.command?.targetId,
    new Uint8Array(16).fill(0x81),
  );
  assert.equal(decoded.command?.expectedGenerationOrRevision, 5n);
  assert.deepEqual(
    toBinary(SubmitControlCommandRequestSchema, decoded),
    canonical,
    `${label} prost-order golden must decode and re-encode canonically`,
  );
}

// Divergence witness: the prost declaration-order bytes and the canonical
// TS/Python bytes are wire-equivalent but not identical. If a runtime
// upgrade ever makes these converge, this assert forces a conscious re-pin.
assert.notDeepEqual(
  hexToBytes(SUBMIT_PREFIX_V1_2_HEX + "5a00" + SUBMIT_REASON_HEX),
  hexToBytes(SUBMIT_PREFIX_V1_2_HEX + SUBMIT_REASON_HEX + "5a00"),
  "prost declaration order and TS canonical order must stay distinct bytes",
);

// W28-C-3b (v1.4) resource-domain recovery snapshot golden — byte-equal to
// the Rust RESOURCE_RECOVERY_SNAPSHOT_GOLDEN_HEX (no oneof; ascending field
// numbers, so declaration order and field-number order coincide).
const resourceRecoverySnapshotGoldenHex =
  "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c10011804" +
  "1210080f10071802200528043002380b40011a330a10818181818181818181" +
  "818181818181811008180620e80728f80a30dc0b3a120a1082828282828282" +
  "8282828282828282821a1f0a10838383838383838383838383838383831009" +
  "180320b81728c81a30ac1b2001";
const resourceRecoverySnapshotGolden = hexToBytes(resourceRecoverySnapshotGoldenHex);

const resourceRecoverySnapshot = create(
  ResourceRecoveryOperationsSnapshotSchema,
  {
    schema: create(SchemaIdentitySchema, {
      name: "nlos.sabi.SystemControl",
      major: 1,
      minor: 4,
      criticalExtensionIds: [],
      nonCriticalExtensionIds: [],
    }),
    metrics: create(ResourceRecoveryMetricsSchema, {
      totalInspected: 15n,
      totalFinalized: 7n,
      consecutiveFailedCycles: 2n,
      durableRetrying: 5n,
      durableEscalated: 4n,
      durableUnacknowledgedEscalated: 2n,
      durableResolved: 11n,
      domainFaulted: true,
    }),
    alerts: [
      create(ResourceRecoveryAlertStatusSchema, {
        planId: new Uint8Array(16).fill(0x81),
        totalFailures: 8n,
        lastFailureAuthority: RecoveryFailureAuthority.RESOURCE,
        firstFailedAtMs: 1000n,
        lastFailedAtMs: 1400n,
        escalatedAtMs: 1500n,
        acknowledgementReceipt: create(ReceiptReferenceSchema, {
          receiptId: new Uint8Array(16).fill(0x82),
        }),
      }),
      create(ResourceRecoveryAlertStatusSchema, {
        planId: new Uint8Array(16).fill(0x83),
        totalFailures: 9n,
        lastFailureAuthority: RecoveryFailureAuthority.COORDINATOR,
        firstFailedAtMs: 3000n,
        lastFailedAtMs: 3400n,
        escalatedAtMs: 3500n,
      }),
    ],
    alertsTruncated: true,
  },
);
assert.deepEqual(
  toBinary(ResourceRecoveryOperationsSnapshotSchema, resourceRecoverySnapshot),
  resourceRecoverySnapshotGolden,
);

const decodedResourceRecoverySnapshot = fromBinary(
  ResourceRecoveryOperationsSnapshotSchema,
  resourceRecoverySnapshotGolden,
);
assert.equal(decodedResourceRecoverySnapshot.schema?.minor, 4);
assert.equal(decodedResourceRecoverySnapshot.metrics?.totalInspected, 15n);
assert.equal(decodedResourceRecoverySnapshot.metrics?.durableResolved, 11n);
assert.equal(decodedResourceRecoverySnapshot.metrics?.domainFaulted, true);
assert.equal(
  decodedResourceRecoverySnapshot.alerts[0]?.lastFailureAuthority,
  RecoveryFailureAuthority.RESOURCE,
);
assert.ok(decodedResourceRecoverySnapshot.alerts[0]?.acknowledgementReceipt);
assert.equal(
  decodedResourceRecoverySnapshot.alerts[1]?.acknowledgementReceipt,
  undefined,
);
assert.equal(decodedResourceRecoverySnapshot.alertsTruncated, true);
assert.deepEqual(
  toBinary(
    ResourceRecoveryOperationsSnapshotSchema,
    decodedResourceRecoverySnapshot,
  ),
  resourceRecoverySnapshotGolden,
);

// Recovery-domain acknowledge/resume command submits round-trip with the
// oneof arm addressed (mirroring the Rust fixtures; the Rust lane pins no
// byte golden for these, so wire-level round-trip + decode is the pinned
// surface here). The schema identity stays at each arm's freeze-point minor.
const recoveryCommandCases: {
  label: string;
  minor: number;
  controlId: number;
  targetByte: number;
  armCase: ControlArm["case"];
  arm: ControlArm;
  reason: string;
}[] = [
  {
    label: "acknowledge_semantic",
    minor: 1,
    controlId: 0x51,
    targetByte: 0x71,
    armCase: "acknowledgeSemanticRecoveryAlert",
    arm: {
      case: "acknowledgeSemanticRecoveryAlert",
      value: create(AcknowledgeSemanticRecoveryAlertCommandSchema, {}),
    },
    reason: "operator inspected durable semantic recovery state",
  },
  {
    label: "resume_semantic",
    minor: 1,
    controlId: 0x52,
    targetByte: 0x71,
    armCase: "resumeSemanticRecovery",
    arm: {
      case: "resumeSemanticRecovery",
      value: create(ResumeSemanticRecoveryCommandSchema, {}),
    },
    reason: "operator resumes the escalated semantic plan",
  },
  {
    label: "acknowledge_resource",
    minor: 4,
    controlId: 0x55,
    targetByte: 0x81,
    armCase: "acknowledgeResourceRecoveryAlert",
    arm: {
      case: "acknowledgeResourceRecoveryAlert",
      value: create(AcknowledgeResourceRecoveryAlertCommandSchema, {}),
    },
    reason: "operator inspected durable resource recovery state",
  },
  {
    label: "resume_resource",
    minor: 4,
    controlId: 0x56,
    targetByte: 0x81,
    armCase: "resumeResourceRecovery",
    arm: {
      case: "resumeResourceRecovery",
      value: create(ResumeResourceRecoveryCommandSchema, {}),
    },
    reason: "operator resumes the escalated resource plan",
  },
];

for (const { label, minor, controlId, targetByte, armCase, arm, reason } of recoveryCommandCases) {
  const request = create(SubmitControlCommandRequestSchema, {
    schema: create(SchemaIdentitySchema, {
      name: "nlos.sabi.SystemControl",
      major: 1,
      minor,
      criticalExtensionIds: [],
      nonCriticalExtensionIds: [],
    }),
    command: create(ControlCommandSchema, {
      controlCommandId: new Uint8Array(16).fill(controlId),
      issuerPrincipalId: new Uint8Array(16).fill(0x32),
      source: ControlCommandSource.CLI,
      scope: ControlScope.OPERATION,
      targetId: new Uint8Array(16).fill(targetByte),
      expectedGenerationOrRevision: 8n,
      command: arm,
      reason,
    }),
  });
  const wire = toBinary(SubmitControlCommandRequestSchema, request);
  const decoded = fromBinary(SubmitControlCommandRequestSchema, wire);
  assert.equal(decoded.command?.command?.case, armCase, `${label} oneof case`);
  assert.equal(decoded.command?.reason, reason);
  assert.deepEqual(
    decoded.command?.targetId,
    new Uint8Array(16).fill(targetByte),
  );
  assert.deepEqual(
    toBinary(SubmitControlCommandRequestSchema, decoded),
    wire,
    `${label} submit must round-trip byte-identically`,
  );
}

// W32-G (v1.5) per-layer inspect requests: the additive addressing fields
// (target_id / plan_id / target_generation) must survive the wire (the Rust
// lane enforces the fail-closed addressing rules; TS/Python pin the wire
// round-trip). The identity stays pinned at the v1.5 freeze point.
function layerGet(
  view: SystemControlView,
  targetId: Uint8Array,
  planId: Uint8Array,
  targetGeneration: bigint,
) {
  return create(GetSystemControlRequestSchema, {
    schema: create(SchemaIdentitySchema, {
      name: "nlos.sabi.SystemControl",
      major: 1,
      minor: 5,
      criticalExtensionIds: [],
      nonCriticalExtensionIds: [],
    }),
    view,
    alertLimit: 8,
    targetId,
    planId,
    targetGeneration,
  });
}

const layerViewCases: {
  label: string;
  view: SystemControlView;
  targetByte: number;
  planId: Uint8Array;
  targetGeneration: bigint;
}[] = [
  {
    label: "task_group",
    view: SystemControlView.TASK_GROUP,
    targetByte: 0x91,
    planId: new Uint8Array(),
    targetGeneration: 0n,
  },
  {
    label: "task_node",
    view: SystemControlView.TASK_NODE,
    targetByte: 0xa2,
    planId: new Uint8Array(16).fill(0xa1),
    targetGeneration: 0n,
  },
  {
    label: "execution_fiber",
    view: SystemControlView.EXECUTION_FIBER,
    targetByte: 0xb1,
    planId: new Uint8Array(),
    targetGeneration: 2n,
  },
  {
    label: "topic",
    view: SystemControlView.TOPIC,
    targetByte: 0xc1,
    planId: new Uint8Array(),
    targetGeneration: 0n,
  },
  {
    label: "operation",
    view: SystemControlView.OPERATION,
    targetByte: 0xd1,
    planId: new Uint8Array(),
    targetGeneration: 1n,
  },
];

for (const { label, view, targetByte, planId, targetGeneration } of layerViewCases) {
  const request = layerGet(
    view,
    new Uint8Array(16).fill(targetByte),
    planId,
    targetGeneration,
  );
  const wire = toBinary(GetSystemControlRequestSchema, request);
  const decoded = fromBinary(GetSystemControlRequestSchema, wire);
  assert.equal(decoded.view, view, `${label} view`);
  assert.deepEqual(
    decoded.targetId,
    new Uint8Array(16).fill(targetByte),
    `${label} target_id`,
  );
  assert.deepEqual(decoded.planId, planId, `${label} plan_id`);
  assert.equal(
    decoded.targetGeneration,
    targetGeneration,
    `${label} target_generation`,
  );
  assert.deepEqual(
    toBinary(GetSystemControlRequestSchema, decoded),
    wire,
    `${label} get request must round-trip byte-identically`,
  );
}

for (const view of [
  SystemControlView.ARTIFACT_COMMIT_RECOVERY,
  SystemControlView.SEMANTIC_COMMIT_RECOVERY,
  SystemControlView.RESOURCE_COMMIT_RECOVERY,
]) {
  const recoveryGet = layerGet(view, new Uint8Array(), new Uint8Array(), 0n);
  const wire = toBinary(GetSystemControlRequestSchema, recoveryGet);
  const decoded = fromBinary(GetSystemControlRequestSchema, wire);
  assert.equal(decoded.view, view);
  assert.equal(decoded.targetId.length, 0, "recovery views carry no target");
  assert.equal(decoded.planId.length, 0);
  assert.equal(decoded.targetGeneration, 0n);
}

// W32-G per-layer snapshot goldens — byte-equal to the Rust
// w32g_layer_snapshots_pin_the_deterministic_golden_bytes vectors.
const taskGroupSnapshot = create(TaskGroupOperationsSnapshotSchema, {
  schema: create(SchemaIdentitySchema, {
    name: "nlos.sabi.SystemControl",
    major: 1,
    minor: 5,
    criticalExtensionIds: [],
    nonCriticalExtensionIds: [],
  }),
  group: create(TaskGroupStatusSchema, {
    groupId: new Uint8Array(16).fill(0x91),
    taskId: new Uint8Array(16).fill(0x92),
    state: TaskGroupLifecycleState.OPEN,
    membershipGeneration: 3n,
    stateSeq: 1n,
    createdAtMs: 1000n,
    updatedAtMs: 1500n,
  }),
  members: [
    create(TaskGroupMemberStatusSchema, {
      memberType: TaskGroupMemberType.TASK_ATTEMPT,
      memberId: new Uint8Array(16).fill(0x93),
      membershipState: TaskGroupMembershipState.ACTIVE,
      membershipGeneration: 1n,
      admissionReceipt: create(ReceiptReferenceSchema, {
        receiptId: new Uint8Array(16).fill(0x94),
      }),
    }),
    create(TaskGroupMemberStatusSchema, {
      memberType: TaskGroupMemberType.CHILD_GROUP,
      memberId: new Uint8Array(16).fill(0x95),
      membershipState: TaskGroupMembershipState.REMOVED,
      membershipGeneration: 2n,
      admissionReceipt: create(ReceiptReferenceSchema, {
        receiptId: new Uint8Array(16).fill(0x96),
      }),
      removalReceipt: create(ReceiptReferenceSchema, {
        receiptId: new Uint8Array(16).fill(0x97),
      }),
    }),
  ],
});

const taskNodeSnapshot = create(TaskNodeOperationsSnapshotSchema, {
  schema: create(SchemaIdentitySchema, {
    name: "nlos.sabi.SystemControl",
    major: 1,
    minor: 5,
    criticalExtensionIds: [],
    nonCriticalExtensionIds: [],
  }),
  node: create(TaskNodeStatusSchema, {
    planId: new Uint8Array(16).fill(0xa1),
    nodeId: new Uint8Array(16).fill(0xa2),
    kind: PlanNodeKind.EXECUTABLE,
    state: PlanNodeLifecycleState.ELIGIBLE,
    declaredRevision: 4n,
    nodeDigest: new Uint8Array(32).fill(0xa3),
    transitionCount: 2n,
    residencyTier: ContextResidencyTier.METADATA_ONLY,
    firstDeclaredAtMs: 2000n,
    updatedAtMs: 2400n,
  }),
});

const executionFiberSnapshot = create(ExecutionFiberOperationsSnapshotSchema, {
  schema: create(SchemaIdentitySchema, {
    name: "nlos.sabi.SystemControl",
    major: 1,
    minor: 5,
    criticalExtensionIds: [],
    nonCriticalExtensionIds: [],
  }),
  fiber: create(ExecutionFiberStatusSchema, {
    fiberId: new Uint8Array(16).fill(0xb1),
    generation: 2n,
    state: ExecutionFiberLifecycleState.RUNNING,
    lifecyclePhase: ExecutionFiberPhase.WAITING_EXTERNAL,
    activeCpuMs: 11n,
    elapsedWallMs: 40n,
    schedulerWaitMs: 3n,
    externalWaitMs: 20n,
    backpressureWaitMs: 1n,
  }),
});

const topicSnapshot = create(TopicOperationsSnapshotSchema, {
  schema: create(SchemaIdentitySchema, {
    name: "nlos.sabi.SystemControl",
    major: 1,
    minor: 5,
    criticalExtensionIds: [],
    nonCriticalExtensionIds: [],
  }),
  topic: create(TopicStatusSchema, {
    topicId: new Uint8Array(16).fill(0xc1),
    channelId: new Uint8Array(16).fill(0xc2),
    channelGeneration: 5n,
    name: new TextEncoder().encode("stage-b/inspect"),
    activeSubscriptions: 2n,
    policyDigest: new Uint8Array(32).fill(0xc3),
    createdAtMs: 3000n,
  }),
});

const durableOperationSnapshot = create(DurableOperationSnapshotSchema, {
  schema: create(SchemaIdentitySchema, {
    name: "nlos.sabi.SystemControl",
    major: 1,
    minor: 5,
    criticalExtensionIds: [],
    nonCriticalExtensionIds: [],
  }),
  operation: create(DurableOperationStatusSchema, {
    operationId: new Uint8Array(16).fill(0xd1),
    generation: 1n,
    state: DurableOperationState.DISPATCHED,
    ownerFiberId: new Uint8Array(16).fill(0xb1),
    ownerFiberGeneration: 2n,
  }),
});

const layerSnapshotCases: {
  label: string;
  goldenHex: string;
  wire: Uint8Array;
}[] = [
  {
    label: "task_group",
    goldenHex:
      "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180512300a10919191" +
      "9191919191919191919191919112109292929292929292929292929292929220012803300148" +
      "e80750dc0b1a2c0802121093939393939393939393939393939393180120012a120a10949494" +
      "949494949494949494949494941a400801121095959595959595959595959595959595180220" +
      "022a120a109696969696969696969696969696969632120a1097979797979797979797979797" +
      "979797",
    wire: toBinary(TaskGroupOperationsSnapshotSchema, taskGroupSnapshot),
  },
  {
    label: "task_node",
    goldenHex:
      "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180512560a10a1a1a1" +
      "a1a1a1a1a1a1a1a1a1a1a1a1a11210a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a218022003280432" +
      "20a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a33802400150" +
      "d00f58e012",
    wire: toBinary(TaskNodeOperationsSnapshotSchema, taskNodeSnapshot),
  },
  {
    label: "fiber",
    goldenHex:
      "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180512220a10b1b1b1" +
      "b1b1b1b1b1b1b1b1b1b1b1b1b1100218032002280b3028380340144801",
    wire: toBinary(ExecutionFiberOperationsSnapshotSchema, executionFiberSnapshot),
  },
  {
    label: "topic",
    goldenHex:
      "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c10011805125e0a10c1c1c1" +
      "c1c1c1c1c1c1c1c1c1c1c1c1c11210c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c21805220f737461" +
      "67652d622f696e737065637428023220c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3" +
      "c3c3c3c3c3c3c3c3c3c338b817",
    wire: toBinary(TopicOperationsSnapshotSchema, topicSnapshot),
  },
  {
    label: "operation",
    goldenHex:
      "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c10011805122a0a10d1d1d1" +
      "d1d1d1d1d1d1d1d1d1d1d1d1d1100118022a10b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b13002",
    wire: toBinary(DurableOperationSnapshotSchema, durableOperationSnapshot),
  },
];

for (const { label, goldenHex, wire } of layerSnapshotCases) {
  assert.deepEqual(wire, hexToBytes(goldenHex), `${label} snapshot golden bytes`);
}

const decodedTaskGroup = fromBinary(
  TaskGroupOperationsSnapshotSchema,
  hexToBytes(layerSnapshotCases[0]!.goldenHex),
);
assert.equal(decodedTaskGroup.group?.state, TaskGroupLifecycleState.OPEN);
assert.equal(decodedTaskGroup.group?.membershipGeneration, 3n);
assert.deepEqual(decodedTaskGroup.members.map((member) => member.memberType), [
  TaskGroupMemberType.TASK_ATTEMPT,
  TaskGroupMemberType.CHILD_GROUP,
]);
assert.ok(decodedTaskGroup.members[0]?.admissionReceipt);
assert.equal(decodedTaskGroup.members[0]?.removalReceipt, undefined);
assert.ok(decodedTaskGroup.members[1]?.removalReceipt);
assert.equal(decodedTaskGroup.membersTruncated, false);
assert.deepEqual(
  toBinary(TaskGroupOperationsSnapshotSchema, decodedTaskGroup),
  hexToBytes(layerSnapshotCases[0]!.goldenHex),
);

const decodedTaskNode = fromBinary(
  TaskNodeOperationsSnapshotSchema,
  hexToBytes(layerSnapshotCases[1]!.goldenHex),
);
assert.equal(decodedTaskNode.node?.kind, PlanNodeKind.EXECUTABLE);
assert.equal(decodedTaskNode.node?.state, PlanNodeLifecycleState.ELIGIBLE);
assert.deepEqual(decodedTaskNode.node?.nodeDigest, new Uint8Array(32).fill(0xa3));
assert.equal(decodedTaskNode.node?.residencyTier, ContextResidencyTier.METADATA_ONLY);
assert.deepEqual(
  toBinary(TaskNodeOperationsSnapshotSchema, decodedTaskNode),
  hexToBytes(layerSnapshotCases[1]!.goldenHex),
);

const decodedFiber = fromBinary(
  ExecutionFiberOperationsSnapshotSchema,
  hexToBytes(layerSnapshotCases[2]!.goldenHex),
);
assert.equal(decodedFiber.fiber?.state, ExecutionFiberLifecycleState.RUNNING);
assert.equal(
  decodedFiber.fiber?.lifecyclePhase,
  ExecutionFiberPhase.WAITING_EXTERNAL,
);
assert.equal(decodedFiber.fiber?.generation, 2n);
assert.equal(decodedFiber.fiber?.externalWaitMs, 20n);
assert.deepEqual(
  toBinary(ExecutionFiberOperationsSnapshotSchema, decodedFiber),
  hexToBytes(layerSnapshotCases[2]!.goldenHex),
);

const decodedTopic = fromBinary(
  TopicOperationsSnapshotSchema,
  hexToBytes(layerSnapshotCases[3]!.goldenHex),
);
assert.equal(
  new TextDecoder().decode(decodedTopic.topic?.name ?? new Uint8Array()),
  "stage-b/inspect",
);
assert.equal(decodedTopic.topic?.channelGeneration, 5n);
assert.deepEqual(decodedTopic.topic?.policyDigest, new Uint8Array(32).fill(0xc3));
assert.deepEqual(
  toBinary(TopicOperationsSnapshotSchema, decodedTopic),
  hexToBytes(layerSnapshotCases[3]!.goldenHex),
);

const decodedOperation = fromBinary(
  DurableOperationSnapshotSchema,
  hexToBytes(layerSnapshotCases[4]!.goldenHex),
);
assert.equal(decodedOperation.operation?.state, DurableOperationState.DISPATCHED);
assert.equal(decodedOperation.operation?.generation, 1n);
assert.deepEqual(
  decodedOperation.operation?.ownerFiberId,
  new Uint8Array(16).fill(0xb1),
);
assert.equal(decodedOperation.operation?.outcomeReceipt, undefined);
assert.deepEqual(
  toBinary(DurableOperationSnapshotSchema, decodedOperation),
  hexToBytes(layerSnapshotCases[4]!.goldenHex),
);
