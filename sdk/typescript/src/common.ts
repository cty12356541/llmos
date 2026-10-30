import {
  RetryDirective,
  SabiErrorCode,
  type CapabilityHandle,
  type Envelope,
  type OperationReference,
  type ReceiptReference,
  type SabiRequestContext,
  type SabiResponseContext,
  type SchemaIdentity,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";

const ID_BYTES = 16;
const SHA256_BYTES = 32;
const MAX_ACTIVITY_CONTEXT_BYTES = 4 * 1024;
const MAX_CAPABILITY_HANDLES = 64;
const MAX_RECEIPTS = 64;
const MAX_SAFE_MESSAGE_BYTES = 512;

export interface MethodSemantics {
  readonly sideEffecting: boolean;
  readonly longRunning: boolean;
}

export class CommonSemanticsError extends Error {
  constructor(
    readonly code: string,
    message: string,
  ) {
    super(message);
    this.name = "CommonSemanticsError";
  }
}

export interface SabiChannelSpec {
  readonly name: string;
  readonly major: number;
  readonly minor: number;
  readonly supportedCriticalExtensionIds: readonly number[];
}

export const SABI_V1_CHANNELS: Readonly<Record<string, SabiChannelSpec>> = {
  "nlos.sabi.Envelope": {
    name: "nlos.sabi.Envelope",
    major: 1,
    minor: 1,
    supportedCriticalExtensionIds: [],
  },
  "nlos.sabi.ServiceDirectory": {
    name: "nlos.sabi.ServiceDirectory",
    major: 1,
    minor: 0,
    supportedCriticalExtensionIds: [],
  },
  "nlos.sabi.OperationControl": {
    name: "nlos.sabi.OperationControl",
    major: 1,
    minor: 0,
    supportedCriticalExtensionIds: [],
  },
  "nlos.sabi.SystemControl": {
    name: "nlos.sabi.SystemControl",
    major: 1,
    minor: 7,
    supportedCriticalExtensionIds: [],
  },
  "nlos.sabi.TakeoverControl": {
    name: "nlos.sabi.TakeoverControl",
    major: 1,
    minor: 0,
    supportedCriticalExtensionIds: [],
  },
  "nlos.sabi.WaitControl": {
    name: "nlos.sabi.WaitControl",
    major: 1,
    minor: 0,
    supportedCriticalExtensionIds: [],
  },
  "nlos.sabi.PrincipalHandshake": {
    name: "nlos.sabi.PrincipalHandshake",
    major: 1,
    minor: 0,
    supportedCriticalExtensionIds: [],
  },
  "nlos.sabi.DriverProvider": {
    name: "nlos.sabi.DriverProvider",
    major: 1,
    minor: 0,
    supportedCriticalExtensionIds: [],
  },
  "nlos.sabi.ChannelControl": {
    name: "nlos.sabi.ChannelControl",
    major: 1,
    minor: 0,
    supportedCriticalExtensionIds: [],
  },
};

export interface SchemaCompatibilityOptions {
  readonly supportedMajor?: number;
  readonly supportedCriticalExtensionIds?: readonly number[];
}

export function validateSchemaCompatibility(
  identity: SchemaIdentity | undefined,
  expectedName?: string,
  options: SchemaCompatibilityOptions = {},
): SchemaIdentity {
  if (!identity || identity.name.length === 0 || identity.name.includes("\0")) {
    fail("MISSING_SCHEMA_IDENTITY", "SABI schema identity is missing or empty");
  }
  if (expectedName !== undefined && identity.name !== expectedName) {
    fail(
      "UNKNOWN_SCHEMA",
      `unexpected SABI schema ${JSON.stringify(identity.name)}, expected ${JSON.stringify(expectedName)}`,
    );
  }
  const spec = SABI_V1_CHANNELS[identity.name];
  if (spec === undefined && options.supportedMajor === undefined) {
    fail("UNKNOWN_SCHEMA", `unregistered SABI schema ${JSON.stringify(identity.name)}`);
  }
  const requiredMajor = options.supportedMajor ?? spec!.major;
  if (identity.major !== requiredMajor) {
    fail(
      "E_ABI_VERSION",
      `unsupported SABI major version ${identity.major} for ${identity.name} (expected ${requiredMajor})`,
    );
  }
  const allowedCritical = new Set(
    options.supportedCriticalExtensionIds ??
      spec?.supportedCriticalExtensionIds ??
      [],
  );
  for (const extId of identity.criticalExtensionIds) {
    if (extId === 0 || !allowedCritical.has(extId)) {
      fail(
        "UNSUPPORTED_CRITICAL_EXTENSION",
        `unsupported critical extension id ${extId} on ${identity.name}`,
      );
    }
  }
  return identity;
}

export function validateRequestContext(
  envelope: Envelope,
  semantics: MethodSemantics,
  nowMonotonicNs: bigint,
): SabiRequestContext {
  if (envelope.commonContext.case !== "requestContext") {
    fail("MISSING_REQUEST_CONTEXT", "SABI request context is missing");
  }
  const context = envelope.commonContext.value;
  if (!context.caller) {
    fail("MISSING_CALLER", "caller identity is missing");
  }
  requireId("principal_id", context.caller.principalId);
  requireId("application_id", context.caller.applicationId);
  requireId("process_id", context.caller.processId);
  requirePositive("process_generation", context.caller.processGeneration);
  requireId("correlation_id", context.correlationId);

  if (context.idempotencyKey.length === 0) {
    if (semantics.sideEffecting) {
      fail("MISSING_IDEMPOTENCY_KEY", "side-effecting call requires an idempotency key");
    }
  } else {
    requireId("idempotency_key", context.idempotencyKey);
  }
  if (context.deadlineMonotonicNs === 0n) {
    if (semantics.longRunning) {
      fail("MISSING_DEADLINE", "long-running call requires a deadline");
    }
  } else if (context.deadlineMonotonicNs <= nowMonotonicNs) {
    fail("DEADLINE_EXPIRED", "call deadline has expired");
  }
  if (context.activityContext.length > MAX_ACTIVITY_CONTEXT_BYTES) {
    fail("ACTIVITY_CONTEXT_TOO_LARGE", "activity context exceeds 4 KiB");
  }
  if (context.taskExecutionBinding) {
    requireId("task_attempt_id", context.taskExecutionBinding.taskAttemptId);
    requirePositive("task_authority_term", context.taskExecutionBinding.taskAuthorityTerm);
    requirePositive(
      "isolation_domain_generation",
      context.taskExecutionBinding.isolationDomainGeneration,
    );
  }
  requireCapabilities(context.capabilityHandles, context.reservationHandle);
  const digestLength = context.proposalOrInputDigestSha256.length;
  if (digestLength !== 0 && digestLength !== SHA256_BYTES) {
    fail("INVALID_PROPOSAL_DIGEST", "proposal/input digest must be SHA-256 sized");
  }
  return context;
}

export function validateResponseContext(
  envelope: Envelope,
  semantics: MethodSemantics,
): SabiResponseContext {
  if (envelope.commonContext.case !== "responseContext") {
    fail("MISSING_RESPONSE_CONTEXT", "SABI response context is missing");
  }
  const context = envelope.commonContext.value;
  requireId("correlation_id", context.correlationId);
  if (context.operation) requireOperation(context.operation);
  requireReceipts(context.receipts);
  if (
    semantics.sideEffecting &&
    context.operation === undefined &&
    context.receipts.length === 0 &&
    context.failure === undefined
  ) {
    fail("MISSING_EFFECT_EVIDENCE", "mutation response requires Operation or Receipt");
  }
  const failure = context.failure;
  if (!failure) return context;

  if (!(failure.code in SabiErrorCode) || failure.code === SabiErrorCode.UNSPECIFIED) {
    fail("INVALID_ERROR_CODE", "unknown or unspecified SABI error code");
  }
  if (!(failure.retry in RetryDirective) || failure.retry === RetryDirective.UNSPECIFIED) {
    fail("INVALID_RETRY_DIRECTIVE", "unknown or unspecified retry directive");
  }
  if (
    new TextEncoder().encode(failure.safeMessage).length > MAX_SAFE_MESSAGE_BYTES ||
    failure.safeMessage.includes("\0")
  ) {
    fail("UNSAFE_ERROR_MESSAGE", "safe error message is oversized or contains NUL");
  }

  const hasOperation = context.operation !== undefined;
  const hasReceipt = context.receipts.length > 0;
  if (
    failure.retry ===
      RetryDirective.QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY &&
    !hasOperation
  ) {
    fail("MISSING_OPERATION", "query-operation retry requires an Operation reference");
  }
  if (
    failure.code === SabiErrorCode.UNCERTAIN ||
    failure.code === SabiErrorCode.EFFECT_UNKNOWN
  ) {
    if (
      failure.retry !==
      RetryDirective.QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY
    ) {
      fail("UNSAFE_RETRY", "uncertain outcome must preserve the original idempotency key");
    }
  } else if (
    failure.code === SabiErrorCode.RETRY &&
    failure.retry !== RetryDirective.RETRY_SAME_IDEMPOTENCY_KEY
  ) {
    fail("UNSAFE_RETRY", "retry outcome must preserve the original idempotency key");
  }
  if (failure.code === SabiErrorCode.PARTIAL && !hasReceipt) {
    fail("MISSING_RECEIPT", "partial outcome requires a Receipt reference");
  }
  return context;
}

function requireId(field: string, value: Uint8Array): void {
  if (value.length !== ID_BYTES) {
    fail("INVALID_ID", `${field} must contain exactly ${ID_BYTES} bytes`);
  }
}

function requirePositive(field: string, value: bigint): void {
  if (value <= 0n) fail("INVALID_GENERATION", `${field} must be positive`);
}

function requireCapabilities(
  handles: CapabilityHandle[],
  reservation: CapabilityHandle | undefined,
): void {
  if (handles.length > MAX_CAPABILITY_HANDLES) {
    fail("TOO_MANY_CAPABILITIES", "too many capability handles");
  }
  const seen = new Set<string>();
  for (const handle of [...handles, ...(reservation ? [reservation] : [])]) {
    requirePositive("capability_slot", handle.slot);
    requirePositive("capability_generation", handle.generation);
    const key = `${handle.slot}:${handle.generation}`;
    if (seen.has(key)) fail("DUPLICATE_CAPABILITY", "duplicate capability handle");
    seen.add(key);
  }
}

function requireOperation(operation: OperationReference): void {
  requireId("operation_id", operation.operationId);
  requirePositive("operation_generation", operation.generation);
}

function requireReceipts(receipts: ReceiptReference[]): void {
  if (receipts.length > MAX_RECEIPTS) fail("TOO_MANY_RECEIPTS", "too many receipts");
  const seen = new Set<string>();
  for (const receipt of receipts) {
    requireId("receipt_id", receipt.receiptId);
    const key = Buffer.from(receipt.receiptId).toString("hex");
    if (seen.has(key)) fail("DUPLICATE_RECEIPT", "duplicate receipt reference");
    seen.add(key);
  }
}

function fail(code: string, message: string): never {
  throw new CommonSemanticsError(code, message);
}
