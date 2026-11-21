import { create } from "@bufbuild/protobuf";
import { randomBytes } from "node:crypto";

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
  WaitStateCode,
  WakeReportSchema,
  type CancelWaitResult,
  type InspectWaitResult,
  type ListWaitsResult,
  type WaitRecord,
  type WakeReport,
  type RegisterWaitResult,
} from "../../../gen/typescript/nlos/sabi/v1/wait_control_pb.ts";
import { type MethodSemantics } from "./common.ts";
import { IpcError, equalBytes } from "./local_rpc.ts";
import { type Ed25519Signer } from "./principal_session.ts";
import {
  SABI_ID_BYTES,
  SabiServiceClient,
  SabiServiceError,
  connectAuthenticatedServiceClient,
  connectServiceClient,
  copyBytes,
  requireFixedBytes,
  type CallOptions,
  type RpcTarget,
  type SabiClientOptions,
  type ServiceErrorContext,
} from "./sabi_client.ts";
import { type ServiceRequirement } from "./service_directory.ts";

export const WAIT_CONTROL_SCHEMA = "nlos.sabi.WaitControl";
export const WAIT_CONTROL_MAJOR = 1;
export const WAIT_CONTROL_MINOR = 0;
export const WAIT_CONTROL_SERVICE = "wait_control";
export const MAX_WAIT_CONTROL_PAYLOAD_BYTES = 64 * 1024;

export const REGISTER_WAIT_METHOD = "register_wait";
export const NOTIFY_COMMITS_METHOD = "notify_commits";
export const CANCEL_WAIT_METHOD = "cancel_wait";
export const LIST_WAITS_METHOD = "list_waits";
export const INSPECT_WAIT_METHOD = "inspect_wait";

const QUERY_SEMANTICS: MethodSemantics = {
  sideEffecting: false,
  longRunning: false,
};
const MUTATION_SEMANTICS: MethodSemantics = {
  sideEffecting: true,
  longRunning: false,
};

export class WaitControlError extends SabiServiceError {
  constructor(context: ServiceErrorContext) {
    super(context);
    this.name = "WaitControlError";
  }
}

export interface RegisterWaitParams {
  readonly binding: Uint8Array;
  readonly channelId: Uint8Array;
  readonly targetSequence: bigint;
  readonly registeredAtMs?: bigint;
  readonly idempotencyKey?: Uint8Array;
}

export interface NotifyCommitsParams {
  readonly channelId: Uint8Array;
  readonly upToSequence: bigint;
  readonly notifiedAtMs?: bigint;
  readonly idempotencyKey?: Uint8Array;
}

export interface CancelWaitParams {
  readonly waitId: Uint8Array;
  readonly cancelledAtMs?: bigint;
  readonly idempotencyKey?: Uint8Array;
}

export interface ListWaitsParams {
  readonly filterChannelId?: Uint8Array;
}

/**
 * Thin SABI client for `nlos.sabi.WaitControl` (`v1.0`).
 */
export class WaitClient {
  readonly #client: SabiServiceClient;

  constructor(target: RpcTarget, options: SabiClientOptions = {}) {
    this.#client = new SabiServiceClient(
      target,
      {
        service: WAIT_CONTROL_SERVICE,
        schemaName: WAIT_CONTROL_SCHEMA,
        schemaMajor: WAIT_CONTROL_MAJOR,
        schemaMinor: WAIT_CONTROL_MINOR,
        maxPayloadBytes: MAX_WAIT_CONTROL_PAYLOAD_BYTES,
        createError: (context) => new WaitControlError(context),
      },
      options,
    );
  }

  static fromEndpoint(
    endpoint: string,
    options: SabiClientOptions = {},
  ): WaitClient {
    return new WaitClient(endpoint, options);
  }

  static async connect(
    endpoint: string,
    options: SabiClientOptions = {},
  ): Promise<WaitClient> {
    return connectServiceClient(
      endpoint,
      (rpc, opts) => new WaitClient(rpc, opts),
      options,
    );
  }

  static async connectAuthenticated(
    endpoint: string,
    signer: Ed25519Signer,
    principalId: Uint8Array,
    options: Omit<SabiClientOptions, "principalId"> = {},
  ): Promise<WaitClient> {
    return connectAuthenticatedServiceClient(
      endpoint,
      signer,
      principalId,
      (rpc, opts) => new WaitClient(rpc, opts),
      options,
    );
  }

  static serviceRequirement(): ServiceRequirement {
    return {
      service: WAIT_CONTROL_SERVICE,
      schemaName: WAIT_CONTROL_SCHEMA,
      major: WAIT_CONTROL_MAJOR,
      minimumMinor: WAIT_CONTROL_MINOR,
    };
  }

  get identity() {
    return this.#client.identity;
  }

  close(): void {
    this.#client.close();
  }

  async registerWait(
    params: RegisterWaitParams,
    options: CallOptions = {},
  ): Promise<RegisterWaitResult> {
    const binding = requireFixedBytes("binding", params.binding, SABI_ID_BYTES);
    const channelId = requireFixedBytes(
      "channelId",
      params.channelId,
      SABI_ID_BYTES,
    );
    const idempotencyKey = resolveMutationKey(
      params.idempotencyKey,
      options.idempotencyKey,
    );
    const requestMessage = create(RegisterWaitRequestSchema, {
      schema: this.#client.schemaIdentity(),
      binding,
      channelId,
      targetSequence: params.targetSequence,
      idempotencyKey: copyBytes(idempotencyKey),
      registeredAtMs: params.registeredAtMs ?? 0n,
    });
    const { result } = await this.#client.call({
      method: REGISTER_WAIT_METHOD,
      semantics: MUTATION_SEMANTICS,
      requestSchema: RegisterWaitRequestSchema,
      requestMessage,
      responseSchema: RegisterWaitResultSchema,
      options: { ...options, idempotencyKey },
    });
    requireValidWaitRecord(result.record, "RegisterWaitResult");
    return result;
  }

  async notifyCommits(
    params: NotifyCommitsParams,
    options: CallOptions = {},
  ): Promise<WakeReport> {
    const channelId = requireFixedBytes(
      "channelId",
      params.channelId,
      SABI_ID_BYTES,
    );
    const idempotencyKey = resolveMutationKey(
      params.idempotencyKey,
      options.idempotencyKey,
    );
    const requestMessage = create(NotifyCommitsRequestSchema, {
      schema: this.#client.schemaIdentity(),
      channelId,
      upToSequence: params.upToSequence,
      notifiedAtMs: params.notifiedAtMs ?? 0n,
      idempotencyKey: copyBytes(idempotencyKey),
    });
    const { result } = await this.#client.call({
      method: NOTIFY_COMMITS_METHOD,
      semantics: MUTATION_SEMANTICS,
      requestSchema: NotifyCommitsRequestSchema,
      requestMessage,
      responseSchema: WakeReportSchema,
      options: { ...options, idempotencyKey },
    });
    for (const record of result.woken) {
      requireValidWaitRecord(record, "WakeReport");
    }
    return result;
  }

  async cancelWait(
    params: CancelWaitParams,
    options: CallOptions = {},
  ): Promise<CancelWaitResult> {
    const waitId = requireFixedBytes("waitId", params.waitId, SABI_ID_BYTES);
    const idempotencyKey = resolveMutationKey(
      params.idempotencyKey,
      options.idempotencyKey,
    );
    const requestMessage = create(CancelWaitRequestSchema, {
      schema: this.#client.schemaIdentity(),
      waitId,
      cancelledAtMs: params.cancelledAtMs ?? 0n,
      idempotencyKey: copyBytes(idempotencyKey),
    });
    const { result } = await this.#client.call({
      method: CANCEL_WAIT_METHOD,
      semantics: MUTATION_SEMANTICS,
      requestSchema: CancelWaitRequestSchema,
      requestMessage,
      responseSchema: CancelWaitResultSchema,
      options: { ...options, idempotencyKey },
    });
    requireValidWaitRecord(result.record, "CancelWaitResult");
    return result;
  }

  async listWaits(
    params: ListWaitsParams = {},
    options: CallOptions = {},
  ): Promise<ListWaitsResult> {
    const filterChannelId =
      params.filterChannelId !== undefined && params.filterChannelId.byteLength > 0
        ? requireFixedBytes(
            "filterChannelId",
            params.filterChannelId,
            SABI_ID_BYTES,
          )
        : new Uint8Array();
    const requestMessage = create(ListWaitsRequestSchema, {
      schema: this.#client.schemaIdentity(),
      filterChannelId,
    });
    const { result } = await this.#client.call({
      method: LIST_WAITS_METHOD,
      semantics: QUERY_SEMANTICS,
      requestSchema: ListWaitsRequestSchema,
      requestMessage,
      responseSchema: ListWaitsResultSchema,
      options,
    });
    for (const record of result.waits) {
      requireValidWaitRecord(record, "ListWaitsResult");
    }
    return result;
  }

  async inspectWait(
    waitId: Uint8Array,
    options: CallOptions = {},
  ): Promise<InspectWaitResult> {
    const checkedWaitId = requireFixedBytes("waitId", waitId, SABI_ID_BYTES);
    const requestMessage = create(InspectWaitRequestSchema, {
      schema: this.#client.schemaIdentity(),
      waitId: checkedWaitId,
    });
    const { result } = await this.#client.call({
      method: INSPECT_WAIT_METHOD,
      semantics: QUERY_SEMANTICS,
      requestSchema: InspectWaitRequestSchema,
      requestMessage,
      responseSchema: InspectWaitResultSchema,
      options,
    });
    requireValidWaitRecord(result.record, "InspectWaitResult");
    return result;
  }
}

function resolveMutationKey(
  payloadKey: Uint8Array | undefined,
  optionKey: Uint8Array | undefined,
): Uint8Array {
  if (payloadKey !== undefined && optionKey !== undefined) {
    if (!equalBytes(payloadKey, optionKey)) {
      throw new IpcError(
        "INVALID_CONFIG",
        "WaitControl idempotencyKey differs between payload and options",
      );
    }
  }
  const chosen = payloadKey ?? optionKey;
  if (chosen === undefined) {
    return Uint8Array.from(randomBytes(SABI_ID_BYTES));
  }
  return copyBytes(chosen);
}

function requireValidWaitRecord(
  record: WaitRecord | undefined,
  context: string,
): WaitRecord {
  if (record === undefined) {
    throw new IpcError("COMPATIBILITY", `${context} is missing WaitRecord`);
  }
  if (
    record.waitId.byteLength !== SABI_ID_BYTES ||
    record.channelId.byteLength !== SABI_ID_BYTES
  ) {
    throw new IpcError(
      "COMPATIBILITY",
      `${context} WaitRecord contains malformed identifier length`,
    );
  }
  if (
    record.state !== WaitStateCode.PENDING &&
    record.state !== WaitStateCode.WOKEN &&
    record.state !== WaitStateCode.CANCELLED
  ) {
    throw new IpcError(
      "COMPATIBILITY",
      `${context} WaitRecord carries unknown or unspecified WaitStateCode (${record.state})`,
    );
  }
  return record;
}
