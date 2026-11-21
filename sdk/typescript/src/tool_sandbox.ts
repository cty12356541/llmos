import { create } from "@bufbuild/protobuf";

import {
  InvokeToolRequestSchema,
  InvokeToolResultSchema,
  ListToolsRequestSchema,
  ListToolsResultSchema,
  type InvokeToolResult,
  type ListToolsResult,
  type ToolDescriptorWire,
} from "../../../gen/typescript/nlos/sabi/v1/tool_sandbox_pb.ts";
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

export const TOOL_SANDBOX_SCHEMA = "nlos.sabi.ToolSandbox";
export const TOOL_SANDBOX_MAJOR = 1;
export const TOOL_SANDBOX_MINOR = 0;
export const TOOL_SANDBOX_SERVICE = "tool_sandbox";
export const MAX_TOOL_SANDBOX_PAYLOAD_BYTES = 64 * 1024;

export const LIST_TOOLS_METHOD = "list_tools";
export const INVOKE_TOOL_METHOD = "invoke_tool";

const VALID_EFFECT_CLASSES = new Set([
  "READ_ONLY",
  "REVERSIBLE",
  "COMPENSATABLE",
  "IRREVERSIBLE",
]);

const LIST_TOOLS_SEMANTICS: MethodSemantics = {
  sideEffecting: false,
  longRunning: true,
};
const INVOKE_TOOL_SEMANTICS: MethodSemantics = {
  sideEffecting: true,
  longRunning: true,
};

export class ToolSandboxError extends SabiServiceError {
  constructor(context: ServiceErrorContext) {
    super(context);
    this.name = "ToolSandboxError";
  }
}

export interface InvokeToolParams {
  readonly tool: string;
  readonly argumentsJson: string;
}

/**
 * Thin SABI client for `nlos.sabi.ToolSandbox` (`v1.0`). Enforces the 64 KiB
 * payload frame bound, `deadlineMonotonicNs` on long-running queries, and
 * explicit `idempotencyKey` + `deadlineMonotonicNs` on `invokeTool`.
 */
export class ToolSandboxClient {
  readonly #client: SabiServiceClient;

  constructor(target: RpcTarget, options: SabiClientOptions = {}) {
    this.#client = new SabiServiceClient(
      target,
      {
        service: TOOL_SANDBOX_SERVICE,
        schemaName: TOOL_SANDBOX_SCHEMA,
        schemaMajor: TOOL_SANDBOX_MAJOR,
        schemaMinor: TOOL_SANDBOX_MINOR,
        maxPayloadBytes: MAX_TOOL_SANDBOX_PAYLOAD_BYTES,
        createError: (context) => new ToolSandboxError(context),
      },
      options,
    );
  }

  static fromEndpoint(
    endpoint: string,
    options: SabiClientOptions = {},
  ): ToolSandboxClient {
    return new ToolSandboxClient(endpoint, options);
  }

  static async connect(
    endpoint: string,
    options: SabiClientOptions = {},
  ): Promise<ToolSandboxClient> {
    return connectServiceClient(
      endpoint,
      (rpc, opts) => new ToolSandboxClient(rpc, opts),
      options,
    );
  }

  static async connectAuthenticated(
    endpoint: string,
    signer: Ed25519Signer,
    principalId: Uint8Array,
    options: Omit<SabiClientOptions, "principalId"> = {},
  ): Promise<ToolSandboxClient> {
    return connectAuthenticatedServiceClient(
      endpoint,
      signer,
      principalId,
      (rpc, opts) => new ToolSandboxClient(rpc, opts),
      options,
    );
  }

  static serviceRequirement(): ServiceRequirement {
    return {
      service: TOOL_SANDBOX_SERVICE,
      schemaName: TOOL_SANDBOX_SCHEMA,
      major: TOOL_SANDBOX_MAJOR,
      minimumMinor: TOOL_SANDBOX_MINOR,
    };
  }

  get identity() {
    return this.#client.identity;
  }

  close(): void {
    this.#client.close();
  }

  async listTools(options: CallOptions = {}): Promise<ListToolsResult> {
    const requestMessage = create(ListToolsRequestSchema, {
      schema: this.#client.schemaIdentity(),
    });
    const { result } = await this.#client.call({
      method: LIST_TOOLS_METHOD,
      semantics: LIST_TOOLS_SEMANTICS,
      requestSchema: ListToolsRequestSchema,
      requestMessage,
      responseSchema: ListToolsResultSchema,
      options,
    });
    for (const descriptor of result.tools) {
      validateToolDescriptor(descriptor);
    }
    return result;
  }

  async invokeTool(
    params: InvokeToolParams,
    options: CallOptions = {},
  ): Promise<InvokeToolResult> {
    if (params.tool.trim().length === 0 || params.tool.includes("\0")) {
      throw new IpcError(
        "INVALID_CONFIG",
        "tool name must be non-empty and must not contain NUL",
      );
    }
    const requestMessage = create(InvokeToolRequestSchema, {
      schema: this.#client.schemaIdentity(),
      tool: params.tool,
      argumentsJson: params.argumentsJson,
    });
    const { result } = await this.#client.call({
      method: INVOKE_TOOL_METHOD,
      semantics: INVOKE_TOOL_SEMANTICS,
      requestSchema: InvokeToolRequestSchema,
      requestMessage,
      responseSchema: InvokeToolResultSchema,
      options,
      autoGenerateIdempotencyKey: false,
      redactProtobufDecodeError: true,
    });
    return result;
  }
}

function validateToolDescriptor(descriptor: ToolDescriptorWire): void {
  if (
    descriptor.name.trim().length === 0 ||
    descriptor.name.includes("\0") ||
    !VALID_EFFECT_CLASSES.has(descriptor.effectClass)
  ) {
    throw new IpcError(
      "COMPATIBILITY",
      "ToolSandbox catalog contains invalid or undeclared tool descriptor",
    );
  }
  if (
    descriptor.effectClass === "COMPENSATABLE" &&
    descriptor.compensationHandler.trim().length === 0
  ) {
    throw new IpcError(
      "COMPATIBILITY",
      "COMPENSATABLE tool descriptor must declare a non-empty compensationHandler",
    );
  }
}
