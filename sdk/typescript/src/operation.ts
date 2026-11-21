import { create } from "@bufbuild/protobuf";

import { OperationReferenceSchema } from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import {
  CancelOperationRequestSchema,
  OperationLifecycleState,
  OperationStatusSchema,
  QueryOperationRequestSchema,
  type OperationStatus,
} from "../../../gen/typescript/nlos/sabi/v1/operation_control_pb.ts";
import { type MethodSemantics } from "./common.ts";
import { IpcError } from "./local_rpc.ts";
import { type Ed25519Signer } from "./principal_session.ts";
import {
  SABI_ID_BYTES,
  SabiServiceClient,
  SabiServiceError,
  connectAuthenticatedServiceClient,
  connectServiceClient,
  requireFixedBytes,
  type CallOptions,
  type RpcTarget,
  type SabiClientOptions,
  type ServiceErrorContext,
} from "./sabi_client.ts";
import { type ServiceRequirement } from "./service_directory.ts";

export const OPERATION_CONTROL_SCHEMA = "nlos.sabi.OperationControl";
export const OPERATION_CONTROL_MAJOR = 1;
export const OPERATION_CONTROL_MINOR = 0;
export const OPERATION_CONTROL_SERVICE = "operation_control";
export const MAX_OPERATION_CONTROL_PAYLOAD_BYTES = 64 * 1024;

export const QUERY_OPERATION_METHOD = "query";
export const CANCEL_OPERATION_METHOD = "cancel";

const QUERY_SEMANTICS: MethodSemantics = {
  sideEffecting: false,
  longRunning: false,
};
const MUTATION_SEMANTICS: MethodSemantics = {
  sideEffecting: true,
  longRunning: false,
};

export class OperationControlError extends SabiServiceError {
  constructor(context: ServiceErrorContext) {
    super(context);
    this.name = "OperationControlError";
  }
}

export interface OperationReferenceInput {
  readonly operationId: Uint8Array;
  readonly generation: bigint;
}

/**
 * Thin SABI client for `nlos.sabi.OperationControl` (`v1.0`).
 */
export class OperationClient {
  readonly #client: SabiServiceClient;

  constructor(target: RpcTarget, options: SabiClientOptions = {}) {
    this.#client = new SabiServiceClient(
      target,
      {
        service: OPERATION_CONTROL_SERVICE,
        schemaName: OPERATION_CONTROL_SCHEMA,
        schemaMajor: OPERATION_CONTROL_MAJOR,
        schemaMinor: OPERATION_CONTROL_MINOR,
        maxPayloadBytes: MAX_OPERATION_CONTROL_PAYLOAD_BYTES,
        createError: (context) => new OperationControlError(context),
      },
      options,
    );
  }

  static fromEndpoint(
    endpoint: string,
    options: SabiClientOptions = {},
  ): OperationClient {
    return new OperationClient(endpoint, options);
  }

  static async connect(
    endpoint: string,
    options: SabiClientOptions = {},
  ): Promise<OperationClient> {
    return connectServiceClient(
      endpoint,
      (rpc, opts) => new OperationClient(rpc, opts),
      options,
    );
  }

  static async connectAuthenticated(
    endpoint: string,
    signer: Ed25519Signer,
    principalId: Uint8Array,
    options: Omit<SabiClientOptions, "principalId"> = {},
  ): Promise<OperationClient> {
    return connectAuthenticatedServiceClient(
      endpoint,
      signer,
      principalId,
      (rpc, opts) => new OperationClient(rpc, opts),
      options,
    );
  }

  static serviceRequirement(): ServiceRequirement {
    return {
      service: OPERATION_CONTROL_SERVICE,
      schemaName: OPERATION_CONTROL_SCHEMA,
      major: OPERATION_CONTROL_MAJOR,
      minimumMinor: OPERATION_CONTROL_MINOR,
    };
  }

  get identity() {
    return this.#client.identity;
  }

  close(): void {
    this.#client.close();
  }

  async query(
    operation: OperationReferenceInput,
    options: CallOptions = {},
  ): Promise<OperationStatus> {
    const operationRef = buildOperationRef(operation);
    const requestMessage = create(QueryOperationRequestSchema, {
      schema: this.#client.schemaIdentity(),
      operation: operationRef,
    });
    const { result } = await this.#client.call({
      method: QUERY_OPERATION_METHOD,
      semantics: QUERY_SEMANTICS,
      requestSchema: QueryOperationRequestSchema,
      requestMessage,
      responseSchema: OperationStatusSchema,
      options,
    });
    return validateOperationStatus(result);
  }

  async cancel(
    operation: OperationReferenceInput,
    expectedCancelEpoch = 0n,
    options: CallOptions = {},
  ): Promise<OperationStatus> {
    if (expectedCancelEpoch < 0n) {
      throw new IpcError(
        "INVALID_CONFIG",
        "expectedCancelEpoch must be non-negative",
      );
    }
    const operationRef = buildOperationRef(operation);
    const requestMessage = create(CancelOperationRequestSchema, {
      schema: this.#client.schemaIdentity(),
      operation: operationRef,
      expectedCancelEpoch,
    });
    const { result } = await this.#client.call({
      method: CANCEL_OPERATION_METHOD,
      semantics: MUTATION_SEMANTICS,
      requestSchema: CancelOperationRequestSchema,
      requestMessage,
      responseSchema: OperationStatusSchema,
      options,
      autoGenerateIdempotencyKey: true,
    });
    return validateOperationStatus(result);
  }
}

function buildOperationRef(input: OperationReferenceInput) {
  const operationId = requireFixedBytes(
    "operationId",
    input.operationId,
    SABI_ID_BYTES,
  );
  if (input.generation <= 0n) {
    throw new IpcError(
      "INVALID_CONFIG",
      "operation generation must be positive",
    );
  }
  return create(OperationReferenceSchema, {
    operationId,
    generation: input.generation,
  });
}

function validateOperationStatus(status: OperationStatus): OperationStatus {
  if (
    status.operation === undefined ||
    status.operation.operationId.byteLength !== SABI_ID_BYTES ||
    status.operation.generation <= 0n
  ) {
    throw new IpcError(
      "COMPATIBILITY",
      "OperationStatus is missing a valid OperationReference",
    );
  }
  if (
    !(status.state in OperationLifecycleState) ||
    status.state === OperationLifecycleState.UNSPECIFIED
  ) {
    throw new IpcError(
      "COMPATIBILITY",
      `OperationStatus carries unknown or unspecified state (${status.state})`,
    );
  }
  return status;
}
