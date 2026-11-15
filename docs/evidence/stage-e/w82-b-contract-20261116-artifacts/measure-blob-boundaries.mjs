// W82-B-CONTRACT diagnostic: actual protobuf encoding, not a Blob service test.
import assert from "node:assert/strict";
import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { create, toBinary } from "@bufbuild/protobuf";
import { ReadReferenceResultSchema } from "../../../../gen/typescript/nlos/sabi/v1/blob_reference_pb.ts";
import { SchemaIdentitySchema } from "../../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";

const schema = create(SchemaIdentitySchema, {
  name: "nlos.sabi.BlobReference",
  major: 1,
});
const encodedLength = (chunk, offset, identity = schema) =>
  toBinary(ReadReferenceResultSchema, create(ReadReferenceResultSchema, {
    schema: identity,
    chunk: new Uint8Array(chunk),
    offset,
    chunkDigestSha256: new Uint8Array(32),
  })).length;

const vectors = [];
for (const offset of [0n, 1n, 127n, 128n, 16384n, 18446744073709551615n]) {
  let lo = 0;
  let hi = 65536;
  while (lo < hi) {
    const mid = Math.floor((lo + hi + 1) / 2);
    if (encodedLength(mid, offset) <= 65536) lo = mid;
    else hi = mid - 1;
  }
  assert.equal(encodedLength(lo, offset), 65536);
  assert.equal(encodedLength(lo + 1, offset), 65537);
  assert.ok(encodedLength(65408, offset) <= 65486);
  assert.ok(encodedLength(65536, offset) > 65536);
  vectors.push({
    offset: offset.toString(),
    max_chunk: lo,
    encoded_at_max: encodedLength(lo, offset),
    encoded_plus_one: encodedLength(lo + 1, offset),
    encoded_64k_chunk: encodedLength(65536, offset),
    proposed_65408_chunk_encoded: encodedLength(65408, offset),
  });
}

assert.equal(toBinary(SchemaIdentitySchema, schema).length, 27);
const extended = create(SchemaIdentitySchema, {
  name: "nlos.sabi.BlobReference",
  major: 1,
  nonCriticalExtensionIds: Array.from({ length: 32 }, (_, i) => 1000000 + i),
});
const extendedLength = encodedLength(65408, 18446744073709551615n, extended);
assert.ok(extendedLength > 65536);
const result = {
  status: "PASS",
  schema_bytes: 27,
  payload_limit: 65536,
  digest_bytes: 32,
  proposed_chunk_cap: 65408,
  canonical_max_overhead: 78,
  canonical_max_encoded: 65486,
  vectors,
  extension_counterexample: {
    noncritical_id_count: 32,
    encoded_length: extendedLength,
    required_behavior: "Do not reflect request extensions into canonical response; actual bounded encoder must reject oversize.",
  },
  scope: "Checked-in generated TypeScript and locked @bufbuild/protobuf; no authorization, Blob service, fd, or resource behavior exercised.",
};
writeFileSync(fileURLToPath(new URL("blob-encoded-boundaries.json", import.meta.url)), `${JSON.stringify(result, null, 2)}\n`);
console.log(JSON.stringify(result, null, 2));
