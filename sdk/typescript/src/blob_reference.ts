import { create } from "@bufbuild/protobuf";
import { createHash, timingSafeEqual } from "node:crypto";

import {
  MintReferenceRequestSchema,
  MintReferenceResultSchema,
  ReadReferenceRequestSchema,
  ReadReferenceResultSchema,
  ReleaseReferenceRequestSchema,
  ReleaseReferenceResultSchema,
  type MintReferenceResult,
  type ReadReferenceResult,
  type ReleaseReferenceResult,
} from "../../../gen/typescript/nlos/sabi/v1/blob_reference_pb.ts";
import { type MethodSemantics } from "./common.ts";
import { IpcError, equalBytes } from "./local_rpc.ts";
import { type Ed25519Signer } from "./principal_session.ts";
import {
  SABI_ID_BYTES,
  SHA256_DIGEST_BYTES,
  SabiServiceClient,
  SabiServiceError,
  connectAuthenticatedServiceClient,
  connectServiceClient,
  copyBytes,
  type CallOptions,
  type RpcTarget,
  type SabiClientOptions,
  type ServiceErrorContext,
} from "./sabi_client.ts";
import { type ServiceRequirement } from "./service_directory.ts";

export const BLOB_REFERENCE_SCHEMA = "nlos.sabi.BlobReference";
export const BLOB_REFERENCE_MAJOR = 1;
export const BLOB_REFERENCE_MINOR = 0;
export const BLOB_REFERENCE_SERVICE = "blob_reference";
export const MAX_BLOB_CHUNK_BYTES = 65_408;
export const MAX_BLOB_FRAME_BYTES = 65_536;

export const MINT_REFERENCE_METHOD = "mint_reference";
export const READ_REFERENCE_METHOD = "read_reference";
export const RELEASE_REFERENCE_METHOD = "release_reference";

const MAX_BLOB_CHUNK_BIGINT = BigInt(MAX_BLOB_CHUNK_BYTES);
const MAX_I64_BIGINT = (1n << 63n) - 1n;

const MINT_SEMANTICS: MethodSemantics = {
  sideEffecting: true,
  longRunning: true,
};
const READ_RELEASE_SEMANTICS: MethodSemantics = {
  sideEffecting: true,
  longRunning: false,
};

export type BlobValidationErrorCode =
  | "INVALID_RECIPIENT_PRINCIPAL"
  | "INVALID_REFERENCE_ID"
  | "INVALID_CHUNK_BOUND"
  | "INVALID_PATH_HANDLE"
  | "INVALID_DIGEST"
  | "INVALID_ARGUMENT"
  | "UNALIGNED_OFFSET"
  | "HOLDER_MISMATCH"
  | "OFFSET_MISMATCH"
  | "CHUNK_DIGEST_MISMATCH";

/**
 * Client-side validation / integrity error for `BlobReferenceClient`.
 * Error messages strictly omit raw blob bytes, receipts, keys, and digests.
 */
export class BlobReferenceValidationError extends Error {
  readonly code: BlobValidationErrorCode;

  constructor(code: BlobValidationErrorCode, message: string) {
    super(message);
    this.name = "BlobReferenceValidationError";
    this.code = code;
  }
}

export class BlobReferenceError extends SabiServiceError {
  constructor(context: ServiceErrorContext) {
    super(context);
    this.name = "BlobReferenceError";
  }
}

export interface MintReferenceParams {
  readonly pathHandle: string;
  readonly fileDigestSha256: Uint8Array;
  readonly sizeBytes: bigint;
  readonly maxReads: bigint;
  readonly maxChunkBytes: bigint;
  readonly validUntilMs: bigint;
  readonly recipientPrincipalId: Uint8Array;
}

export interface ReadReferenceParams {
  readonly referenceId: Uint8Array;
  readonly offset: bigint;
  readonly lengthBytes: bigint;
  readonly chunkSizeBytes?: bigint;
  readonly expectedRecipientPrincipalId?: Uint8Array;
}

interface TrackedBlobToken {
  readonly recipientPrincipalId: Uint8Array;
  readonly maxChunkBytes: bigint;
  readonly sizeBytes: bigint;
}

/**
 * Thin SABI client for `nlos.sabi.BlobReference` (`v1.0`). Enforces explicit
 * 16-byte `recipientPrincipalId`, `1..=65408` `maxChunkBytes`, `<= 65536`
 * encoded payload frame bounds, aligned offsets, holder principal checks,
 * per-chunk SHA-256 verification, and zero leakage of raw blob/key/receipt bytes.
 */
export class BlobReferenceClient {
  readonly #client: SabiServiceClient;
  readonly #knownTokens = new Map<string, TrackedBlobToken>();

  constructor(target: RpcTarget, options: SabiClientOptions = {}) {
    this.#client = new SabiServiceClient(
      target,
      {
        service: BLOB_REFERENCE_SERVICE,
        schemaName: BLOB_REFERENCE_SCHEMA,
        schemaMajor: BLOB_REFERENCE_MAJOR,
        schemaMinor: BLOB_REFERENCE_MINOR,
        maxPayloadBytes: MAX_BLOB_FRAME_BYTES,
        createError: (context) => new BlobReferenceError(context),
      },
      options,
    );
  }

  static fromEndpoint(
    endpoint: string,
    options: SabiClientOptions = {},
  ): BlobReferenceClient {
    return new BlobReferenceClient(endpoint, options);
  }

  static async connect(
    endpoint: string,
    options: SabiClientOptions = {},
  ): Promise<BlobReferenceClient> {
    return connectServiceClient(
      endpoint,
      (rpc, opts) => new BlobReferenceClient(rpc, opts),
      options,
    );
  }

  static async connectAuthenticated(
    endpoint: string,
    signer: Ed25519Signer,
    principalId: Uint8Array,
    options: Omit<SabiClientOptions, "principalId"> = {},
  ): Promise<BlobReferenceClient> {
    return connectAuthenticatedServiceClient(
      endpoint,
      signer,
      principalId,
      (rpc, opts) => new BlobReferenceClient(rpc, opts),
      options,
    );
  }

  static serviceRequirement(): ServiceRequirement {
    return {
      service: BLOB_REFERENCE_SERVICE,
      schemaName: BLOB_REFERENCE_SCHEMA,
      major: BLOB_REFERENCE_MAJOR,
      minimumMinor: BLOB_REFERENCE_MINOR,
    };
  }

  get identity() {
    return this.#client.identity;
  }

  close(): void {
    this.#client.close();
  }

  async mintReference(
    params: MintReferenceParams,
    options: CallOptions = {},
  ): Promise<MintReferenceResult> {
    validateRelativePathHandle(params.pathHandle);
    if (params.fileDigestSha256.byteLength !== SHA256_DIGEST_BYTES) {
      throw new BlobReferenceValidationError(
        "INVALID_DIGEST",
        `fileDigestSha256 must contain exactly ${SHA256_DIGEST_BYTES} bytes`,
      );
    }
    if (
      params.recipientPrincipalId === undefined ||
      params.recipientPrincipalId.byteLength !== SABI_ID_BYTES
    ) {
      throw new BlobReferenceValidationError(
        "INVALID_RECIPIENT_PRINCIPAL",
        `recipientPrincipalId must contain exactly ${SABI_ID_BYTES} bytes`,
      );
    }
    if (params.sizeBytes < 0n) {
      throw new BlobReferenceValidationError(
        "INVALID_ARGUMENT",
        "sizeBytes must be non-negative",
      );
    }
    if (params.maxReads <= 0n) {
      throw new BlobReferenceValidationError(
        "INVALID_ARGUMENT",
        "maxReads must be positive",
      );
    }
    validateChunkBound("maxChunkBytes", params.maxChunkBytes);
    if (params.validUntilMs <= 0n || params.validUntilMs > MAX_I64_BIGINT) {
      throw new BlobReferenceValidationError(
        "INVALID_ARGUMENT",
        "validUntilMs must be positive and within int64 range",
      );
    }

    const requestMessage = create(MintReferenceRequestSchema, {
      schema: this.#client.schemaIdentity(),
      pathHandle: params.pathHandle,
      fileDigestSha256: copyBytes(params.fileDigestSha256),
      sizeBytes: params.sizeBytes,
      maxReads: params.maxReads,
      maxChunkBytes: params.maxChunkBytes,
      validUntilMs: params.validUntilMs,
      recipientPrincipalId: copyBytes(params.recipientPrincipalId),
    });

    const { result } = await this.#client.call({
      method: MINT_REFERENCE_METHOD,
      semantics: MINT_SEMANTICS,
      requestSchema: MintReferenceRequestSchema,
      requestMessage,
      responseSchema: MintReferenceResultSchema,
      options,
      autoGenerateIdempotencyKey: true,
      redactProtobufDecodeError: true,
    });

    if (result.referenceId.byteLength !== SABI_ID_BYTES) {
      throw new IpcError(
        "COMPATIBILITY",
        `MintReferenceResult referenceId must contain ${SABI_ID_BYTES} bytes`,
      );
    }
    this.#knownTokens.set(tokenKey(result.referenceId), {
      recipientPrincipalId: copyBytes(params.recipientPrincipalId),
      maxChunkBytes: params.maxChunkBytes,
      sizeBytes: params.sizeBytes,
    });
    return result;
  }

  async readReference(
    params: ReadReferenceParams,
    options: CallOptions = {},
  ): Promise<ReadReferenceResult> {
    if (params.referenceId.byteLength !== SABI_ID_BYTES) {
      throw new BlobReferenceValidationError(
        "INVALID_REFERENCE_ID",
        `referenceId must contain exactly ${SABI_ID_BYTES} bytes`,
      );
    }
    if (params.offset < 0n) {
      throw new BlobReferenceValidationError(
        "UNALIGNED_OFFSET",
        "blob read offset must be non-negative",
      );
    }
    validateChunkBound("lengthBytes", params.lengthBytes);

    if (params.expectedRecipientPrincipalId !== undefined) {
      if (params.expectedRecipientPrincipalId.byteLength !== SABI_ID_BYTES) {
        throw new BlobReferenceValidationError(
          "INVALID_RECIPIENT_PRINCIPAL",
          `expectedRecipientPrincipalId must contain exactly ${SABI_ID_BYTES} bytes`,
        );
      }
    }
    if (params.chunkSizeBytes !== undefined) {
      validateChunkBound("chunkSizeBytes", params.chunkSizeBytes);
    }

    const tracked = this.#knownTokens.get(tokenKey(params.referenceId));
    const expectedRecipient =
      params.expectedRecipientPrincipalId ?? tracked?.recipientPrincipalId;
    if (
      expectedRecipient !== undefined &&
      !equalBytes(this.#client.identity.principalId, expectedRecipient)
    ) {
      throw new BlobReferenceValidationError(
        "HOLDER_MISMATCH",
        "authenticated session principal does not match blob recipient",
      );
    }

    const blockSize = params.chunkSizeBytes ?? tracked?.maxChunkBytes;
    if (blockSize !== undefined && params.offset % blockSize !== 0n) {
      throw new BlobReferenceValidationError(
        "UNALIGNED_OFFSET",
        "blob read offset must align to mint-time chunk size",
      );
    }

    const requestMessage = create(ReadReferenceRequestSchema, {
      schema: this.#client.schemaIdentity(),
      referenceId: copyBytes(params.referenceId),
      offset: params.offset,
      lengthBytes: params.lengthBytes,
    });

    const { result } = await this.#client.call({
      method: READ_REFERENCE_METHOD,
      semantics: READ_RELEASE_SEMANTICS,
      requestSchema: ReadReferenceRequestSchema,
      requestMessage,
      responseSchema: ReadReferenceResultSchema,
      options,
      autoGenerateIdempotencyKey: true,
      redactProtobufDecodeError: true,
    });

    if (result.chunk.byteLength > MAX_BLOB_CHUNK_BYTES) {
      throw new IpcError(
        "FRAME_TOO_LARGE",
        `BlobReference chunk exceeds ${MAX_BLOB_CHUNK_BYTES} bytes`,
      );
    }
    if (BigInt(result.chunk.byteLength) > params.lengthBytes) {
      throw new IpcError(
        "COMPATIBILITY",
        "BlobReference chunk length exceeds requested lengthBytes",
      );
    }
    if (result.offset !== params.offset) {
      throw new BlobReferenceValidationError(
        "OFFSET_MISMATCH",
        "BlobReference response offset does not match request offset",
      );
    }
    if (result.chunkDigestSha256.byteLength !== SHA256_DIGEST_BYTES) {
      throw new BlobReferenceValidationError(
        "INVALID_DIGEST",
        `BlobReference chunk digest must contain ${SHA256_DIGEST_BYTES} bytes`,
      );
    }

    const actualDigest = createHash("sha256").update(result.chunk).digest();
    if (!timingSafeEqual(actualDigest, result.chunkDigestSha256)) {
      throw new BlobReferenceValidationError(
        "CHUNK_DIGEST_MISMATCH",
        "BlobReference chunk SHA-256 verification failed",
      );
    }
    return result;
  }

  async releaseReference(
    referenceId: Uint8Array,
    options: CallOptions = {},
  ): Promise<ReleaseReferenceResult> {
    if (referenceId.byteLength !== SABI_ID_BYTES) {
      throw new BlobReferenceValidationError(
        "INVALID_REFERENCE_ID",
        `referenceId must contain exactly ${SABI_ID_BYTES} bytes`,
      );
    }
    const requestMessage = create(ReleaseReferenceRequestSchema, {
      schema: this.#client.schemaIdentity(),
      referenceId: copyBytes(referenceId),
    });

    const { result } = await this.#client.call({
      method: RELEASE_REFERENCE_METHOD,
      semantics: READ_RELEASE_SEMANTICS,
      requestSchema: ReleaseReferenceRequestSchema,
      requestMessage,
      responseSchema: ReleaseReferenceResultSchema,
      options,
      autoGenerateIdempotencyKey: true,
      redactProtobufDecodeError: true,
    });
    if (result.released) {
      this.#knownTokens.delete(tokenKey(referenceId));
    }
    return result;
  }
}

function validateChunkBound(field: string, value: bigint): void {
  if (value <= 0n) {
    throw new BlobReferenceValidationError(
      "INVALID_CHUNK_BOUND",
      `${field} must be within 1..=${MAX_BLOB_CHUNK_BYTES}`,
    );
  }
  if (value > MAX_BLOB_CHUNK_BIGINT) {
    throw new IpcError(
      "FRAME_TOO_LARGE",
      `${field} exceeds MAX_BLOB_CHUNK_BYTES (${MAX_BLOB_CHUNK_BYTES})`,
    );
  }
}

function validateRelativePathHandle(pathHandle: string): void {
  if (
    pathHandle.length === 0 ||
    pathHandle.length > 1024 ||
    pathHandle.includes("\0") ||
    pathHandle.includes("\\")
  ) {
    throw new BlobReferenceValidationError(
      "INVALID_PATH_HANDLE",
      "pathHandle must be a bounded relative POSIX path",
    );
  }
  const segments = pathHandle.split("/");
  if (
    segments.length > 32 ||
    segments.some(
      (segment) =>
        segment.length === 0 ||
        segment === "." ||
        segment === ".." ||
        segment.length > 255,
    )
  ) {
    throw new BlobReferenceValidationError(
      "INVALID_PATH_HANDLE",
      "pathHandle contains invalid path segments",
    );
  }
}

function tokenKey(referenceId: Uint8Array): string {
  return Buffer.from(referenceId).toString("hex");
}
