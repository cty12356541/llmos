import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { fromBinary, toBinary } from "@bufbuild/protobuf";

import {
  CommonSemanticsError,
  validateRequestContext,
  validateResponseContext,
} from "../../../sdk/typescript/src/common.ts";
import {
  EnvelopeSchema,
  ExchangeRequestSchema,
  ExchangeResponseSchema,
  LocalRpcService,
  type Envelope,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import {
  LocalTransportKind,
  ResolveServiceRequestSchema,
} from "../../../gen/typescript/nlos/sabi/v1/service_directory_pb.ts";

const schemaName = "nlos.sabi.Envelope";
const goldenPath = fileURLToPath(
  new URL("../../../schema/golden/nlos.sabi.Envelope-v1.hex", import.meta.url),
);
const goldenHex = readFileSync(goldenPath, "utf8").trim();
assert.equal(goldenHex.length % 2, 0);
const golden = Uint8Array.from(
  Array.from({ length: goldenHex.length / 2 }, (_, index) =>
    Number.parseInt(goldenHex.slice(index * 2, index * 2 + 2), 16),
  ),
);

function validate(envelope: Envelope): void {
  const { schema } = envelope;
  assert.ok(schema, "schema identity is required");
  assert.equal(schema.name, schemaName);
  assert.equal(schema.major, 1, "unknown major must fail closed");
  assert.deepEqual(
    schema.criticalExtensionIds,
    [],
    "unknown critical extensions must fail closed",
  );
  assert.equal(envelope.requestId.length, 16);
  assert.notEqual(envelope.service, "");
  assert.notEqual(envelope.method, "");
}

const decoded = fromBinary(EnvelopeSchema, golden);
validate(decoded);
assert.equal(decoded.schema?.minor, 0);
assert.deepEqual(decoded.schema?.nonCriticalExtensionIds, [42]);
assert.equal(decoded.service, "operation");
assert.equal(decoded.method, "get");
assert.equal(new TextDecoder().decode(decoded.payload), "abc");
assert.deepEqual(toBinary(EnvelopeSchema, decoded), golden);

const compatible = fromBinary(EnvelopeSchema, golden);
compatible.schema!.minor = 99;
compatible.schema!.nonCriticalExtensionIds.push(7_001);
validate(compatible);

const wrongMajor = fromBinary(EnvelopeSchema, golden);
wrongMajor.schema!.major = 2;
assert.throws(() => validate(wrongMajor), /unknown major/);

const unknownCritical = fromBinary(EnvelopeSchema, golden);
unknownCritical.schema!.criticalExtensionIds.push(7_001);
assert.throws(() => validate(unknownCritical), /unknown critical/);

const withUnknownField = new Uint8Array([...golden, 0xa0, 0x06, 0x07]);
assert.deepEqual(
  toBinary(EnvelopeSchema, fromBinary(EnvelopeSchema, withUnknownField)),
  withUnknownField,
);

assert.equal(LocalRpcService.typeName, "nlos.sabi.v1.LocalRpcService");
assert.equal(LocalRpcService.method.exchange.methodKind, "unary");
assert.equal(
  LocalRpcService.method.exchange.input.typeName,
  ExchangeRequestSchema.typeName,
);
assert.equal(
  LocalRpcService.method.exchange.output.typeName,
  ExchangeResponseSchema.typeName,
);

const directoryGoldenPath = fileURLToPath(
  new URL(
    "../../../schema/golden/nlos.sabi.ServiceDirectory.ResolveRequest-v1.hex",
    import.meta.url,
  ),
);
const directoryGolden = Uint8Array.from(
  Buffer.from(readFileSync(directoryGoldenPath, "utf8").trim(), "hex"),
);
const resolveRequest = fromBinary(ResolveServiceRequestSchema, directoryGolden);
assert.equal(resolveRequest.schema?.name, "nlos.sabi.ServiceDirectory");
assert.equal(resolveRequest.schema?.major, 1);
assert.equal(resolveRequest.service, "operation");
assert.deepEqual(
  toBinary(ResolveServiceRequestSchema, resolveRequest),
  directoryGolden,
);
assert.equal(LocalTransportKind.UNIX_SOCKET, 1);
assert.equal(LocalTransportKind.WINDOWS_NAMED_PIPE, 2);

const commonRequestGolden = Uint8Array.from(
  Buffer.from(
    readFileSync(
      fileURLToPath(
        new URL(
          "../../../schema/golden/nlos.sabi.Envelope-common-request-v1.hex",
          import.meta.url,
        ),
      ),
      "utf8",
    ).trim(),
    "hex",
  ),
);
const commonRequest = fromBinary(EnvelopeSchema, commonRequestGolden);
const requestContext = validateRequestContext(
  commonRequest,
  { sideEffecting: true, longRunning: true },
  123_455n,
);
assert.equal(commonRequest.schema?.minor, 1);
assert.equal(requestContext.caller?.processGeneration, 7n);
assert.deepEqual(requestContext.idempotencyKey, new Uint8Array(16).fill(6));
assert.deepEqual(toBinary(EnvelopeSchema, commonRequest), commonRequestGolden);

requestContext.idempotencyKey = new Uint8Array();
assert.throws(
  () =>
    validateRequestContext(
      commonRequest,
      { sideEffecting: true, longRunning: false },
      0n,
    ),
  (error: unknown) =>
    error instanceof CommonSemanticsError &&
    error.code === "MISSING_IDEMPOTENCY_KEY",
);

const uncertainGolden = Uint8Array.from(
  Buffer.from(
    readFileSync(
      fileURLToPath(
        new URL(
          "../../../schema/golden/nlos.sabi.Envelope-common-uncertain-v1.hex",
          import.meta.url,
        ),
      ),
      "utf8",
    ).trim(),
    "hex",
  ),
);
const uncertain = fromBinary(EnvelopeSchema, uncertainGolden);
const responseContext = validateResponseContext(uncertain, {
  sideEffecting: true,
  longRunning: true,
});
assert.equal(responseContext.operation?.generation, 4n);
assert.equal(responseContext.failure?.code, 13);
assert.equal(responseContext.failure?.retry, 3);
assert.deepEqual(toBinary(EnvelopeSchema, uncertain), uncertainGolden);

responseContext.failure!.retry = 2;
assert.throws(
  () =>
    validateResponseContext(uncertain, {
      sideEffecting: true,
      longRunning: true,
    }),
  (error: unknown) =>
    error instanceof CommonSemanticsError && error.code === "UNSAFE_RETRY",
);
