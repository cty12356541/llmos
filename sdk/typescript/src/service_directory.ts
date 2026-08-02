import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { randomBytes } from "node:crypto";

import {
  EnvelopeSchema,
  ExchangeRequestSchema,
  SchemaIdentitySchema,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import {
  DirectoryErrorCode,
  LocalTransportKind,
  NegotiateServiceRequestSchema,
  NegotiateServiceResponseSchema,
  type ServiceBinding,
} from "../../../gen/typescript/nlos/sabi/v1/service_directory_pb.ts";
import {
  IpcError,
  LocalRpcClient,
  type TransportConfig,
} from "./local_rpc.ts";

const ENVELOPE_SCHEMA = "nlos.sabi.Envelope";
const DIRECTORY_SCHEMA = "nlos.sabi.ServiceDirectory";
const DIRECTORY_SERVICE = "service_directory";
const MAX_DIRECTORY_PAYLOAD_BYTES = 64 * 1024;

export interface ServiceRequirement {
  service: string;
  schemaName: string;
  major: number;
  minimumMinor?: number;
  requiredFeatureIds?: readonly number[];
}

export interface ConnectedService {
  binding: ServiceBinding;
  client: LocalRpcClient;
}

export class DirectoryNegotiationError extends Error {
  readonly code: DirectoryErrorCode;
  readonly service: string;

  constructor(code: DirectoryErrorCode, service: string) {
    super(`ServiceDirectory negotiation failed with ${DirectoryErrorCode[code]}`);
    this.name = "DirectoryNegotiationError";
    this.code = code;
    this.service = service;
  }
}

export class ServiceDirectoryClient {
  readonly #rpc: LocalRpcClient;

  private constructor(rpc: LocalRpcClient) {
    this.#rpc = rpc;
  }

  static async connect(
    trustedBootstrapEndpoint: string,
    config: Partial<TransportConfig> = {},
  ): Promise<ServiceDirectoryClient> {
    return new ServiceDirectoryClient(
      await LocalRpcClient.connect(trustedBootstrapEndpoint, config),
    );
  }

  static async negotiateAndConnect(
    trustedBootstrapEndpoint: string,
    requirement: ServiceRequirement,
    config: Partial<TransportConfig> = {},
  ): Promise<ConnectedService> {
    const directory = await ServiceDirectoryClient.connect(
      trustedBootstrapEndpoint,
      config,
    );
    let binding: ServiceBinding;
    try {
      binding = await directory.negotiate(requirement);
    } finally {
      directory.close();
    }
    const endpoint = requireCompatibleBinding(binding, requirement);
    return {
      binding,
      client: await LocalRpcClient.connect(endpoint, config),
    };
  }

  async negotiate(requirement: ServiceRequirement): Promise<ServiceBinding> {
    validateRequirement(requirement);
    const transport = platformTransport();
    const directoryRequest = create(NegotiateServiceRequestSchema, {
      schema: directoryIdentity(),
      service: requirement.service,
      schemaName: requirement.schemaName,
      major: requirement.major,
      minimumMinor: requirement.minimumMinor ?? 0,
      requiredFeatureIds: [...(requirement.requiredFeatureIds ?? [])],
      supportedTransportKinds: [transport],
    });
    const payload = toBinary(NegotiateServiceRequestSchema, directoryRequest);
    if (payload.byteLength > MAX_DIRECTORY_PAYLOAD_BYTES) {
      throw new IpcError(
        "FRAME_TOO_LARGE",
        "ServiceDirectory request exceeds 64 KiB",
      );
    }
    const response = await this.#rpc.exchange(
      create(ExchangeRequestSchema, {
        envelope: create(EnvelopeSchema, {
          schema: create(SchemaIdentitySchema, {
            name: ENVELOPE_SCHEMA,
            major: 1,
            minor: 0,
          }),
          requestId: Uint8Array.from(randomBytes(16)),
          service: DIRECTORY_SERVICE,
          method: "negotiate",
          payload,
        }),
      }),
    );
    const responsePayload = response.envelope?.payload;
    if (
      responsePayload === undefined ||
      responsePayload.byteLength > MAX_DIRECTORY_PAYLOAD_BYTES
    ) {
      throw new IpcError(
        "COMPATIBILITY",
        "ServiceDirectory response payload is missing or oversized",
      );
    }
    let negotiation;
    try {
      negotiation = fromBinary(
        NegotiateServiceResponseSchema,
        responsePayload,
      );
    } catch (cause) {
      throw new IpcError("COMPATIBILITY", "malformed ServiceDirectory response", {
        cause,
      });
    }
    requireDirectoryIdentity(negotiation.schema);
    if (negotiation.result.case === "error") {
      if (!validDirectoryErrorCode(negotiation.result.value.code)) {
        throw new IpcError(
          "COMPATIBILITY",
          "ServiceDirectory returned an unknown error code",
        );
      }
      throw new DirectoryNegotiationError(
        negotiation.result.value.code,
        negotiation.result.value.service,
      );
    }
    if (negotiation.result.case !== "binding") {
      throw new IpcError(
        "COMPATIBILITY",
        "ServiceDirectory response is missing a result",
      );
    }
    requireCompatibleBinding(negotiation.result.value, requirement);
    return negotiation.result.value;
  }

  close(): void {
    this.#rpc.close();
  }
}

function directoryIdentity() {
  return create(SchemaIdentitySchema, {
    name: DIRECTORY_SCHEMA,
    major: 1,
    minor: 0,
  });
}

function requireDirectoryIdentity(
  identity: ReturnType<typeof directoryIdentity> | undefined,
): void {
  if (
    identity === undefined ||
    identity.name !== DIRECTORY_SCHEMA ||
    identity.major !== 1 ||
    identity.criticalExtensionIds.length !== 0
  ) {
    throw new IpcError(
      "COMPATIBILITY",
      "incompatible ServiceDirectory response identity",
    );
  }
}

function validateRequirement(requirement: ServiceRequirement): void {
  if (
    !validName(requirement.service) ||
    !validName(requirement.schemaName) ||
    !Number.isSafeInteger(requirement.major) ||
    requirement.major <= 0 ||
    !Number.isSafeInteger(requirement.minimumMinor ?? 0) ||
    (requirement.minimumMinor ?? 0) < 0 ||
    !validFeatureIds(requirement.requiredFeatureIds ?? [])
  ) {
    throw new IpcError("INVALID_CONFIG", "invalid service requirement");
  }
}

function requireCompatibleBinding(
  binding: ServiceBinding,
  requirement: ServiceRequirement,
): string {
  const { candidate, endpoint } = binding;
  if (
    candidate === undefined ||
    endpoint === undefined ||
    candidate.bindingId.byteLength !== 16 ||
    candidate.generation <= 0n ||
    candidate.service !== requirement.service ||
    candidate.version === undefined ||
    candidate.version.schemaName !== requirement.schemaName ||
    candidate.version.major !== requirement.major ||
    candidate.version.minor < (requirement.minimumMinor ?? 0) ||
    !validFeatureIds(candidate.featureIds) ||
    !(requirement.requiredFeatureIds ?? []).every((feature) =>
      candidate.featureIds.includes(feature),
    ) ||
    candidate.transportKinds.length !== 1 ||
    candidate.transportKinds[0] !== endpoint.kind ||
    endpoint.kind !== platformTransport() ||
    endpoint.address.length === 0 ||
    endpoint.address.length > 4096 ||
    endpoint.address.includes("\0")
  ) {
    throw new IpcError(
      "COMPATIBILITY",
      "ServiceDirectory returned an incompatible binding",
    );
  }
  return endpoint.address;
}

function validDirectoryErrorCode(code: DirectoryErrorCode): boolean {
  return (
    code === DirectoryErrorCode.INVALID_REQUEST ||
    code === DirectoryErrorCode.NOT_FOUND ||
    code === DirectoryErrorCode.SCHEMA_UNSUPPORTED ||
    code === DirectoryErrorCode.VERSION_UNSUPPORTED ||
    code === DirectoryErrorCode.REQUIRED_FEATURE_UNSUPPORTED ||
    code === DirectoryErrorCode.TRANSPORT_UNSUPPORTED
  );
}

function platformTransport(): LocalTransportKind {
  return process.platform === "win32"
    ? LocalTransportKind.WINDOWS_NAMED_PIPE
    : LocalTransportKind.UNIX_SOCKET;
}

function validName(value: string): boolean {
  return value.length > 0 && value.length <= 255 && !value.includes("\0");
}

function validFeatureIds(values: readonly number[]): boolean {
  return (
    values.length <= 128 &&
    values.every(
      (value, index) =>
        Number.isSafeInteger(value) &&
        value > 0 &&
        (index === 0 || values[index - 1]! < value),
    )
  );
}
