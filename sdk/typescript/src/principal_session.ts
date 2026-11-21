import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import {
  createHash,
  createPrivateKey,
  createPublicKey,
  randomBytes,
  sign as cryptoSign,
  type KeyObject,
} from "node:crypto";
import { endianness } from "node:os";

import {
  CallerIdentitySchema,
  SchemaIdentitySchema,
  type CallerIdentity,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import {
  PrincipalHandshakeAttestationSchema,
  PrincipalHandshakeChallengeSchema,
  type PrincipalHandshakeChallenge,
} from "../../../gen/typescript/nlos/sabi/v1/principal_handshake_pb.ts";
import {
  CommonSemanticsError,
  validateSchemaCompatibility,
} from "./common.ts";
import {
  IpcError,
  LocalRpcClient,
  connectSocket,
  ensureFrameBound,
  readFrame,
  resolveTransportConfig,
  validateEndpoint,
  writeFrame,
  type TransportConfig,
} from "./local_rpc.ts";

export const HANDSHAKE_MESSAGE_DOMAIN = Buffer.from(
  "llmos/principal-handshake/v1",
  "ascii",
);
export const CHANNEL_BINDING_DOMAIN = Buffer.from(
  "llmos/ipc-channel-binding/v1",
  "ascii",
);

export const ED25519_SEED_BYTES = 32;
export const ED25519_SIGNATURE_BYTES = 64;
export const HANDSHAKE_NONCE_BYTES = 32;
export const HANDSHAKE_SIGNATURE_BYTES = 64;
export const HANDSHAKE_PRINCIPAL_ID_BYTES = 16;
export const MAX_HANDSHAKE_CHANNEL_BINDING_BYTES = 256;

export const HANDSHAKE_SCHEMA_NAME = "nlos.sabi.PrincipalHandshake";
export const HANDSHAKE_SCHEMA_MAJOR = 1;
export const HANDSHAKE_SCHEMA_MINOR = 0;

const ID_BYTES = 16;
const ED25519_PKCS8_PREFIX = Buffer.from(
  "302e020100300506032b657004220420",
  "hex",
);

/**
 * Holds an Ed25519 signing key constructed from a 32-byte raw seed using
 * Node's built-in `node:crypto` PKCS#8 DER encoding without external crypto
 * dependencies or exposing private key bytes in errors or inspection output.
 */
export class Ed25519Signer {
  readonly #privateKey: KeyObject;
  readonly #publicKey: Uint8Array;

  constructor(seed: Uint8Array) {
    if (seed.byteLength !== ED25519_SEED_BYTES) {
      throw new IpcError(
        "INVALID_CONFIG",
        `Ed25519 seed must contain exactly ${ED25519_SEED_BYTES} bytes`,
      );
    }
    const pkcs8 = Buffer.allocUnsafe(
      ED25519_PKCS8_PREFIX.byteLength + ED25519_SEED_BYTES,
    );
    ED25519_PKCS8_PREFIX.copy(pkcs8, 0);
    pkcs8.set(seed, ED25519_PKCS8_PREFIX.byteLength);
    try {
      this.#privateKey = createPrivateKey({
        key: pkcs8,
        format: "der",
        type: "pkcs8",
      });
    } finally {
      pkcs8.fill(0);
    }
    const spki = createPublicKey(this.#privateKey).export({
      format: "der",
      type: "spki",
    });
    this.#publicKey = Uint8Array.from(spki.subarray(-ED25519_SEED_BYTES));
  }

  static fromSeed(seed: Uint8Array): Ed25519Signer {
    return new Ed25519Signer(seed);
  }

  get publicKey(): Uint8Array {
    return Uint8Array.from(this.#publicKey);
  }

  sign(message: Uint8Array): Uint8Array {
    const signature = cryptoSign(
      null,
      Buffer.from(message.buffer, message.byteOffset, message.byteLength),
      this.#privateKey,
    );
    if (signature.byteLength !== ED25519_SIGNATURE_BYTES) {
      throw new IpcError(
        "HANDSHAKE",
        "Ed25519 signer produced unexpected signature length",
      );
    }
    return Uint8Array.from(signature);
  }

  [Symbol.for("nodejs.util.inspect.custom")](): string {
    return `Ed25519Signer { publicKey: ${Buffer.from(this.#publicKey).toString("hex")} }`;
  }

  toJSON(): { publicKeyHex: string } {
    return {
      publicKeyHex: Buffer.from(this.#publicKey).toString("hex"),
    };
  }
}

export interface SessionIdentityInit {
  readonly principalId: Uint8Array;
  readonly applicationId: Uint8Array;
  readonly processId: Uint8Array;
  readonly processGeneration?: bigint;
}

export interface DeriveSessionIdentityOptions {
  readonly applicationId?: Uint8Array;
  readonly processId?: Uint8Array;
  readonly processGeneration?: bigint;
}

/**
 * Caller identity carried on SABI request envelopes; principal is bound by
 * the ADR-0011 handshake while application/process default to fresh random IDs.
 */
export class SessionIdentity {
  readonly principalId: Uint8Array;
  readonly applicationId: Uint8Array;
  readonly processId: Uint8Array;
  readonly processGeneration: bigint;

  constructor(init: SessionIdentityInit) {
    const generation = init.processGeneration ?? 1n;
    requireIdentityLength("principalId", init.principalId, ID_BYTES);
    requireIdentityLength("applicationId", init.applicationId, ID_BYTES);
    requireIdentityLength("processId", init.processId, ID_BYTES);
    if (generation <= 0n) {
      throw new IpcError(
        "INVALID_CONFIG",
        "processGeneration must be positive",
      );
    }
    this.principalId = Uint8Array.from(init.principalId);
    this.applicationId = Uint8Array.from(init.applicationId);
    this.processId = Uint8Array.from(init.processId);
    this.processGeneration = generation;
  }

  static derive(
    principalId: Uint8Array,
    options: DeriveSessionIdentityOptions = {},
  ): SessionIdentity {
    return new SessionIdentity({
      principalId,
      applicationId: options.applicationId ?? Uint8Array.from(randomBytes(ID_BYTES)),
      processId: options.processId ?? Uint8Array.from(randomBytes(ID_BYTES)),
      processGeneration: options.processGeneration ?? 1n,
    });
  }

  static from(identity: SessionIdentity | SessionIdentityInit): SessionIdentity {
    return identity instanceof SessionIdentity
      ? identity
      : new SessionIdentity(identity);
  }

  toCallerIdentity(): CallerIdentity {
    return create(CallerIdentitySchema, {
      principalId: Uint8Array.from(this.principalId),
      applicationId: Uint8Array.from(this.applicationId),
      processId: Uint8Array.from(this.processId),
      processGeneration: this.processGeneration,
    });
  }
}

export interface AuthenticatedSession {
  readonly client: LocalRpcClient;
  readonly identity: SessionIdentity;
}

export interface ConnectAuthenticatedSessionOptions {
  readonly transport?: Partial<TransportConfig>;
  readonly applicationId?: Uint8Array;
  readonly processId?: Uint8Array;
  readonly processGeneration?: bigint;
}

/**
 * Encodes an endpoint path into the exact byte form hashed by Rust
 * `crates/nlos-ipc/src/handshake/transport.rs:97-108`.
 */
export function endpointPathBytes(
  endpoint: string,
  platform: NodeJS.Platform = process.platform,
): Uint8Array {
  validateEndpoint(endpoint);
  if (platform === "win32") {
    const utf16 = Buffer.from(endpoint, "utf16le");
    if (endianness() === "BE") {
      utf16.swap16();
    }
    return Uint8Array.from(utf16);
  }
  return Uint8Array.from(Buffer.from(endpoint, "utf8"));
}

/**
 * Computes `SHA256(CHANNEL_BINDING_DOMAIN || endpointPathBytes(endpoint))`.
 */
export function endpointChannelBinding(
  endpoint: string,
  platform: NodeJS.Platform = process.platform,
): Uint8Array {
  const hash = createHash("sha256");
  hash.update(CHANNEL_BINDING_DOMAIN);
  hash.update(endpointPathBytes(endpoint, platform));
  return Uint8Array.from(hash.digest());
}

/**
 * Computes `SHA256(HANDSHAKE_MESSAGE_DOMAIN || nonce || principalId || channelBinding)`.
 */
export function principalHandshakeMessage(
  nonce: Uint8Array,
  principalId: Uint8Array,
  channelBinding: Uint8Array,
): Uint8Array {
  requireHandshakeLength("nonce", nonce, HANDSHAKE_NONCE_BYTES);
  requireHandshakeLength(
    "principalId",
    principalId,
    HANDSHAKE_PRINCIPAL_ID_BYTES,
  );
  requireChannelBinding(channelBinding);
  const hash = createHash("sha256");
  hash.update(HANDSHAKE_MESSAGE_DOMAIN);
  hash.update(nonce);
  hash.update(principalId);
  hash.update(channelBinding);
  return Uint8Array.from(hash.digest());
}

export function decodeChallenge(wire: Uint8Array): PrincipalHandshakeChallenge {
  let challenge: PrincipalHandshakeChallenge;
  try {
    challenge = fromBinary(PrincipalHandshakeChallengeSchema, wire);
  } catch (cause) {
    throw new IpcError(
      "COMPATIBILITY",
      "malformed PrincipalHandshakeChallenge",
      { cause },
    );
  }
  try {
    validateSchemaCompatibility(challenge.schema, HANDSHAKE_SCHEMA_NAME);
  } catch (error) {
    if (error instanceof CommonSemanticsError) {
      throw new IpcError("COMPATIBILITY", error.message, { cause: error });
    }
    throw error;
  }
  requireHandshakeLength("nonce", challenge.nonce, HANDSHAKE_NONCE_BYTES);
  return challenge;
}

export function buildAttestation(
  principalId: Uint8Array,
  nonce: Uint8Array,
  channelBinding: Uint8Array,
  signature: Uint8Array,
): Uint8Array {
  requireHandshakeLength(
    "principalId",
    principalId,
    HANDSHAKE_PRINCIPAL_ID_BYTES,
  );
  requireHandshakeLength("nonce", nonce, HANDSHAKE_NONCE_BYTES);
  requireHandshakeLength(
    "signature",
    signature,
    HANDSHAKE_SIGNATURE_BYTES,
  );
  requireChannelBinding(channelBinding);
  const attestation = create(PrincipalHandshakeAttestationSchema, {
    schema: create(SchemaIdentitySchema, {
      name: HANDSHAKE_SCHEMA_NAME,
      major: HANDSHAKE_SCHEMA_MAJOR,
      minor: HANDSHAKE_SCHEMA_MINOR,
      criticalExtensionIds: [],
      nonCriticalExtensionIds: [],
    }),
    principalId: Uint8Array.from(principalId),
    nonce: Uint8Array.from(nonce),
    channelBinding: Uint8Array.from(channelBinding),
    signature: Uint8Array.from(signature),
  });
  return toBinary(PrincipalHandshakeAttestationSchema, attestation);
}

/**
 * Connects to a local IPC endpoint, performs the ADR-0011 Ed25519
 * challenge-response handshake, and returns a persistent `LocalRpcClient`
 * over that authenticated connection. Any failure closes the socket and
 * throws a typed `IpcError` without leaking key material.
 */
export async function connectAuthenticated(
  endpoint: string,
  signer: Ed25519Signer,
  principalId: Uint8Array,
  overrides: Partial<TransportConfig> = {},
): Promise<LocalRpcClient> {
  validateEndpoint(endpoint);
  if (principalId.byteLength !== HANDSHAKE_PRINCIPAL_ID_BYTES) {
    throw new IpcError(
      "INVALID_CONFIG",
      `principalId must contain exactly ${HANDSHAKE_PRINCIPAL_ID_BYTES} bytes`,
    );
  }
  const config = resolveTransportConfig(overrides);
  const socket = await connectSocket(endpoint, config.connectTimeoutMs);
  try {
    const challengeWire = await readFrame(
      socket,
      config.maximumFrameBytes,
      config.readTimeoutMs,
    );
    const challenge = decodeChallenge(challengeWire);
    const binding = endpointChannelBinding(endpoint);
    const digest = principalHandshakeMessage(
      challenge.nonce,
      principalId,
      binding,
    );
    let signature: Uint8Array;
    try {
      signature = signer.sign(digest);
    } catch {
      throw new IpcError("HANDSHAKE", "principal handshake signing failed");
    }
    const attestation = buildAttestation(
      principalId,
      challenge.nonce,
      binding,
      signature,
    );
    ensureFrameBound(attestation.byteLength, config.maximumFrameBytes);
    await writeFrame(socket, attestation, config.writeTimeoutMs);
    return LocalRpcClient.fromSocket(socket, config);
  } catch (error) {
    socket.destroy();
    if (error instanceof IpcError) {
      throw error;
    }
    throw new IpcError("HANDSHAKE", "principal handshake failed");
  }
}

export async function connectAuthenticatedSession(
  endpoint: string,
  signer: Ed25519Signer,
  principalId: Uint8Array,
  options: ConnectAuthenticatedSessionOptions = {},
): Promise<AuthenticatedSession> {
  const client = await connectAuthenticated(
    endpoint,
    signer,
    principalId,
    options.transport,
  );
  try {
    const identity = SessionIdentity.derive(principalId, {
      applicationId: options.applicationId,
      processId: options.processId,
      processGeneration: options.processGeneration,
    });
    return { client, identity };
  } catch (error) {
    client.close();
    throw error;
  }
}

function requireIdentityLength(
  field: string,
  value: Uint8Array,
  expected: number,
): void {
  if (value.byteLength !== expected) {
    throw new IpcError(
      "INVALID_CONFIG",
      `${field} must contain exactly ${expected} bytes`,
    );
  }
}

function requireHandshakeLength(
  field: string,
  value: Uint8Array,
  expected: number,
): void {
  if (value.byteLength !== expected) {
    throw new IpcError(
      "COMPATIBILITY",
      `handshake ${field} has ${value.byteLength} bytes; exactly ${expected} required`,
    );
  }
}

function requireChannelBinding(binding: Uint8Array): void {
  if (
    binding.byteLength === 0 ||
    binding.byteLength > MAX_HANDSHAKE_CHANNEL_BINDING_BYTES
  ) {
    throw new IpcError(
      "COMPATIBILITY",
      `handshake channel binding has ${binding.byteLength} bytes; 1..=${MAX_HANDSHAKE_CHANNEL_BINDING_BYTES} required`,
    );
  }
}
