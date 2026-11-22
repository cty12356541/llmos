import { create } from "@bufbuild/protobuf";

import {
  AttenuateBudgetRequestSchema,
  AttenuateBudgetResultSchema,
  LlmChatRequestSchema,
  LlmChatResultSchema,
  LlmChatTurnSchema,
  LlmUsageWireSchema,
  QueryBudgetRequestSchema,
  QueryBudgetResultSchema,
  RecordExternalUsageRequestSchema,
  RecordExternalUsageResultSchema,
  type AttenuateBudgetResult,
  type LlmChatResult,
  type LlmUsageWire,
  type QueryBudgetResult,
  type RecordExternalUsageResult,
} from "../../../gen/typescript/nlos/sabi/v1/llm_gateway_pb.ts";
import { type MethodSemantics } from "./common.ts";
import { IpcError } from "./local_rpc.ts";
import { type Ed25519Signer } from "./principal_session.ts";
import {
  SabiServiceClient,
  SabiServiceError,
  connectAuthenticatedServiceClient,
  connectServiceClient,
  type CallOptions,
  type RpcTarget,
  type SabiClientOptions,
  type ServiceErrorContext,
} from "./sabi_client.ts";
import { type ServiceRequirement } from "./service_directory.ts";

export const LLM_GATEWAY_SCHEMA = "nlos.sabi.LlmGateway";
export const LLM_GATEWAY_MAJOR = 1;
export const LLM_GATEWAY_MINOR = 0;
export const LLM_GATEWAY_SERVICE = "llm_gateway";
export const MAX_LLM_GATEWAY_PAYLOAD_BYTES = 64 * 1024;

export const LLM_CHAT_METHOD = "llm_chat";
export const QUERY_BUDGET_METHOD = "query_budget";
export const ATTENUATE_BUDGET_METHOD = "attenuate_budget";
export const RECORD_EXTERNAL_USAGE_METHOD = "record_external_usage";

const QUERY_SEMANTICS: MethodSemantics = {
  sideEffecting: false,
  longRunning: false,
};
// Side-effecting LlmGateway calls enforce both explicit idempotencyKey and
// explicit deadlineMonotonicNs fail-closed before crossing the wire.
const SIDE_EFFECTING_SEMANTICS: MethodSemantics = {
  sideEffecting: true,
  longRunning: true,
};

export class LlmGatewayError extends SabiServiceError {
  constructor(context: ServiceErrorContext) {
    super(context);
    this.name = "LlmGatewayError";
  }
}

export interface LlmChatTurnInput {
  readonly role: string;
  readonly content: string;
}

export interface LlmChatParams {
  readonly model: string;
  readonly turns: readonly LlmChatTurnInput[];
  readonly maxOutputTokens?: number;
  readonly temperatureMilli?: number;
  readonly sessionHint?: string;
}

export interface LlmUsageInput {
  readonly apiCalls: bigint;
  readonly promptTokens: bigint;
  readonly completionTokens: bigint;
  readonly totalTokens: bigint;
  readonly costMicrocredits: bigint;
}

export interface AttenuateBudgetParams {
  readonly childLimit: LlmUsageInput;
  readonly validUntilMs: bigint;
}

export interface RecordExternalUsageParams {
  readonly provider: string;
  readonly model: string;
  readonly usage: LlmUsageInput;
  readonly recordedAtMs: bigint;
}

/**
 * Thin SABI client for `nlos.sabi.LlmGateway` (`v1.0`). Enforces the 64 KiB
 * payload frame bound and requires explicit `idempotencyKey` and `deadlineMonotonicNs`
 * on all side-effecting methods.
 */
export class LlmGatewayClient {
  readonly #client: SabiServiceClient;

  constructor(target: RpcTarget, options: SabiClientOptions = {}) {
    this.#client = new SabiServiceClient(
      target,
      {
        service: LLM_GATEWAY_SERVICE,
        schemaName: LLM_GATEWAY_SCHEMA,
        schemaMajor: LLM_GATEWAY_MAJOR,
        schemaMinor: LLM_GATEWAY_MINOR,
        maxPayloadBytes: MAX_LLM_GATEWAY_PAYLOAD_BYTES,
        createError: (context) => new LlmGatewayError(context),
      },
      options,
    );
  }

  static fromEndpoint(
    endpoint: string,
    options: SabiClientOptions = {},
  ): LlmGatewayClient {
    return new LlmGatewayClient(endpoint, options);
  }

  static async connect(
    endpoint: string,
    options: SabiClientOptions = {},
  ): Promise<LlmGatewayClient> {
    return connectServiceClient(
      endpoint,
      (rpc, opts) => new LlmGatewayClient(rpc, opts),
      options,
    );
  }

  static async connectAuthenticated(
    endpoint: string,
    signer: Ed25519Signer,
    principalId: Uint8Array,
    options: Omit<SabiClientOptions, "principalId"> = {},
  ): Promise<LlmGatewayClient> {
    return connectAuthenticatedServiceClient(
      endpoint,
      signer,
      principalId,
      (rpc, opts) => new LlmGatewayClient(rpc, opts),
      options,
    );
  }

  static serviceRequirement(): ServiceRequirement {
    return {
      service: LLM_GATEWAY_SERVICE,
      schemaName: LLM_GATEWAY_SCHEMA,
      major: LLM_GATEWAY_MAJOR,
      minimumMinor: LLM_GATEWAY_MINOR,
    };
  }

  get identity() {
    return this.#client.identity;
  }

  close(): void {
    this.#client.close();
  }

  async llmChat(
    params: LlmChatParams,
    options: CallOptions = {},
  ): Promise<LlmChatResult> {
    requireNonEmptyToken("model", params.model);
    if (params.turns.length === 0) {
      throw new IpcError(
        "INVALID_CONFIG",
        "llmChat requires at least one conversation turn",
      );
    }
    const turns = params.turns.map((turn, index) => {
      requireNonEmptyToken(`turns[${index}].role`, turn.role);
      return create(LlmChatTurnSchema, {
        role: turn.role,
        content: turn.content,
      });
    });
    const maxOutputTokens = params.maxOutputTokens ?? 0;
    const temperatureMilli = params.temperatureMilli ?? 0;
    if (
      !Number.isSafeInteger(maxOutputTokens) ||
      maxOutputTokens < 0 ||
      !Number.isSafeInteger(temperatureMilli) ||
      temperatureMilli < 0
    ) {
      throw new IpcError(
        "INVALID_CONFIG",
        "maxOutputTokens and temperatureMilli must be non-negative integers",
      );
    }
    const sessionHint = params.sessionHint ?? "";
    if (sessionHint.includes("\0")) {
      throw new IpcError(
        "INVALID_CONFIG",
        "sessionHint must not contain NUL",
      );
    }

    const requestMessage = create(LlmChatRequestSchema, {
      schema: this.#client.schemaIdentity(),
      model: params.model,
      turns,
      maxOutputTokens,
      temperatureMilli,
      sessionHint,
    });
    const { result } = await this.#client.call({
      method: LLM_CHAT_METHOD,
      semantics: SIDE_EFFECTING_SEMANTICS,
      requestSchema: LlmChatRequestSchema,
      requestMessage,
      responseSchema: LlmChatResultSchema,
      options,
      autoGenerateIdempotencyKey: false,
      redactProtobufDecodeError: true,
    });
    if (result.usage === undefined) {
      throw new IpcError(
        "COMPATIBILITY",
        "LlmChatResult is missing usage accounting",
      );
    }
    return result;
  }

  async queryBudget(options: CallOptions = {}): Promise<QueryBudgetResult> {
    const requestMessage = create(QueryBudgetRequestSchema, {
      schema: this.#client.schemaIdentity(),
    });
    const { result } = await this.#client.call({
      method: QUERY_BUDGET_METHOD,
      semantics: QUERY_SEMANTICS,
      requestSchema: QueryBudgetRequestSchema,
      requestMessage,
      responseSchema: QueryBudgetResultSchema,
      options,
    });
    if (
      result.limit === undefined ||
      result.consumed === undefined ||
      result.remaining === undefined ||
      result.externalAdvisory === undefined
    ) {
      throw new IpcError(
        "COMPATIBILITY",
        "QueryBudgetResult is missing required accounting fields",
      );
    }
    return result;
  }

  async attenuateBudget(
    params: AttenuateBudgetParams,
    options: CallOptions = {},
  ): Promise<AttenuateBudgetResult> {
    if (params.validUntilMs < 0n) {
      throw new IpcError(
        "INVALID_CONFIG",
        "validUntilMs must be non-negative",
      );
    }
    const requestMessage = create(AttenuateBudgetRequestSchema, {
      schema: this.#client.schemaIdentity(),
      childLimit: toUsageWire(params.childLimit),
      validUntilMs: params.validUntilMs,
    });
    const { result } = await this.#client.call({
      method: ATTENUATE_BUDGET_METHOD,
      semantics: SIDE_EFFECTING_SEMANTICS,
      requestSchema: AttenuateBudgetRequestSchema,
      requestMessage,
      responseSchema: AttenuateBudgetResultSchema,
      options,
      autoGenerateIdempotencyKey: false,
    });
    if (result.grantedLimit === undefined) {
      throw new IpcError(
        "COMPATIBILITY",
        "AttenuateBudgetResult is missing grantedLimit",
      );
    }
    return result;
  }

  async recordExternalUsage(
    params: RecordExternalUsageParams,
    options: CallOptions = {},
  ): Promise<RecordExternalUsageResult> {
    requireNonEmptyToken("provider", params.provider);
    requireNonEmptyToken("model", params.model);
    if (params.recordedAtMs < 0n) {
      throw new IpcError(
        "INVALID_CONFIG",
        "recordedAtMs must be non-negative",
      );
    }
    const requestMessage = create(RecordExternalUsageRequestSchema, {
      schema: this.#client.schemaIdentity(),
      provider: params.provider,
      model: params.model,
      usage: toUsageWire(params.usage),
      recordedAtMs: params.recordedAtMs,
    });
    const { result } = await this.#client.call({
      method: RECORD_EXTERNAL_USAGE_METHOD,
      semantics: SIDE_EFFECTING_SEMANTICS,
      requestSchema: RecordExternalUsageRequestSchema,
      requestMessage,
      responseSchema: RecordExternalUsageResultSchema,
      options,
      autoGenerateIdempotencyKey: false,
    });
    return result;
  }
}

function requireNonEmptyToken(field: string, value: string): void {
  if (value.trim().length === 0 || value.includes("\0")) {
    throw new IpcError(
      "INVALID_CONFIG",
      `${field} must be non-empty and must not contain NUL`,
    );
  }
}

function toUsageWire(usage: LlmUsageInput): LlmUsageWire {
  for (const [name, value] of [
    ["apiCalls", usage.apiCalls],
    ["promptTokens", usage.promptTokens],
    ["completionTokens", usage.completionTokens],
    ["totalTokens", usage.totalTokens],
    ["costMicrocredits", usage.costMicrocredits],
  ] as const) {
    if (value < 0n) {
      throw new IpcError("INVALID_CONFIG", `usage.${name} must be non-negative`);
    }
  }
  return create(LlmUsageWireSchema, {
    apiCalls: usage.apiCalls,
    promptTokens: usage.promptTokens,
    completionTokens: usage.completionTokens,
    totalTokens: usage.totalTokens,
    costMicrocredits: usage.costMicrocredits,
  });
}
