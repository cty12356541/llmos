import { create, fromBinary, toBinary, type Message } from "@bufbuild/protobuf";
import type { GenMessage } from "@bufbuild/protobuf/codegenv2";
import { randomBytes } from "node:crypto";

import {
  CapabilityHandleSchema,
  EnvelopeSchema,
  ExchangeRequestSchema,
  RetryDirective,
  SabiErrorCode,
  SabiRequestContextSchema,
  SchemaIdentitySchema,
  TaskExecutionBindingSchema,
  type CapabilityHandle,
  type OperationReference,
  type ReceiptReference,
  type SabiFailure,
  type SabiResponseContext,
  type SchemaIdentity,
  type TaskExecutionBinding,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import {
  validateRequestContext,
  validateResponseContext,
  validateSchemaCompatibility,
  type MethodSemantics,
} from "./common.ts";
import {
  IpcError,
  LocalRpcClient,
  equalBytes,
  resolveTransportConfig,
  validateEndpoint,
  type TransportConfig,
} from "./local_rpc.ts";
import {
  SessionIdentity,
  connectAuthenticated,
  type Ed25519Signer,
  type SessionIdentityInit,
} from "./principal_session.ts";

export const ENVELOPE_SCHEMA_NAME = "nlos.sabi.Envelope";
export const ENVELOPE_SCHEMA_MAJOR = 1;
export const ENVELOPE_SCHEMA_MINOR = 1;
export const DEFAULT_MAX_SERVICE_PAYLOAD_BYTES = 64 * 1024;
export const SABI_ID_BYTES = 16;
export const SHA256_DIGEST_BYTES = 32;

export type RpcTarget = string | LocalRpcClient;

export interface CapabilityHandleInput {
  readonly slot: bigint;
  readonly generation: bigint;
}

export interface TaskExecutionBindingInput {
  readonly taskAttemptId: Uint8Array;
  readonly taskAuthorityTerm: bigint;
  readonly taskControlEpoch?: bigint;
  readonly cancelEpoch?: bigint;
  readonly permitEpoch?: bigint;
  readonly isolationDomainGeneration: bigint;
}

export interface SabiClientOptions {
  readonly transport?: Partial<TransportConfig>;
  readonly identity?: SessionIdentity | SessionIdentityInit;
  readonly principalId?: Uint8Array;
  readonly applicationId?: Uint8Array;
  readonly processId?: Uint8Array;
  readonly processGeneration?: bigint;
  readonly capabilityHandles?: readonly CapabilityHandleInput[];
  readonly reservationHandle?: CapabilityHandleInput;
  readonly activityContext?: Uint8Array;
  readonly taskExecutionBinding?: TaskExecutionBindingInput;
}

export interface CallOptions {
  readonly idempotencyKey?: Uint8Array;
  readonly correlationId?: Uint8Array;
  readonly requestId?: Uint8Array;
  readonly deadlineMonotonicNs?: bigint;
  readonly nowMonotonicNs?: bigint;
  readonly capabilityHandles?: readonly CapabilityHandleInput[];
  readonly reservationHandle?: CapabilityHandleInput;
  readonly activityContext?: Uint8Array;
  readonly taskExecutionBinding?: TaskExecutionBindingInput;
  readonly proposalOrInputDigestSha256?: Uint8Array;
}

export interface ServiceErrorContext {
  readonly service: string;
  readonly method: string;
  readonly failure: SabiFailure;
  readonly operation: OperationReference | undefined;
  readonly receipts: readonly ReceiptReference[];
}

export type ServiceErrorFactory = (context: ServiceErrorContext) => SabiServiceError;

/**
 * Base typed error surfaced when a remote SABI service returns a `SabiFailure`.
 * Messages never serialize raw receipts, operation bytes, keys, or blob data.
 */
export class SabiServiceError extends Error {
  readonly service: string;
  readonly method: string;
  readonly code: SabiErrorCode;
  readonly retry: RetryDirective;
  readonly safeMessage: string;
  readonly operation: OperationReference | undefined;
  readonly receipts: readonly ReceiptReference[];

  constructor(context: ServiceErrorContext) {
    const codeName = SabiErrorCode[context.failure.code] ?? String(context.failure.code);
    const retryName =
      RetryDirective[context.failure.retry] ?? String(context.failure.retry);
    super(
      `${context.service}.${context.method} failed: code=${codeName} (${context.failure.code}), retry=${retryName} (${context.failure.retry}), detail=${JSON.stringify(context.failure.safeMessage)}`,
    );
    this.name = "SabiServiceError";
    this.service = context.service;
    this.method = context.method;
    this.code = context.failure.code;
    this.retry = context.failure.retry;
    this.safeMessage = context.failure.safeMessage;
    this.operation = context.operation;
    this.receipts = context.receipts;
  }
}

export interface SabiServiceExchangeSpec<
  TReq extends Message,
  TRes extends Message,
> {
  readonly method: string;
  readonly semantics: MethodSemantics;
  readonly requestSchema: GenMessage<TReq>;
  readonly requestMessage: TReq;
  readonly responseSchema: GenMessage<TRes>;
  readonly options?: CallOptions;
  readonly autoGenerateIdempotencyKey?: boolean;
  readonly redactProtobufDecodeError?: boolean;
}

export interface SabiServiceExchangeOutput<TRes extends Message> {
  readonly result: TRes;
  readonly responseContext: SabiResponseContext;
}

export interface SabiServiceDescriptor {
  readonly service: string;
  readonly schemaName: string;
  readonly schemaMajor: number;
  readonly schemaMinor: number;
  readonly maxPayloadBytes?: number;
  readonly createError?: ServiceErrorFactory;
}

/**
 * Shared SABI thin-client core handling envelope construction, request/response
 * context validation, 64 KiB payload bounds, correlation verification, and
 * single-connection vs persistent-connection dispatch.
 */
export class SabiServiceClient {
  readonly #target: RpcTarget;
  readonly #config: TransportConfig;
  readonly #identity: SessionIdentity;
  readonly #defaultCapabilities: readonly CapabilityHandle[];
  readonly #defaultReservation: CapabilityHandle | undefined;
  readonly #defaultActivityContext: Uint8Array | undefined;
  readonly #defaultTaskBinding: TaskExecutionBinding | undefined;
  readonly #descriptor: Required<SabiServiceDescriptor>;

  constructor(
    target: RpcTarget,
    descriptor: SabiServiceDescriptor,
    options: SabiClientOptions = {},
  ) {
    if (typeof target === "string") {
      validateEndpoint(target);
    }
    this.#target = target;
    this.#config = resolveTransportConfig(options.transport);
    this.#identity = resolveSessionIdentity(options);
    this.#defaultCapabilities = (options.capabilityHandles ?? []).map(
      toCapabilityHandle,
    );
    this.#defaultReservation =
      options.reservationHandle !== undefined
        ? toCapabilityHandle(options.reservationHandle)
        : undefined;
    this.#defaultActivityContext =
      options.activityContext !== undefined
        ? copyBytes(options.activityContext)
        : undefined;
    this.#defaultTaskBinding =
      options.taskExecutionBinding !== undefined
        ? toTaskExecutionBinding(options.taskExecutionBinding)
        : undefined;
    this.#descriptor = {
      service: descriptor.service,
      schemaName: descriptor.schemaName,
      schemaMajor: descriptor.schemaMajor,
      schemaMinor: descriptor.schemaMinor,
      maxPayloadBytes:
        descriptor.maxPayloadBytes ?? DEFAULT_MAX_SERVICE_PAYLOAD_BYTES,
      createError:
        descriptor.createError ?? ((context) => new SabiServiceError(context)),
    };
  }

  get identity(): SessionIdentity {
    return this.#identity;
  }

  get transportConfig(): TransportConfig {
    return this.#config;
  }

  schemaIdentity(): SchemaIdentity {
    return createSchemaIdentity(
      this.#descriptor.schemaName,
      this.#descriptor.schemaMajor,
      this.#descriptor.schemaMinor,
    );
  }

  close(): void {
    if (this.#target instanceof LocalRpcClient) {
      this.#target.close();
    }
  }

  async call<TReq extends Message, TRes extends Message>(
    spec: SabiServiceExchangeSpec<TReq, TRes>,
  ): Promise<SabiServiceExchangeOutput<TRes>> {
    const {
      method,
      semantics,
      requestSchema,
      requestMessage,
      responseSchema,
      options,
      autoGenerateIdempotencyKey = false,
      redactProtobufDecodeError = false,
    } = spec;

    const payload = toBinary(requestSchema, requestMessage);
    if (payload.byteLength > this.#descriptor.maxPayloadBytes) {
      throw new IpcError(
        "FRAME_TOO_LARGE",
        `${this.#descriptor.service}.${method} request payload exceeds ${this.#descriptor.maxPayloadBytes} bytes`,
      );
    }

    const requestId =
      options?.requestId !== undefined
        ? copyBytes(options.requestId)
        : Uint8Array.from(randomBytes(SABI_ID_BYTES));
    const idempotencyKey =
      options?.idempotencyKey !== undefined
        ? copyBytes(options.idempotencyKey)
        : autoGenerateIdempotencyKey && semantics.sideEffecting
          ? Uint8Array.from(randomBytes(SABI_ID_BYTES))
          : new Uint8Array();
    const correlationId =
      options?.correlationId !== undefined
        ? copyBytes(options.correlationId)
        : semantics.sideEffecting && idempotencyKey.byteLength === SABI_ID_BYTES
          ? copyBytes(idempotencyKey)
          : Uint8Array.from(randomBytes(SABI_ID_BYTES));
    const deadlineMonotonicNs = options?.deadlineMonotonicNs ?? 0n;
    if (deadlineMonotonicNs < 0n) {
      throw new IpcError(
        "INVALID_CONFIG",
        "deadlineMonotonicNs must be non-negative",
      );
    }

    const capabilityHandles =
      options?.capabilityHandles !== undefined
        ? options.capabilityHandles.map(toCapabilityHandle)
        : this.#defaultCapabilities.map((handle) =>
            create(CapabilityHandleSchema, {
              slot: handle.slot,
              generation: handle.generation,
            }),
          );
    const reservationHandle =
      options?.reservationHandle !== undefined
        ? toCapabilityHandle(options.reservationHandle)
        : this.#defaultReservation;
    const activityContext =
      options?.activityContext !== undefined
        ? copyBytes(options.activityContext)
        : this.#defaultActivityContext !== undefined
          ? copyBytes(this.#defaultActivityContext)
          : new Uint8Array();
    const taskExecutionBinding =
      options?.taskExecutionBinding !== undefined
        ? toTaskExecutionBinding(options.taskExecutionBinding)
        : this.#defaultTaskBinding;
    const proposalOrInputDigestSha256 =
      options?.proposalOrInputDigestSha256 !== undefined
        ? copyBytes(options.proposalOrInputDigestSha256)
        : new Uint8Array();

    const envelope = create(EnvelopeSchema, {
      schema: createSchemaIdentity(
        ENVELOPE_SCHEMA_NAME,
        ENVELOPE_SCHEMA_MAJOR,
        ENVELOPE_SCHEMA_MINOR,
      ),
      requestId,
      service: this.#descriptor.service,
      method,
      commonContext: {
        case: "requestContext",
        value: create(SabiRequestContextSchema, {
          caller: this.#identity.toCallerIdentity(),
          activityContext,
          taskExecutionBinding,
          correlationId,
          idempotencyKey,
          deadlineMonotonicNs,
          capabilityHandles,
          reservationHandle,
          proposalOrInputDigestSha256,
        }),
      },
      payload,
    });

    validateRequestContext(
      envelope,
      semantics,
      options?.nowMonotonicNs ?? 0n,
    );

    const response = await this.#exchange(
      create(ExchangeRequestSchema, { envelope }),
    );
    const responseEnvelope = response.envelope;
    if (responseEnvelope === undefined) {
      this.close();
      throw new IpcError(
        "COMPATIBILITY",
        `${this.#descriptor.service}.${method} response envelope is missing`,
      );
    }
    if (
      responseEnvelope.service !== this.#descriptor.service ||
      responseEnvelope.method !== method
    ) {
      this.close();
      throw new IpcError(
        "COMPATIBILITY",
        `${this.#descriptor.service}.${method} response service/method mismatch`,
      );
    }

    if (responseEnvelope.commonContext.case !== "responseContext") {
      this.close();
    } else if (
      responseEnvelope.commonContext.value.correlationId.byteLength ===
        correlationId.byteLength &&
      !equalBytes(
        responseEnvelope.commonContext.value.correlationId,
        correlationId,
      )
    ) {
      this.close();
      throw new IpcError(
        "CORRELATION_ID_MISMATCH",
        "SABI response correlation_id does not match request",
      );
    }

    const responseContext = validateResponseContext(responseEnvelope, semantics);
    if (!equalBytes(responseContext.correlationId, correlationId)) {
      this.close();
      throw new IpcError(
        "CORRELATION_ID_MISMATCH",
        "SABI response correlation_id does not match request",
      );
    }
    if (responseContext.failure !== undefined) {
      throw this.#descriptor.createError({
        service: this.#descriptor.service,
        method,
        failure: responseContext.failure,
        operation: responseContext.operation,
        receipts: responseContext.receipts,
      });
    }

    const responsePayload = responseEnvelope.payload;
    if (responsePayload.byteLength > this.#descriptor.maxPayloadBytes) {
      throw new IpcError(
        "FRAME_TOO_LARGE",
        `${this.#descriptor.service}.${method} response payload exceeds ${this.#descriptor.maxPayloadBytes} bytes`,
      );
    }
    if (responsePayload.byteLength === 0) {
      this.close();
      throw new IpcError(
        "COMPATIBILITY",
        `${this.#descriptor.service}.${method} response payload is empty`,
      );
    }

    let decoded: TRes;
    try {
      decoded = fromBinary(responseSchema, responsePayload);
    } catch (cause) {
      this.close();
      throw new IpcError(
        "COMPATIBILITY",
        `malformed ${this.#descriptor.service}.${method} response payload`,
        redactProtobufDecodeError ? undefined : { cause },
      );
    }

    const schema = (decoded as Message & { schema?: SchemaIdentity }).schema;
    try {
      validateSchemaCompatibility(schema, this.#descriptor.schemaName, {
        supportedMajor: this.#descriptor.schemaMajor,
      });
    } catch (error) {
      this.close();
      throw error;
    }

    return { result: decoded, responseContext };
  }

  async #exchange(
    request: ReturnType<typeof create<typeof ExchangeRequestSchema>>,
  ) {
    if (typeof this.#target === "string") {
      const client = await LocalRpcClient.connect(this.#target, this.#config);
      try {
        return await client.exchange(request);
      } finally {
        client.close();
      }
    }
    return await this.#target.exchange(request);
  }
}

export async function connectServiceClient<T>(
  endpoint: string,
  construct: (rpc: LocalRpcClient, options: SabiClientOptions) => T,
  options: SabiClientOptions = {},
): Promise<T> {
  const rpc = await LocalRpcClient.connect(endpoint, options.transport);
  try {
    return construct(rpc, options);
  } catch (error) {
    rpc.close();
    throw error;
  }
}

export async function connectAuthenticatedServiceClient<T>(
  endpoint: string,
  signer: Ed25519Signer,
  principalId: Uint8Array,
  construct: (rpc: LocalRpcClient, options: SabiClientOptions) => T,
  options: Omit<SabiClientOptions, "principalId"> = {},
): Promise<T> {
  const rpc = await connectAuthenticated(
    endpoint,
    signer,
    principalId,
    options.transport,
  );
  try {
    const identity =
      options.identity !== undefined
        ? SessionIdentity.from(options.identity)
        : SessionIdentity.derive(principalId, {
            applicationId: options.applicationId,
            processId: options.processId,
            processGeneration: options.processGeneration,
          });
    if (!equalBytes(identity.principalId, principalId)) {
      throw new IpcError(
        "INVALID_CONFIG",
        "session identity principalId must match handshake principalId",
      );
    }
    return construct(rpc, { ...options, identity });
  } catch (error) {
    rpc.close();
    throw error;
  }
}

export function createSchemaIdentity(
  name: string,
  major = 1,
  minor = 0,
): SchemaIdentity {
  return create(SchemaIdentitySchema, {
    name,
    major,
    minor,
    criticalExtensionIds: [],
    nonCriticalExtensionIds: [],
  });
}

export function copyBytes(value: Uint8Array): Uint8Array {
  return Uint8Array.from(value);
}

export function requireFixedBytes(
  field: string,
  value: Uint8Array,
  expectedBytes: number,
): Uint8Array {
  if (value.byteLength !== expectedBytes) {
    throw new IpcError(
      "INVALID_CONFIG",
      `${field} must contain exactly ${expectedBytes} bytes`,
    );
  }
  return Uint8Array.from(value);
}

function resolveSessionIdentity(options: SabiClientOptions): SessionIdentity {
  if (options.identity !== undefined) {
    return SessionIdentity.from(options.identity);
  }
  const principalId =
    options.principalId ?? Uint8Array.from(randomBytes(SABI_ID_BYTES));
  return SessionIdentity.derive(principalId, {
    applicationId: options.applicationId,
    processId: options.processId,
    processGeneration: options.processGeneration,
  });
}

function toCapabilityHandle(input: CapabilityHandleInput): CapabilityHandle {
  return create(CapabilityHandleSchema, {
    slot: input.slot,
    generation: input.generation,
  });
}

function toTaskExecutionBinding(
  input: TaskExecutionBindingInput,
): TaskExecutionBinding {
  return create(TaskExecutionBindingSchema, {
    taskAttemptId: copyBytes(input.taskAttemptId),
    taskAuthorityTerm: input.taskAuthorityTerm,
    taskControlEpoch: input.taskControlEpoch ?? 0n,
    cancelEpoch: input.cancelEpoch ?? 0n,
    permitEpoch: input.permitEpoch ?? 0n,
    isolationDomainGeneration: input.isolationDomainGeneration,
  });
}
