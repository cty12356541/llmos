import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";

import { fromBinary, toBinary } from "@bufbuild/protobuf";

import {
  EnvelopeSchema,
  type Envelope,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";

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
