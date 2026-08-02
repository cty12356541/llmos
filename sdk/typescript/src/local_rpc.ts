import { fromBinary, toBinary } from "@bufbuild/protobuf";
import { createConnection, type Socket } from "node:net";

import {
  ExchangeRequestSchema,
  ExchangeResponseSchema,
  type Envelope,
  type ExchangeRequest,
  type ExchangeResponse,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";

const MAXIMUM_SCHEMA_FRAME_BYTES = 1024 * 1024;
const SUPPORTED_SCHEMA = "nlos.sabi.Envelope";
const SUPPORTED_MAJOR = 1;

export type IpcErrorCode =
  | "INVALID_CONFIG"
  | "CONNECT"
  | "READ"
  | "WRITE"
  | "TIMEOUT"
  | "FRAME_TOO_LARGE"
  | "COMPATIBILITY"
  | "BACKPRESSURE"
  | "CONNECTION_UNUSABLE"
  | "REQUEST_ID_MISMATCH";

export class IpcError extends Error {
  readonly code: IpcErrorCode;

  constructor(code: IpcErrorCode, message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = "IpcError";
    this.code = code;
  }
}

export interface TransportConfig {
  maximumFrameBytes: number;
  connectTimeoutMs: number;
  readTimeoutMs: number;
  writeTimeoutMs: number;
}

const defaultConfig: TransportConfig = {
  maximumFrameBytes: MAXIMUM_SCHEMA_FRAME_BYTES,
  connectTimeoutMs: 5_000,
  readTimeoutMs: 5_000,
  writeTimeoutMs: 5_000,
};

export class LocalRpcClient {
  readonly #socket: Socket;
  readonly #config: TransportConfig;
  #inFlight = false;
  #usable = true;

  private constructor(socket: Socket, config: TransportConfig) {
    this.#socket = socket;
    this.#config = config;
    socket.on("error", () => {
      this.#usable = false;
    });
    socket.on("close", () => {
      this.#usable = false;
    });
  }

  static async connect(
    endpoint: string,
    overrides: Partial<TransportConfig> = {},
  ): Promise<LocalRpcClient> {
    if (endpoint.length === 0) {
      throw new IpcError("INVALID_CONFIG", "IPC endpoint must not be empty");
    }
    const config = validateConfig({ ...defaultConfig, ...overrides });
    const socket = createConnection({ path: endpoint });
    await waitForConnect(socket, config.connectTimeoutMs);
    return new LocalRpcClient(socket, config);
  }

  async exchange(request: ExchangeRequest): Promise<ExchangeResponse> {
    const requestEnvelope = requireCompatibleEnvelope(request.envelope);
    const wire = toBinary(ExchangeRequestSchema, request);
    ensureFrameBound(wire.byteLength, this.#config.maximumFrameBytes);
    if (this.#inFlight) {
      throw new IpcError(
        "BACKPRESSURE",
        "IPC client already has an in-flight call",
      );
    }
    if (!this.#usable) {
      throw new IpcError(
        "CONNECTION_UNUSABLE",
        "IPC connection is unusable; reconnect",
      );
    }

    this.#inFlight = true;
    try {
      const responsePromise = readFrame(
        this.#socket,
        this.#config.maximumFrameBytes,
        this.#config.readTimeoutMs,
      );
      try {
        await writeFrame(this.#socket, wire, this.#config.writeTimeoutMs);
      } catch (error) {
        void responsePromise.catch(() => undefined);
        throw error;
      }
      const responseWire = await responsePromise;
      const response = fromBinary(ExchangeResponseSchema, responseWire);
      const responseEnvelope = requireCompatibleEnvelope(response.envelope);
      if (!equalBytes(responseEnvelope.requestId, requestEnvelope.requestId)) {
        throw new IpcError(
          "REQUEST_ID_MISMATCH",
          "IPC response request_id does not match the request",
        );
      }
      return response;
    } catch (error) {
      this.#poison();
      if (error instanceof IpcError) {
        throw error;
      }
      throw new IpcError("READ", "IPC exchange failed", { cause: error });
    } finally {
      this.#inFlight = false;
    }
  }

  close(): void {
    this.#poison();
  }

  #poison(): void {
    this.#usable = false;
    this.#socket.destroy();
  }
}

function validateConfig(config: TransportConfig): TransportConfig {
  if (
    !Number.isSafeInteger(config.maximumFrameBytes) ||
    config.maximumFrameBytes <= 0 ||
    config.maximumFrameBytes > MAXIMUM_SCHEMA_FRAME_BYTES
  ) {
    throw new IpcError(
      "INVALID_CONFIG",
      "maximumFrameBytes must be within 1..=1048576",
    );
  }
  for (const [name, value] of [
    ["connectTimeoutMs", config.connectTimeoutMs],
    ["readTimeoutMs", config.readTimeoutMs],
    ["writeTimeoutMs", config.writeTimeoutMs],
  ] as const) {
    if (!Number.isFinite(value) || value <= 0) {
      throw new IpcError("INVALID_CONFIG", `${name} must be positive`);
    }
  }
  return Object.freeze({ ...config });
}

function waitForConnect(socket: Socket, timeoutMs: number): Promise<void> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      finish(
        new IpcError("TIMEOUT", `IPC connect timed out after ${timeoutMs} ms`),
      );
    }, timeoutMs);
    const onConnect = (): void => finish();
    const onError = (cause: Error): void =>
      finish(new IpcError("CONNECT", "IPC connect failed", { cause }));
    const finish = (error?: IpcError): void => {
      clearTimeout(timer);
      socket.off("connect", onConnect);
      socket.off("error", onError);
      if (error === undefined) {
        resolve();
      } else {
        socket.destroy();
        reject(error);
      }
    };
    socket.once("connect", onConnect);
    socket.once("error", onError);
  });
}

function writeFrame(
  socket: Socket,
  wire: Uint8Array,
  timeoutMs: number,
): Promise<void> {
  const prefix = Buffer.allocUnsafe(4);
  prefix.writeUInt32BE(wire.byteLength);
  const frame = Buffer.concat([prefix, Buffer.from(wire)]);
  return new Promise((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new IpcError("TIMEOUT", "IPC write timed out")),
      timeoutMs,
    );
    socket.write(frame, (cause?: Error | null) => {
      clearTimeout(timer);
      if (cause == null) {
        resolve();
      } else {
        reject(new IpcError("WRITE", "IPC write failed", { cause }));
      }
    });
  });
}

function readFrame(
  socket: Socket,
  maximumFrameBytes: number,
  timeoutMs: number,
): Promise<Uint8Array> {
  return new Promise((resolve, reject) => {
    let buffered = Buffer.alloc(0);
    let declared: number | undefined;
    const timer = setTimeout(
      () => finish(new IpcError("TIMEOUT", "IPC read timed out")),
      timeoutMs,
    );
    const onData = (chunk: Buffer): void => {
      if (buffered.byteLength + chunk.byteLength > maximumFrameBytes + 4) {
        finish(
          new IpcError(
            "FRAME_TOO_LARGE",
            "IPC peer sent more bytes than the configured unary frame bound",
          ),
        );
        return;
      }
      buffered = Buffer.concat([buffered, chunk]);
      if (declared === undefined && buffered.byteLength >= 4) {
        declared = buffered.readUInt32BE(0);
        try {
          ensureFrameBound(declared, maximumFrameBytes);
        } catch (error) {
          finish(error as IpcError);
          return;
        }
      }
      if (declared !== undefined && buffered.byteLength >= declared + 4) {
        if (buffered.byteLength !== declared + 4) {
          finish(
            new IpcError(
              "READ",
              "IPC peer sent trailing bytes after a unary response frame",
            ),
          );
          return;
        }
        finish(undefined, buffered.subarray(4));
      }
    };
    const onError = (cause: Error): void =>
      finish(new IpcError("READ", "IPC read failed", { cause }));
    const onClose = (): void =>
      finish(new IpcError("READ", "IPC peer disconnected mid-exchange"));
    const finish = (error?: IpcError, wire?: Uint8Array): void => {
      clearTimeout(timer);
      socket.off("data", onData);
      socket.off("error", onError);
      socket.off("close", onClose);
      if (error === undefined && wire !== undefined) {
        resolve(wire);
      } else {
        reject(error ?? new IpcError("READ", "IPC response is missing"));
      }
    };
    socket.on("data", onData);
    socket.once("error", onError);
    socket.once("close", onClose);
  });
}

function ensureFrameBound(actual: number, maximum: number): void {
  if (actual > maximum) {
    throw new IpcError(
      "FRAME_TOO_LARGE",
      `IPC frame has ${actual} bytes; maximum is ${maximum}`,
    );
  }
}

function requireCompatibleEnvelope(envelope: Envelope | undefined): Envelope {
  if (envelope === undefined || envelope.schema === undefined) {
    throw new IpcError("COMPATIBILITY", "schema identity is missing");
  }
  if (envelope.schema.name !== SUPPORTED_SCHEMA) {
    throw new IpcError(
      "COMPATIBILITY",
      `schema ${JSON.stringify(envelope.schema.name)} is not registered`,
    );
  }
  if (envelope.schema.major !== SUPPORTED_MAJOR) {
    throw new IpcError(
      "COMPATIBILITY",
      `unsupported schema major ${envelope.schema.major}`,
    );
  }
  if (envelope.schema.criticalExtensionIds.length !== 0) {
    throw new IpcError(
      "COMPATIBILITY",
      `unsupported critical extension ${envelope.schema.criticalExtensionIds[0]}`,
    );
  }
  if (envelope.requestId.byteLength !== 16) {
    throw new IpcError("COMPATIBILITY", "request_id must contain 16 bytes");
  }
  if (envelope.service.length === 0 || envelope.method.length === 0) {
    throw new IpcError(
      "COMPATIBILITY",
      "service and method must not be empty",
    );
  }
  return envelope;
}

function equalBytes(left: Uint8Array, right: Uint8Array): boolean {
  return (
    left.byteLength === right.byteLength &&
    left.every((value, index) => value === right[index])
  );
}
