import assert from "node:assert/strict";
import { spawn, spawnSync, type ChildProcessWithoutNullStreams } from "node:child_process";
import { createHash, createPublicKey, randomBytes, verify as cryptoVerify } from "node:crypto";
import { chmodSync, existsSync } from "node:fs";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { createServer, type Server, type Socket } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { inspect } from "node:util";

import { create, fromBinary, toBinary } from "@bufbuild/protobuf";

import {
  MintReferenceRequestSchema,
  MintReferenceResultSchema,
  ReadReferenceRequestSchema,
  ReadReferenceResultSchema,
  ReleaseReferenceRequestSchema,
  ReleaseReferenceResultSchema,
} from "../../../gen/typescript/nlos/sabi/v1/blob_reference_pb.ts";
import {
  CreateChannelRequestWireSchema,
  CreateChannelResultWireSchema,
  EnqueueChannelRequestWireSchema,
  EnqueueChannelResultWireSchema,
  InspectChannelRequestWireSchema,
  InspectChannelResultWireSchema,
} from "../../../gen/typescript/nlos/sabi/v1/channel_control_pb.ts";
import {
  CallerIdentitySchema,
  CapabilityHandleSchema,
  EnvelopeSchema,
  ExchangeRequestSchema,
  ExchangeResponseSchema,
  OperationReferenceSchema,
  ReceiptReferenceSchema,
  RetryDirective,
  SabiErrorCode,
  SabiFailureSchema,
  SabiRequestContextSchema,
  SabiResponseContextSchema,
  SchemaIdentitySchema,
  type Envelope,
  type ExchangeRequest,
  type SabiFailure,
} from "../../../gen/typescript/nlos/sabi/v1/envelope_pb.ts";
import {
  AttenuateBudgetRequestSchema,
  AttenuateBudgetResultSchema,
  LlmChatRequestSchema,
  LlmChatResultSchema,
  LlmUsageWireSchema,
  QueryBudgetRequestSchema,
  QueryBudgetResultSchema,
  RecordExternalUsageRequestSchema,
  RecordExternalUsageResultSchema,
} from "../../../gen/typescript/nlos/sabi/v1/llm_gateway_pb.ts";
import {
  CancelOperationRequestSchema,
  OperationLifecycleState,
  OperationStatusSchema,
  QueryOperationRequestSchema,
} from "../../../gen/typescript/nlos/sabi/v1/operation_control_pb.ts";
import {
  PrincipalHandshakeAttestationSchema,
  PrincipalHandshakeChallengeSchema,
} from "../../../gen/typescript/nlos/sabi/v1/principal_handshake_pb.ts";
import {
  InvokeToolRequestSchema,
  InvokeToolResultSchema,
  ListToolsRequestSchema,
  ListToolsResultSchema,
  ToolDescriptorWireSchema,
} from "../../../gen/typescript/nlos/sabi/v1/tool_sandbox_pb.ts";
import {
  WaitStateCode,
} from "../../../gen/typescript/nlos/sabi/v1/wait_control_pb.ts";

import {
  BLOB_REFERENCE_SCHEMA,
  BLOB_REFERENCE_SERVICE,
  BlobReferenceClient,
  BlobReferenceError,
  BlobReferenceValidationError,
  CommonSemanticsError,
  DirectoryNegotiationError,
  Ed25519Signer,
  HANDSHAKE_SCHEMA_MAJOR,
  HANDSHAKE_SCHEMA_MINOR,
  HANDSHAKE_SCHEMA_NAME,
  IpcError,
  LLM_GATEWAY_SCHEMA,
  LLM_GATEWAY_SERVICE,
  LlmGatewayClient,
  LlmGatewayError,
  LocalRpcClient,
  MAX_BLOB_CHUNK_BYTES,
  MAX_BLOB_FRAME_BYTES,
  MAX_LLM_GATEWAY_PAYLOAD_BYTES,
  MAX_TOOL_SANDBOX_PAYLOAD_BYTES,
  OPERATION_CONTROL_SCHEMA,
  OPERATION_CONTROL_SERVICE,
  OperationClient,
  OperationControlError,
  SABI_V1_CHANNELS,
  SabiServiceError,
  ServiceDirectoryClient,
  SessionIdentity,
  TOOL_SANDBOX_SCHEMA,
  TOOL_SANDBOX_SERVICE,
  ToolSandboxClient,
  ToolSandboxError,
  WAIT_CONTROL_SCHEMA,
  WAIT_CONTROL_SERVICE,
  WaitClient,
  WaitControlError,
  connectAuthenticated,
  connectAuthenticatedSession,
  endpointChannelBinding,
  principalHandshakeMessage,
  writeFrame,
  type TransportConfig,
} from "../../../sdk/typescript/src/index.ts";

const ROOT = fileURLToPath(new URL("../../..", import.meta.url));
const CONTROL_CAPABILITY = { slot: 9n, generation: 1n };
const ED25519_SPKI_PREFIX = Buffer.from("302a300506032b6570032100", "hex");

let endpointCounter = 0;
let passedCases = 0;

function pass(label: string): void {
  passedCases += 1;
  console.log(`[sabi_v1_1_typescript] PASS ${passedCases}: ${label}`);
}

function makeEndpoint(label: string): string {
  endpointCounter += 1;
  const unique = `${process.pid}-${Date.now()}-${endpointCounter}-${label}`;
  return process.platform === "win32"
    ? `\\\\.\\pipe\\nlos-ts-sabi-${unique}`
    : join(tmpdir(), `nlos-ts-sabi-${unique}.sock`);
}

function filled(byteValue: number, len = 16): Uint8Array {
  return new Uint8Array(len).fill(byteValue & 0xff);
}

function sha256(bytes: Uint8Array): Uint8Array {
  return Uint8Array.from(createHash("sha256").update(bytes).digest());
}

function ensureDaemonBinary(): string {
  const binaryName =
    process.platform === "win32"
      ? "system-control-daemon.exe"
      : "system-control-daemon";
  const binaryPath = join(ROOT, "target", "debug", binaryName);
  if (!existsSync(binaryPath)) {
    console.log(
      "[sabi_v1_1_typescript] building system-control-daemon binary…",
    );
    const result = spawnSync(
      "cargo",
      [
        "build",
        "-p",
        "nlos-system-control",
        "--features",
        "daemon",
        "--bin",
        "system-control-daemon",
      ],
      { cwd: ROOT, stdio: "inherit" },
    );
    assert.equal(
      result.status,
      0,
      "cargo build for system-control-daemon must exit 0",
    );
  }
  assert.ok(existsSync(binaryPath), `missing daemon binary at ${binaryPath}`);
  return binaryPath;
}

async function startDaemon(
  binaryPath: string,
  rootDir: string,
  authEndpoint: string,
  plainEndpoint: string,
  seedFile: string,
): Promise<{
  process: ChildProcessWithoutNullStreams;
  principalId: Uint8Array;
}> {
  const proc = spawn(
    binaryPath,
    [
      "--root",
      rootDir,
      "--auth-socket",
      authEndpoint,
      "--plain-socket",
      plainEndpoint,
      "--identity-key-file",
      seedFile,
    ],
    { cwd: ROOT, stdio: ["pipe", "pipe", "pipe"] },
  );

  const principalHex = await new Promise<string>((resolve, reject) => {
    const timer = setTimeout(
      () => reject(new Error("system-control-daemon did not become READY")),
      30_000,
    );
    let stdout = "";
    let stderr = "";
    proc.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
      const newline = stdout.indexOf("\n");
      if (newline >= 0) {
        const line = stdout.slice(0, newline).trim();
        if (!line.startsWith("READY ") || !line.includes("principal=")) {
          clearTimeout(timer);
          reject(new Error(`unexpected daemon READY line: ${line}`));
          return;
        }
        const fields = Object.fromEntries(
          line
            .split(/\s+/)
            .filter((part) => part.includes("="))
            .map((part) => {
              const idx = part.indexOf("=");
              return [part.slice(0, idx), part.slice(idx + 1)];
            }),
        );
        if (!fields["principal"] || fields["principal"] === "none") {
          clearTimeout(timer);
          reject(new Error(`daemon READY missing principal: ${line}`));
          return;
        }
        clearTimeout(timer);
        resolve(fields["principal"]);
      }
    });
    proc.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    proc.once("exit", (code) => {
      clearTimeout(timer);
      reject(
        new Error(
          `system-control-daemon exited early (${code}): ${stderr}`,
        ),
      );
    });
    proc.once("error", (err) => {
      clearTimeout(timer);
      reject(err);
    });
  });

  const principalId = Uint8Array.from(Buffer.from(principalHex, "hex"));
  assert.equal(principalId.byteLength, 16);
  return { process: proc, principalId };
}

async function stopDaemon(
  proc: ChildProcessWithoutNullStreams | undefined,
): Promise<void> {
  if (!proc || proc.exitCode !== null || proc.signalCode !== null) {
    return;
  }
  proc.kill();
  await new Promise<void>((resolve) => {
    proc.once("exit", () => resolve());
    setTimeout(() => {
      try {
        proc.kill("SIGKILL");
      } catch {
        // already exited
      }
      resolve();
    }, 3_000);
  });
}

function assertNoSensitiveLeakage(
  error: unknown,
  secrets: readonly (Uint8Array | string)[],
  label: string,
): void {
  const texts: string[] = [];
  if (error instanceof Error) {
    texts.push(error.message, String(error), inspect(error));
    if (error.cause !== undefined) {
      texts.push(String(error.cause), inspect(error.cause));
    }
  } else {
    texts.push(String(error), inspect(error));
  }
  const combined = texts.join("\n").toLowerCase();
  for (const secret of secrets) {
    if (typeof secret === "string") {
      assert.ok(
        !combined.includes(secret.toLowerCase()),
        `${label}: leaked string secret in error`,
      );
    } else {
      const hex = Buffer.from(secret).toString("hex").toLowerCase();
      assert.ok(
        !combined.includes(hex),
        `${label}: leaked hex bytes in error`,
      );
      const utf8 = Buffer.from(secret).toString("utf8").toLowerCase();
      if (/^[a-z0-9_-]{8,}$/i.test(utf8)) {
        assert.ok(
          !combined.includes(utf8),
          `${label}: leaked utf8 bytes in error`,
        );
      }
    }
  }
}

async function createChannelViaRpc(
  rpc: LocalRpcClient,
  identity: SessionIdentity,
  idempotencyKey: Uint8Array,
) {
  const payload = toBinary(
    CreateChannelRequestWireSchema,
    create(CreateChannelRequestWireSchema, {
      schema: create(SchemaIdentitySchema, {
        name: "nlos.sabi.ChannelControl",
        major: 1,
        minor: 0,
      }),
      capacityBytes: 1024n,
      policyDigest: filled(0x33, 32),
      idempotencyKey,
      createdAtMs: 1_500n,
    }),
  );
  const response = await rpc.exchange(
    create(ExchangeRequestSchema, {
      envelope: create(EnvelopeSchema, {
        schema: create(SchemaIdentitySchema, {
          name: "nlos.sabi.Envelope",
          major: 1,
          minor: 1,
        }),
        requestId: Uint8Array.from(randomBytes(16)),
        service: "channel",
        method: "create_channel",
        commonContext: {
          case: "requestContext",
          value: create(SabiRequestContextSchema, {
            caller: identity.toCallerIdentity(),
            correlationId: idempotencyKey,
            idempotencyKey,
            capabilityHandles: [create(CapabilityHandleSchema, CONTROL_CAPABILITY)],
          }),
        },
        payload,
      }),
    }),
  );
  assert.ok(response.envelope);
  assert.equal(response.envelope.commonContext.case, "responseContext");
  assert.equal(response.envelope.commonContext.value.failure, undefined);
  return fromBinary(CreateChannelResultWireSchema, response.envelope.payload);
}

async function inspectChannelViaRpc(
  rpc: LocalRpcClient,
  identity: SessionIdentity,
  channelId: Uint8Array,
) {
  const payload = toBinary(
    InspectChannelRequestWireSchema,
    create(InspectChannelRequestWireSchema, {
      schema: create(SchemaIdentitySchema, {
        name: "nlos.sabi.ChannelControl",
        major: 1,
        minor: 0,
      }),
      channelId,
    }),
  );
  const response = await rpc.exchange(
    create(ExchangeRequestSchema, {
      envelope: create(EnvelopeSchema, {
        schema: create(SchemaIdentitySchema, {
          name: "nlos.sabi.Envelope",
          major: 1,
          minor: 1,
        }),
        requestId: Uint8Array.from(randomBytes(16)),
        service: "channel",
        method: "inspect_channel",
        commonContext: {
          case: "requestContext",
          value: create(SabiRequestContextSchema, {
            caller: identity.toCallerIdentity(),
            correlationId: Uint8Array.from(randomBytes(16)),
            capabilityHandles: [create(CapabilityHandleSchema, CONTROL_CAPABILITY)],
          }),
        },
        payload,
      }),
    }),
  );
  assert.ok(response.envelope);
  return fromBinary(InspectChannelResultWireSchema, response.envelope.payload);
}

async function enqueueChannelViaRpc(
  rpc: LocalRpcClient,
  identity: SessionIdentity,
  channelId: Uint8Array,
  generation: bigint,
  fencingToken: Uint8Array,
  queuePayload: Uint8Array,
  idempotencyKey: Uint8Array,
) {
  const payload = toBinary(
    EnqueueChannelRequestWireSchema,
    create(EnqueueChannelRequestWireSchema, {
      schema: create(SchemaIdentitySchema, {
        name: "nlos.sabi.ChannelControl",
        major: 1,
        minor: 0,
      }),
      channelId,
      expectedGeneration: generation,
      expectedFencingToken: fencingToken,
      payload: queuePayload,
      idempotencyKey,
      enqueuedAtMs: 1_700n,
    }),
  );
  const response = await rpc.exchange(
    create(ExchangeRequestSchema, {
      envelope: create(EnvelopeSchema, {
        schema: create(SchemaIdentitySchema, {
          name: "nlos.sabi.Envelope",
          major: 1,
          minor: 1,
        }),
        requestId: Uint8Array.from(randomBytes(16)),
        service: "channel",
        method: "enqueue",
        commonContext: {
          case: "requestContext",
          value: create(SabiRequestContextSchema, {
            caller: identity.toCallerIdentity(),
            correlationId: idempotencyKey,
            idempotencyKey,
            capabilityHandles: [create(CapabilityHandleSchema, CONTROL_CAPABILITY)],
          }),
        },
        payload,
      }),
    }),
  );
  assert.ok(response.envelope);
  assert.equal(response.envelope.commonContext.case, "responseContext");
  assert.equal(response.envelope.commonContext.value.failure, undefined);
  return fromBinary(EnqueueChannelResultWireSchema, response.envelope.payload);
}

// ===========================================================================
// Suite 0: Registry / Export / Signer Zero-Leakage Static Checks
// ===========================================================================
async function runStaticChecks(): Promise<void> {
  assert.equal(Object.keys(SABI_V1_CHANNELS).length, 12);
  for (const name of [
    "nlos.sabi.Envelope",
    "nlos.sabi.ServiceDirectory",
    "nlos.sabi.OperationControl",
    "nlos.sabi.SystemControl",
    "nlos.sabi.TakeoverControl",
    "nlos.sabi.WaitControl",
    "nlos.sabi.PrincipalHandshake",
    "nlos.sabi.DriverProvider",
    "nlos.sabi.ChannelControl",
    "nlos.sabi.LlmGateway",
    "nlos.sabi.ToolSandbox",
    "nlos.sabi.BlobReference",
  ]) {
    assert.ok(name in SABI_V1_CHANNELS, `missing channel ${name}`);
  }
  assert.equal(SABI_V1_CHANNELS["nlos.sabi.LlmGateway"]?.major, 1);
  assert.equal(SABI_V1_CHANNELS["nlos.sabi.LlmGateway"]?.minor, 0);
  assert.equal(SABI_V1_CHANNELS["nlos.sabi.ToolSandbox"]?.major, 1);
  assert.equal(SABI_V1_CHANNELS["nlos.sabi.ToolSandbox"]?.minor, 0);
  assert.equal(SABI_V1_CHANNELS["nlos.sabi.BlobReference"]?.major, 1);
  assert.equal(SABI_V1_CHANNELS["nlos.sabi.BlobReference"]?.minor, 0);

  const seedSentinel = filled(0xa9, 32);
  const signer = Ed25519Signer.fromSeed(seedSentinel);
  const seedHex = Buffer.from(seedSentinel).toString("hex");
  assert.ok(!inspect(signer).toLowerCase().includes(seedHex));
  assert.ok(!JSON.stringify(signer).toLowerCase().includes(seedHex));
  assert.throws(
    () => new Ed25519Signer(seedSentinel.subarray(0, 16)),
    (err: unknown) => {
      assertNoSensitiveLeakage(err, [seedSentinel], "short seed");
      return err instanceof IpcError && err.code === "INVALID_CONFIG";
    },
  );
  pass("12-channel registry and Ed25519Signer zero-seed-leakage invariants");
}

// ===========================================================================
// Suite A: Cross-Process Real Rust system-control-daemon
// ===========================================================================
async function runRustDaemonSuite(): Promise<void> {
  const binaryPath = ensureDaemonBinary();
  const rootDir = join(
    tmpdir(),
    `nlos-ts-daemon-${process.pid}-${Date.now()}`,
  );
  await mkdir(rootDir, { recursive: true });
  const seed = Uint8Array.from(randomBytes(32));
  const signer = new Ed25519Signer(seed);
  const seedFile = join(rootDir, "client.seed");
  await writeFile(seedFile, Buffer.from(seed).toString("hex") + "\n", "utf8");
  if (process.platform !== "win32") {
    chmodSync(seedFile, 0o600);
  }

  const authEndpoint = makeEndpoint("daemon-auth");
  const plainEndpoint = makeEndpoint("daemon-plain");
  let daemon:
    | { process: ChildProcessWithoutNullStreams; principalId: Uint8Array }
    | undefined;

  const transportConfig: Partial<TransportConfig> = {
    connectTimeoutMs: 5_000,
    readTimeoutMs: 5_000,
    writeTimeoutMs: 5_000,
  };

  try {
    daemon = await startDaemon(
      binaryPath,
      rootDir,
      authEndpoint,
      plainEndpoint,
      seedFile,
    );
    const { principalId } = daemon;

    // 1. Single persistent authenticated connection across multiple services & rounds
    const { client: persistentRpc, identity } =
      await connectAuthenticatedSession(authEndpoint, signer, principalId, {
        transport: transportConfig,
      });

    try {
      const directory = ServiceDirectoryClient.fromClient(
        persistentRpc,
        identity,
      );

      for (const req of [
        WaitClient.serviceRequirement(),
        OperationClient.serviceRequirement(),
        {
          service: "channel",
          schemaName: "nlos.sabi.ChannelControl",
          major: 1,
          minimumMinor: 0,
        },
        {
          service: "service_directory",
          schemaName: "nlos.sabi.ServiceDirectory",
          major: 1,
          minimumMinor: 0,
        },
      ]) {
        const binding = await directory.negotiate(req);
        assert.equal(binding.candidate?.service, req.service);
        assert.ok(
          binding.endpoint?.address === authEndpoint ||
            binding.endpoint?.address === plainEndpoint,
          `unexpected negotiated endpoint: ${binding.endpoint?.address}`,
        );
      }

      // Unmounted v1.1 service returns typed DirectoryNegotiationError(NOT_FOUND)
      // while keeping the persistent authenticated connection alive.
      await assert.rejects(
        directory.negotiate(LlmGatewayClient.serviceRequirement()),
        (err: unknown) =>
          err instanceof DirectoryNegotiationError && err.code === 2, // NOT_FOUND
      );
      await assert.rejects(
        directory.negotiate({
          service: WAIT_CONTROL_SERVICE,
          schemaName: WAIT_CONTROL_SCHEMA,
          major: 99,
        }),
        (err: unknown) =>
          err instanceof DirectoryNegotiationError && err.code === 4, // VERSION_UNSUPPORTED
      );
      assert.equal(persistentRpc.isUsable, true);
      pass("Rust daemon persistent ServiceDirectory.negotiate (+ unmounted/version errors)");

      // 2. ChannelControl create + replay + inspect on the SAME persistent connection
      const channelKey = Uint8Array.from(randomBytes(16));
      const createdChannel = await createChannelViaRpc(
        persistentRpc,
        identity,
        channelKey,
      );
      assert.equal(createdChannel.replayed, false);
      assert.ok(createdChannel.record);
      const channelId = Uint8Array.from(createdChannel.record.channelId);
      assert.equal(channelId.byteLength, 16);

      const replayedChannel = await createChannelViaRpc(
        persistentRpc,
        identity,
        channelKey,
      );
      assert.equal(replayedChannel.replayed, true);
      assert.deepEqual(
        Uint8Array.from(replayedChannel.record!.channelId),
        channelId,
      );
      const inspectedChannel = await inspectChannelViaRpc(
        persistentRpc,
        identity,
        channelId,
      );
      assert.deepEqual(
        Uint8Array.from(inspectedChannel.record!.channelId),
        channelId,
      );
      pass("Rust daemon persistent ChannelControl create/replay/inspect");

      // 3. WaitClient over the SAME persistent authenticated connection
      const waitClient = new WaitClient(persistentRpc, {
        identity,
        capabilityHandles: [CONTROL_CAPABILITY],
      });
      const waitKey1 = filled(0x41, 16);
      const binding1 = filled(0x42, 16);
      const reg1 = await waitClient.registerWait({
        binding: binding1,
        channelId,
        targetSequence: 1n,
        registeredAtMs: 1_000n,
        idempotencyKey: waitKey1,
      });
      assert.equal(reg1.replayed, false);
      assert.equal(reg1.record?.state, WaitStateCode.PENDING);
      const waitId1 = Uint8Array.from(reg1.record!.waitId);

      // Idempotent replay
      const reg1Replay = await waitClient.registerWait({
        binding: binding1,
        channelId,
        targetSequence: 1n,
        registeredAtMs: 1_000n,
        idempotencyKey: waitKey1,
      });
      assert.equal(reg1Replay.replayed, true);
      assert.deepEqual(Uint8Array.from(reg1Replay.record!.waitId), waitId1);

      const inspected1 = await waitClient.inspectWait(waitId1);
      assert.equal(inspected1.record?.state, WaitStateCode.PENDING);

      // Enqueue seq 1 on channel and wake wait1 via notifyCommits
      await enqueueChannelViaRpc(
        persistentRpc,
        identity,
        channelId,
        createdChannel.record.generation,
        createdChannel.record.fencingToken,
        Buffer.from("wake-payload"),
        Uint8Array.from(randomBytes(16)),
      );
      const wakeReport = await waitClient.notifyCommits({
        channelId,
        upToSequence: 1n,
        notifiedAtMs: 2_000n,
      });
      assert.equal(wakeReport.woken.length, 1);
      assert.deepEqual(Uint8Array.from(wakeReport.woken[0]!.waitId), waitId1);
      assert.equal(wakeReport.woken[0]!.state, WaitStateCode.WOKEN);

      // Register second wait and cancel it + replay cancel
      const reg2 = await waitClient.registerWait({
        binding: filled(0x43, 16),
        channelId,
        targetSequence: 5n,
        registeredAtMs: 2_100n,
      });
      const waitId2 = Uint8Array.from(reg2.record!.waitId);
      const cancelKey2 = filled(0x44, 16);
      const cancelled2 = await waitClient.cancelWait({
        waitId: waitId2,
        cancelledAtMs: 2_200n,
        idempotencyKey: cancelKey2,
      });
      assert.equal(cancelled2.replayed, false);
      assert.equal(cancelled2.record?.state, WaitStateCode.CANCELLED);

      const cancel2Replay = await waitClient.cancelWait({
        waitId: waitId2,
        cancelledAtMs: 2_200n,
        idempotencyKey: cancelKey2,
      });
      assert.equal(cancel2Replay.replayed, true);
      assert.equal(cancel2Replay.record?.state, WaitStateCode.CANCELLED);

      const listed = await waitClient.listWaits({ filterChannelId: channelId });
      assert.equal(listed.waits.length, 2);

      // Negative WaitControl inspect unknown waitId -> typed WaitControlError(NOT_FOUND), connection alive
      await assert.rejects(
        waitClient.inspectWait(filled(0xee, 16)),
        (err: unknown) =>
          err instanceof WaitControlError &&
          err.code === SabiErrorCode.NOT_FOUND &&
          err.retry === RetryDirective.DO_NOT_RETRY,
      );
      assert.equal(persistentRpc.isUsable, true);
      pass(
        "Rust daemon persistent WaitClient (register/replay/inspect/notify/cancel/list + NOT_FOUND)",
      );

      // 4. OperationClient over the SAME persistent authenticated connection
      const opClient = new OperationClient(persistentRpc, {
        identity,
        capabilityHandles: [CONTROL_CAPABILITY],
      });
      const unknownOp = { operationId: filled(0xde, 16), generation: 1n };
      await assert.rejects(
        opClient.query(unknownOp),
        (err: unknown) =>
          err instanceof OperationControlError &&
          err.code === SabiErrorCode.NOT_FOUND &&
          err.retry === RetryDirective.DO_NOT_RETRY,
      );
      await assert.rejects(
        opClient.cancel(unknownOp, 0n),
        (err: unknown) =>
          err instanceof OperationControlError &&
          err.code === SabiErrorCode.NOT_FOUND &&
          err.retry === RetryDirective.DO_NOT_RETRY,
      );
      assert.equal(persistentRpc.isUsable, true);
      pass(
        "Rust daemon persistent OperationClient dispatch (query/cancel -> typed NOT_FOUND)",
      );

      // 5. Unmounted service (llm_gateway) on persistent authenticated daemon connection
      const unmountedLlm = new LlmGatewayClient(persistentRpc, {
        identity,
        capabilityHandles: [CONTROL_CAPABILITY],
      });
      await assert.rejects(
        unmountedLlm.queryBudget(),
        (err: unknown) =>
          err instanceof LlmGatewayError &&
          err.code === SabiErrorCode.NOT_SUPPORTED &&
          err.retry === RetryDirective.DO_NOT_RETRY,
      );
      // Unknown major & unknown critical extension fail-closed locally WITHOUT poisoning persistentRpc
      const badMajor = create(ExchangeRequestSchema, {
        envelope: create(EnvelopeSchema, {
          schema: create(SchemaIdentitySchema, {
            name: "nlos.sabi.Envelope",
            major: 2,
            minor: 0,
          }),
          requestId: Uint8Array.from(randomBytes(16)),
          service: "wait_control",
          method: "list_waits",
        }),
      });
      await assert.rejects(
        persistentRpc.exchange(badMajor),
        (err: unknown) =>
          err instanceof IpcError && err.code === "COMPATIBILITY",
      );
      const badCritical = create(ExchangeRequestSchema, {
        envelope: create(EnvelopeSchema, {
          schema: create(SchemaIdentitySchema, {
            name: "nlos.sabi.Envelope",
            major: 1,
            minor: 1,
            criticalExtensionIds: [7001],
          }),
          requestId: Uint8Array.from(randomBytes(16)),
          service: "wait_control",
          method: "list_waits",
        }),
      });
      await assert.rejects(
        persistentRpc.exchange(badCritical),
        (err: unknown) =>
          err instanceof IpcError && err.code === "COMPATIBILITY",
      );
      // Connection still healthy after all local/service fail-closed rejections!
      const stillAlive = await waitClient.listWaits({
        filterChannelId: channelId,
      });
      assert.equal(stillAlive.waits.length, 2);
      pass(
        "Rust daemon unmounted service & unknown major/critical extension fail-closed (connection survives)",
      );

      // 6. Plain endpoint verification (WaitClient.fromEndpoint)
      const plainWait = WaitClient.fromEndpoint(plainEndpoint, {
        transport: transportConfig,
        capabilityHandles: [CONTROL_CAPABILITY],
      });
      const plainListed = await plainWait.listWaits({
        filterChannelId: channelId,
      });
      assert.equal(plainListed.waits.length, 2);
      pass("Rust daemon plain endpoint WaitClient control");
    } finally {
      persistentRpc.close();
    }

    // 7. Wrong-key authentication fail-closed against real Rust verify_strict
    const strangerSeed = Uint8Array.from(randomBytes(32));
    const strangerSigner = new Ed25519Signer(strangerSeed);
    const strangerRpc = await connectAuthenticated(
      authEndpoint,
      strangerSigner,
      principalId,
      transportConfig,
    );
    try {
      const strangerWait = new WaitClient(strangerRpc, {
        identity: SessionIdentity.derive(principalId),
      });
      await assert.rejects(
        strangerWait.listWaits(),
        (err: unknown) => {
          assertNoSensitiveLeakage(
            err,
            [strangerSeed, seed],
            "stranger handshake",
          );
          return (
            err instanceof IpcError &&
            (err.code === "READ" || err.code === "WRITE")
          );
        },
      );
      assert.equal(strangerRpc.isUsable, false);
      await assert.rejects(
        strangerWait.listWaits(),
        (err: unknown) =>
          err instanceof IpcError && err.code === "CONNECTION_UNUSABLE",
      );
      pass("Rust daemon wrong-key authentication fail-closed & connection poisoned");
    } finally {
      strangerRpc.close();
    }
  } finally {
    await stopDaemon(daemon?.process);
    if (process.platform !== "win32") {
      await Promise.all([
        rm(authEndpoint, { force: true }),
        rm(plainEndpoint, { force: true }),
      ]);
    }
    await rm(rootDir, { recursive: true, force: true });
  }
}

// ===========================================================================
// Suite B: Local Real-Socket Frame Protocol, Services & Security Boundaries
// ===========================================================================
type SocketConnectionBehavior =
  | { kind: "authenticated_sabi" }
  | { kind: "timeout" }
  | { kind: "half_header" }
  | { kind: "half_body" }
  | { kind: "eof_immediate" }
  | { kind: "trailing_bytes" }
  | { kind: "malformed_protobuf" }
  | { kind: "wrong_request_id" }
  | { kind: "wrong_correlation_id" }
  | { kind: "bad_response_schema_major" }
  | { kind: "bad_response_critical_ext" }
  | { kind: "bad_challenge_schema" }
  | { kind: "bad_challenge_nonce" }
  | { kind: "handshake_eof" };

interface MockServiceState {
  readonly endpoint: string;
  readonly expectedPrincipalId: Uint8Array;
  readonly expectedPublicKey: Uint8Array;
  readonly secretBlobBytes: Uint8Array;
  readonly secretReceiptBytes: Uint8Array;
  readonly operations: Map<
    string,
    { state: OperationLifecycleState; cancelEpoch: bigint; receiptId: Uint8Array }
  >;
  readonly llmUncertainSeen: Set<string>;
  readonly toolUncertainSeen: Set<string>;
  readonly blobs: Map<
    string,
    {
      bytes: Uint8Array;
      chunkSize: bigint;
      recipientPrincipalId: Uint8Array;
      released: boolean;
    }
  >;
  nextBehavior: SocketConnectionBehavior;
  nextReadCorruption?: "corrupt_digest" | "wrong_offset" | "oversized_chunk" | "oversized_frame";
  nextLlmOversizedResponse?: boolean;
  nextToolOversizedResponse?: boolean;
  nextMutationMissingEvidence?: boolean;
  nextUnsafeUncertainRetry?: boolean;
  nextUncertainMissingOp?: boolean;
}

function buildSabiResponseEnvelope(
  request: Envelope,
  correlationId: Uint8Array,
  options: {
    payload?: Uint8Array;
    operation?: { operationId: Uint8Array; generation: bigint };
    receipts?: Uint8Array[];
    failure?: SabiFailure;
    omitResponseContext?: boolean;
    overrideRequestId?: Uint8Array;
    overrideMajor?: number;
    criticalExtensionIds?: number[];
  },
): Envelope {
  return create(EnvelopeSchema, {
    schema: create(SchemaIdentitySchema, {
      name: "nlos.sabi.Envelope",
      major: options.overrideMajor ?? 1,
      minor: 1,
      criticalExtensionIds: options.criticalExtensionIds ?? [],
    }),
    requestId: options.overrideRequestId ?? Uint8Array.from(request.requestId),
    service: request.service,
    method: request.method,
    commonContext: options.omitResponseContext
      ? { case: undefined }
      : {
          case: "responseContext",
          value: create(SabiResponseContextSchema, {
            correlationId,
            operation: options.operation
              ? create(OperationReferenceSchema, options.operation)
              : undefined,
            receipts: (options.receipts ?? []).map((receiptId) =>
              create(ReceiptReferenceSchema, { receiptId }),
            ),
            failure: options.failure,
          }),
        },
    payload: options.payload ?? new Uint8Array(),
  });
}

class SocketFrameReader {
  #buffer = Buffer.alloc(0);
  #ended = false;
  #error: Error | undefined;
  #waiter: (() => void) | undefined;

  constructor(socket: Socket) {
    socket.on("data", (chunk: Buffer) => {
      this.#buffer = Buffer.concat([this.#buffer, chunk]);
      this.#wake();
    });
    socket.once("error", (err: Error) => {
      this.#error = err;
      this.#wake();
    });
    socket.once("end", () => {
      this.#ended = true;
      this.#wake();
    });
    socket.once("close", () => {
      this.#ended = true;
      this.#wake();
    });
  }

  #wake(): void {
    const notify = this.#waiter;
    this.#waiter = undefined;
    notify?.();
  }

  async readFrame(maxBytes: number, timeoutMs: number): Promise<Uint8Array> {
    const deadline = Date.now() + timeoutMs;
    while (true) {
      if (this.#buffer.byteLength >= 4) {
        const declared = this.#buffer.readUInt32BE(0);
        if (declared > maxBytes) {
          throw new Error(`mock server frame exceeds maxBytes (${declared})`);
        }
        if (this.#buffer.byteLength >= 4 + declared) {
          const body = Uint8Array.from(this.#buffer.subarray(4, 4 + declared));
          this.#buffer = this.#buffer.subarray(4 + declared);
          return body;
        }
      }
      if (this.#error) {
        throw this.#error;
      }
      if (this.#ended) {
        throw new Error("mock server socket ended mid-frame");
      }
      const remaining = deadline - Date.now();
      if (remaining <= 0) {
        throw new Error("mock server read timed out");
      }
      await new Promise<void>((resolve) => {
        const timer = setTimeout(() => {
          this.#waiter = undefined;
          resolve();
        }, remaining);
        this.#waiter = () => {
          clearTimeout(timer);
          resolve();
        };
      });
    }
  }
}

async function startLocalMockServer(
  state: MockServiceState,
): Promise<{ server: Server; close: () => Promise<void> }> {
  const activeSockets = new Set<Socket>();
  const server = createServer((socket) => {
    activeSockets.add(socket);
    socket.once("close", () => activeSockets.delete(socket));
    void handleMockConnection(socket, state).catch(() => {
      if (!socket.destroyed) {
        socket.destroy();
      }
    });
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(state.endpoint, () => {
      server.off("error", reject);
      resolve();
    });
  });
  return {
    server,
    close: async () => {
      for (const socket of activeSockets) {
        if (!socket.destroyed) {
          socket.destroy();
        }
      }
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
}

async function handleMockConnection(
  socket: Socket,
  state: MockServiceState,
): Promise<void> {
  const reader = new SocketFrameReader(socket);
  const behavior = state.nextBehavior;
  state.nextBehavior = { kind: "authenticated_sabi" };

  if (behavior.kind === "handshake_eof") {
    socket.destroy();
    return;
  }
  if (behavior.kind === "bad_challenge_schema") {
    const badChallenge = toBinary(
      PrincipalHandshakeChallengeSchema,
      create(PrincipalHandshakeChallengeSchema, {
        schema: create(SchemaIdentitySchema, {
          name: HANDSHAKE_SCHEMA_NAME,
          major: 2,
          minor: 0,
        }),
        nonce: Uint8Array.from(randomBytes(32)),
      }),
    );
    await writeFrame(socket, badChallenge, 2_000);
    socket.end();
    return;
  }
  if (behavior.kind === "bad_challenge_nonce") {
    const badChallenge = toBinary(
      PrincipalHandshakeChallengeSchema,
      create(PrincipalHandshakeChallengeSchema, {
        schema: create(SchemaIdentitySchema, {
          name: HANDSHAKE_SCHEMA_NAME,
          major: HANDSHAKE_SCHEMA_MAJOR,
          minor: HANDSHAKE_SCHEMA_MINOR,
        }),
        nonce: Uint8Array.from(randomBytes(16)), // short nonce!
      }),
    );
    await writeFrame(socket, badChallenge, 2_000);
    socket.end();
    return;
  }

  // Normal ADR-0011 server-side challenge-response handshake
  const nonce = Uint8Array.from(randomBytes(32));
  const challengeWire = toBinary(
    PrincipalHandshakeChallengeSchema,
    create(PrincipalHandshakeChallengeSchema, {
      schema: create(SchemaIdentitySchema, {
        name: HANDSHAKE_SCHEMA_NAME,
        major: HANDSHAKE_SCHEMA_MAJOR,
        minor: HANDSHAKE_SCHEMA_MINOR,
      }),
      nonce,
    }),
  );
  await writeFrame(socket, challengeWire, 2_000);

  const attestationWire = await reader.readFrame(1024 * 1024, 2_000);
  const attestation = fromBinary(
    PrincipalHandshakeAttestationSchema,
    attestationWire,
  );
  const expectedBinding = endpointChannelBinding(state.endpoint);
  if (
    Buffer.compare(attestation.nonce, nonce) !== 0 ||
    Buffer.compare(attestation.channelBinding, expectedBinding) !== 0
  ) {
    socket.destroy();
    return;
  }
  const digest = principalHandshakeMessage(
    nonce,
    attestation.principalId,
    expectedBinding,
  );
  const spki = Buffer.concat([ED25519_SPKI_PREFIX, state.expectedPublicKey]);
  const pubKey = createPublicKey({ key: spki, format: "der", type: "spki" });
  const verified = cryptoVerify(
    null,
    Buffer.from(digest),
    pubKey,
    Buffer.from(attestation.signature),
  );
  if (
    !verified ||
    Buffer.compare(attestation.principalId, state.expectedPrincipalId) !== 0
  ) {
    socket.destroy();
    return;
  }
  const authenticatedPrincipal = Uint8Array.from(attestation.principalId);

  // Serve multi-round exchanges on this authenticated connection
  while (!socket.destroyed) {
    let requestWire: Uint8Array;
    try {
      requestWire = await reader.readFrame(1024 * 1024, 60_000);
    } catch {
      return;
    }

    if (behavior.kind === "timeout") {
      await new Promise((resolve) => setTimeout(resolve, 600));
      socket.destroy();
      return;
    }
    if (behavior.kind === "eof_immediate") {
      socket.destroy();
      return;
    }
    if (behavior.kind === "half_header") {
      socket.write(Buffer.from([0x00, 0x01]), () => socket.destroy());
      return;
    }
    if (behavior.kind === "half_body") {
      const prefix = Buffer.allocUnsafe(4);
      prefix.writeUInt32BE(100);
      socket.write(
        Buffer.concat([prefix, Buffer.alloc(12, 0xaa)]),
        () => socket.destroy(),
      );
      return;
    }
    if (behavior.kind === "trailing_bytes") {
      const req = fromBinary(ExchangeRequestSchema, requestWire);
      const env = req.envelope!;
      const corr =
        env.commonContext.case === "requestContext"
          ? env.commonContext.value.correlationId
          : env.requestId;
      const respWire = toBinary(
        ExchangeResponseSchema,
        create(ExchangeResponseSchema, {
          envelope: buildSabiResponseEnvelope(env, corr, {
            payload: new Uint8Array([1]),
          }),
        }),
      );
      const prefix = Buffer.allocUnsafe(4);
      prefix.writeUInt32BE(respWire.byteLength);
      socket.write(
        Buffer.concat([prefix, respWire, Buffer.from([0xde, 0xad])]),
      );
      return;
    }
    if (behavior.kind === "malformed_protobuf") {
      await writeFrame(socket, Uint8Array.from([0xff, 0xff, 0xff]), 2_000);
      return;
    }

    const exchangeReq: ExchangeRequest = fromBinary(
      ExchangeRequestSchema,
      requestWire,
    );
    const reqEnv = exchangeReq.envelope!;
    assert.equal(reqEnv.commonContext.case, "requestContext");
    const reqCtx = reqEnv.commonContext.value;
    const corr = Uint8Array.from(reqCtx.correlationId);

    if (behavior.kind === "wrong_request_id") {
      const respEnv = buildSabiResponseEnvelope(reqEnv, corr, {
        overrideRequestId: filled(0x00, 16),
      });
      await writeFrame(
        socket,
        toBinary(
          ExchangeResponseSchema,
          create(ExchangeResponseSchema, { envelope: respEnv }),
        ),
        2_000,
      );
      return;
    }
    if (behavior.kind === "wrong_correlation_id") {
      const respEnv = buildSabiResponseEnvelope(reqEnv, filled(0x00, 16), {
        receipts: [state.secretReceiptBytes],
      });
      await writeFrame(
        socket,
        toBinary(
          ExchangeResponseSchema,
          create(ExchangeResponseSchema, { envelope: respEnv }),
        ),
        2_000,
      );
      return;
    }
    if (behavior.kind === "bad_response_schema_major") {
      const respEnv = buildSabiResponseEnvelope(reqEnv, corr, {
        overrideMajor: 2,
      });
      await writeFrame(
        socket,
        toBinary(
          ExchangeResponseSchema,
          create(ExchangeResponseSchema, { envelope: respEnv }),
        ),
        2_000,
      );
      return;
    }
    if (behavior.kind === "bad_response_critical_ext") {
      const respEnv = buildSabiResponseEnvelope(reqEnv, corr, {
        criticalExtensionIds: [9001],
      });
      await writeFrame(
        socket,
        toBinary(
          ExchangeResponseSchema,
          create(ExchangeResponseSchema, { envelope: respEnv }),
        ),
        2_000,
      );
      return;
    }

    const respEnv = dispatchMockService(
      state,
      reqEnv,
      authenticatedPrincipal,
    );
    await writeFrame(
      socket,
      toBinary(
        ExchangeResponseSchema,
        create(ExchangeResponseSchema, { envelope: respEnv }),
      ),
      2_000,
    );
  }
}

function dispatchMockService(
  state: MockServiceState,
  req: Envelope,
  authenticatedPrincipal: Uint8Array,
): Envelope {
  assert.equal(req.commonContext.case, "requestContext");
  const reqCtx = req.commonContext.value;
  const corr = Uint8Array.from(reqCtx.correlationId);
  const idemHex = Buffer.from(reqCtx.idempotencyKey).toString("hex");

  if (req.service === OPERATION_CONTROL_SERVICE) {
    const schema = create(SchemaIdentitySchema, {
      name: OPERATION_CONTROL_SCHEMA,
      major: 1,
      minor: 0,
    });
    if (req.method === "query") {
      const q = fromBinary(QueryOperationRequestSchema, req.payload);
      const opHex = Buffer.from(q.operation!.operationId).toString("hex");
      const entry = state.operations.get(opHex);
      if (!entry) {
        return buildSabiResponseEnvelope(req, corr, {
          failure: create(SabiFailureSchema, {
            code: SabiErrorCode.NOT_FOUND,
            retry: RetryDirective.DO_NOT_RETRY,
            safeMessage: "operation not found",
          }),
        });
      }
      const statusPayload = toBinary(
        OperationStatusSchema,
        create(OperationStatusSchema, {
          schema,
          operation: q.operation,
          state: entry.state,
          cancelEpoch: entry.cancelEpoch,
          receipt:
            entry.state === OperationLifecycleState.CANCELLED_BEFORE_EFFECT
              ? create(ReceiptReferenceSchema, { receiptId: entry.receiptId })
              : undefined,
        }),
      );
      return buildSabiResponseEnvelope(req, corr, {
        payload: statusPayload,
        operation: {
          operationId: q.operation!.operationId,
          generation: q.operation!.generation,
        },
        receipts:
          entry.state === OperationLifecycleState.CANCELLED_BEFORE_EFFECT
            ? [entry.receiptId]
            : [],
      });
    }
    if (req.method === "cancel") {
      const c = fromBinary(CancelOperationRequestSchema, req.payload);
      const opHex = Buffer.from(c.operation!.operationId).toString("hex");
      const entry = state.operations.get(opHex);
      if (!entry) {
        return buildSabiResponseEnvelope(req, corr, {
          failure: create(SabiFailureSchema, {
            code: SabiErrorCode.NOT_FOUND,
            retry: RetryDirective.DO_NOT_RETRY,
            safeMessage: "operation not found",
          }),
        });
      }
      if (c.expectedCancelEpoch !== entry.cancelEpoch) {
        return buildSabiResponseEnvelope(req, corr, {
          failure: create(SabiFailureSchema, {
            code: SabiErrorCode.CONFLICT,
            retry: RetryDirective.DO_NOT_RETRY,
            safeMessage: "cancel epoch conflict",
          }),
        });
      }
      entry.cancelEpoch += 1n;
      entry.state = OperationLifecycleState.CANCELLED_BEFORE_EFFECT;
      const statusPayload = toBinary(
        OperationStatusSchema,
        create(OperationStatusSchema, {
          schema,
          operation: c.operation,
          state: entry.state,
          cancelEpoch: entry.cancelEpoch,
          receipt: create(ReceiptReferenceSchema, { receiptId: entry.receiptId }),
        }),
      );
      return buildSabiResponseEnvelope(req, corr, {
        payload: statusPayload,
        operation: {
          operationId: c.operation!.operationId,
          generation: c.operation!.generation,
        },
        receipts: [entry.receiptId],
      });
    }
  }

  if (req.service === LLM_GATEWAY_SERVICE) {
    const schema = create(SchemaIdentitySchema, {
      name: LLM_GATEWAY_SCHEMA,
      major: 1,
      minor: 0,
    });
    if (req.method === "query_budget") {
      const payload = toBinary(
        QueryBudgetResultSchema,
        create(QueryBudgetResultSchema, {
          schema,
          limit: create(LlmUsageWireSchema, { apiCalls: 100n, totalTokens: 10_000n }),
          consumed: create(LlmUsageWireSchema, { apiCalls: 1n, totalTokens: 50n }),
          remaining: create(LlmUsageWireSchema, { apiCalls: 99n, totalTokens: 9_950n }),
          externalAdvisory: create(LlmUsageWireSchema, { apiCalls: 0n }),
        }),
      );
      return buildSabiResponseEnvelope(req, corr, { payload });
    }
    if (req.method === "llm_chat") {
      const chatReq = fromBinary(LlmChatRequestSchema, req.payload);
      if (state.nextLlmOversizedResponse) {
        state.nextLlmOversizedResponse = false;
        const payload = toBinary(
          LlmChatResultSchema,
          create(LlmChatResultSchema, {
            schema,
            model: chatReq.model,
            contentUtf8: "x".repeat(MAX_LLM_GATEWAY_PAYLOAD_BYTES + 100),
            usage: create(LlmUsageWireSchema, { apiCalls: 1n }),
          }),
        );
        return buildSabiResponseEnvelope(req, corr, {
          payload,
          receipts: [state.secretReceiptBytes],
        });
      }
      if (state.nextMutationMissingEvidence) {
        state.nextMutationMissingEvidence = false;
        const payload = toBinary(
          LlmChatResultSchema,
          create(LlmChatResultSchema, {
            schema,
            model: chatReq.model,
            contentUtf8: "missing-evidence",
            usage: create(LlmUsageWireSchema, { apiCalls: 1n }),
          }),
        );
        return buildSabiResponseEnvelope(req, corr, { payload });
      }
      if (state.nextUnsafeUncertainRetry) {
        state.nextUnsafeUncertainRetry = false;
        return buildSabiResponseEnvelope(req, corr, {
          operation: { operationId: filled(0x61, 16), generation: 1n },
          failure: create(SabiFailureSchema, {
            code: SabiErrorCode.UNCERTAIN,
            retry: RetryDirective.DO_NOT_RETRY,
            safeMessage: "unsafe retry directive",
          }),
        });
      }
      if (state.nextUncertainMissingOp) {
        state.nextUncertainMissingOp = false;
        return buildSabiResponseEnvelope(req, corr, {
          failure: create(SabiFailureSchema, {
            code: SabiErrorCode.UNCERTAIN,
            retry: RetryDirective.QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY,
            safeMessage: "missing operation ref",
          }),
        });
      }
      if (idemHex.startsWith("aa") && !state.llmUncertainSeen.has(idemHex)) {
        state.llmUncertainSeen.add(idemHex);
        return buildSabiResponseEnvelope(req, corr, {
          operation: { operationId: filled(0x71, 16), generation: 1n },
          failure: create(SabiFailureSchema, {
            code: SabiErrorCode.UNCERTAIN,
            retry: RetryDirective.QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY,
            safeMessage: "provider response ambiguous; retry same idempotency key",
          }),
        });
      }
      const payload = toBinary(
        LlmChatResultSchema,
        create(LlmChatResultSchema, {
          schema,
          model: chatReq.model,
          contentUtf8: `reply:${chatReq.turns[0]?.content ?? ""}`,
          usage: create(LlmUsageWireSchema, {
            apiCalls: 1n,
            promptTokens: 5n,
            completionTokens: 7n,
            totalTokens: 12n,
            costMicrocredits: 20n,
          }),
        }),
      );
      return buildSabiResponseEnvelope(req, corr, {
        payload,
        operation: { operationId: filled(0x71, 16), generation: 1n },
        receipts: [state.secretReceiptBytes],
      });
    }
    if (req.method === "attenuate_budget") {
      const att = fromBinary(AttenuateBudgetRequestSchema, req.payload);
      const payload = toBinary(
        AttenuateBudgetResultSchema,
        create(AttenuateBudgetResultSchema, {
          schema,
          grantedLimit: att.childLimit,
          validUntilMs: att.validUntilMs,
        }),
      );
      return buildSabiResponseEnvelope(req, corr, {
        payload,
        receipts: [state.secretReceiptBytes],
      });
    }
    if (req.method === "record_external_usage") {
      fromBinary(RecordExternalUsageRequestSchema, req.payload);
      const replayed = state.llmUncertainSeen.has(`adv:${idemHex}`);
      state.llmUncertainSeen.add(`adv:${idemHex}`);
      const payload = toBinary(
        RecordExternalUsageResultSchema,
        create(RecordExternalUsageResultSchema, { schema, replayed }),
      );
      return buildSabiResponseEnvelope(req, corr, {
        payload,
        receipts: [state.secretReceiptBytes],
      });
    }
  }

  if (req.service === TOOL_SANDBOX_SERVICE) {
    const schema = create(SchemaIdentitySchema, {
      name: TOOL_SANDBOX_SCHEMA,
      major: 1,
      minor: 0,
    });
    if (req.method === "list_tools") {
      fromBinary(ListToolsRequestSchema, req.payload);
      const payload = toBinary(
        ListToolsResultSchema,
        create(ListToolsResultSchema, {
          schema,
          tools: [
            create(ToolDescriptorWireSchema, {
              name: "read_status",
              summary: "read-only tool",
              effectClass: "READ_ONLY",
              compensationHandler: "",
              inputSchemaJson: "{}",
            }),
            create(ToolDescriptorWireSchema, {
              name: "write_note",
              summary: "compensatable tool",
              effectClass: "COMPENSATABLE",
              compensationHandler: "undo_write_note",
              inputSchemaJson: "{\"type\":\"object\"}",
            }),
          ],
        }),
      );
      return buildSabiResponseEnvelope(req, corr, { payload });
    }
    if (req.method === "invoke_tool") {
      const inv = fromBinary(InvokeToolRequestSchema, req.payload);
      if (state.nextToolOversizedResponse) {
        state.nextToolOversizedResponse = false;
        const payload = toBinary(
          InvokeToolResultSchema,
          create(InvokeToolResultSchema, {
            schema,
            outputJson: "z".repeat(MAX_TOOL_SANDBOX_PAYLOAD_BYTES + 128),
          }),
        );
        return buildSabiResponseEnvelope(req, corr, {
          payload,
          receipts: [state.secretReceiptBytes],
        });
      }
      if (idemHex.startsWith("bb") && !state.toolUncertainSeen.has(idemHex)) {
        state.toolUncertainSeen.add(idemHex);
        return buildSabiResponseEnvelope(req, corr, {
          operation: { operationId: filled(0x72, 16), generation: 1n },
          failure: create(SabiFailureSchema, {
            code: SabiErrorCode.EFFECT_UNKNOWN,
            retry: RetryDirective.QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY,
            safeMessage: "sandbox crashed post-dispatch",
          }),
        });
      }
      const payload = toBinary(
        InvokeToolResultSchema,
        create(InvokeToolResultSchema, {
          schema,
          outputJson: JSON.stringify({ tool: inv.tool, ok: true }),
        }),
      );
      return buildSabiResponseEnvelope(req, corr, {
        payload,
        operation: { operationId: filled(0x72, 16), generation: 1n },
        receipts: [state.secretReceiptBytes],
      });
    }
  }

  if (req.service === BLOB_REFERENCE_SERVICE) {
    const schema = create(SchemaIdentitySchema, {
      name: BLOB_REFERENCE_SCHEMA,
      major: 1,
      minor: 0,
    });
    if (req.method === "mint_reference") {
      const mint = fromBinary(MintReferenceRequestSchema, req.payload);
      const refId = filled(0x55, 16);
      state.blobs.set(Buffer.from(refId).toString("hex"), {
        bytes: state.secretBlobBytes,
        chunkSize: mint.maxChunkBytes,
        recipientPrincipalId: Uint8Array.from(mint.recipientPrincipalId),
        released: false,
      });
      const payload = toBinary(
        MintReferenceResultSchema,
        create(MintReferenceResultSchema, {
          schema,
          referenceId: refId,
          mintedAtMs: 5_000n,
        }),
      );
      return buildSabiResponseEnvelope(req, corr, {
        payload,
        receipts: [state.secretReceiptBytes],
      });
    }
    if (req.method === "read_reference") {
      const readReq = fromBinary(ReadReferenceRequestSchema, req.payload);
      const token = state.blobs.get(Buffer.from(readReq.referenceId).toString("hex"));
      if (!token || token.released) {
        return buildSabiResponseEnvelope(req, corr, {
          failure: create(SabiFailureSchema, {
            code: SabiErrorCode.NOT_FOUND,
            retry: RetryDirective.DO_NOT_RETRY,
            safeMessage: "blob reference unavailable",
          }),
        });
      }
      if (Buffer.compare(authenticatedPrincipal, token.recipientPrincipalId) !== 0) {
        return buildSabiResponseEnvelope(req, corr, {
          failure: create(SabiFailureSchema, {
            code: SabiErrorCode.RIGHTS,
            retry: RetryDirective.DO_NOT_RETRY,
            safeMessage: "recipient principal mismatch",
          }),
        });
      }
      const corruption = state.nextReadCorruption;
      state.nextReadCorruption = undefined;
      if (corruption === "oversized_chunk") {
        const oversize = new Uint8Array(MAX_BLOB_CHUNK_BYTES + 1).fill(0x41);
        const payload = toBinary(
          ReadReferenceResultSchema,
          create(ReadReferenceResultSchema, {
            schema,
            chunk: oversize,
            offset: readReq.offset,
            chunkDigestSha256: sha256(oversize),
          }),
        );
        return buildSabiResponseEnvelope(req, corr, {
          payload,
          receipts: [state.secretReceiptBytes],
        });
      }
      if (corruption === "oversized_frame") {
        // Chunk <= 65408 bytes, but large nonCriticalExtensionIds pushes encoded payload > 65536
        const chunk = new Uint8Array(MAX_BLOB_CHUNK_BYTES).fill(0x42);
        const payload = toBinary(
          ReadReferenceResultSchema,
          create(ReadReferenceResultSchema, {
            schema: create(SchemaIdentitySchema, {
              name: BLOB_REFERENCE_SCHEMA,
              major: 1,
              minor: 0,
              nonCriticalExtensionIds: Array.from({ length: 120 }, (_, i) => i + 1),
            }),
            chunk,
            offset: readReq.offset,
            chunkDigestSha256: sha256(chunk),
          }),
        );
        assert.ok(payload.byteLength > MAX_BLOB_FRAME_BYTES);
        return buildSabiResponseEnvelope(req, corr, {
          payload,
          receipts: [state.secretReceiptBytes],
        });
      }
      const start = Number(readReq.offset);
      const end = Math.min(token.bytes.byteLength, start + Number(token.chunkSize));
      const chunk = Uint8Array.from(token.bytes.subarray(start, end));
      const digest = sha256(chunk);
      if (corruption === "corrupt_digest") {
        chunk[0] = (chunk[0]! ^ 0xff) & 0xff;
      }
      const payload = toBinary(
        ReadReferenceResultSchema,
        create(ReadReferenceResultSchema, {
          schema,
          chunk,
          offset: corruption === "wrong_offset" ? readReq.offset + 1n : readReq.offset,
          chunkDigestSha256: digest,
        }),
      );
      return buildSabiResponseEnvelope(req, corr, {
        payload,
        receipts: [state.secretReceiptBytes],
      });
    }
    if (req.method === "release_reference") {
      const rel = fromBinary(ReleaseReferenceRequestSchema, req.payload);
      const token = state.blobs.get(Buffer.from(rel.referenceId).toString("hex"));
      if (token) {
        token.released = true;
      }
      const payload = toBinary(
        ReleaseReferenceResultSchema,
        create(ReleaseReferenceResultSchema, { schema, released: Boolean(token) }),
      );
      return buildSabiResponseEnvelope(req, corr, {
        payload,
        receipts: [state.secretReceiptBytes],
      });
    }
  }

  return buildSabiResponseEnvelope(req, corr, {
    failure: create(SabiFailureSchema, {
      code: SabiErrorCode.NOT_SUPPORTED,
      retry: RetryDirective.DO_NOT_RETRY,
      safeMessage: "unsupported service or method",
    }),
  });
}

async function runLocalFrameSecuritySuite(): Promise<void> {
  const endpoint = makeEndpoint("local-sabi-v11");
  const clientSeed = filled(0xab, 32);
  const signer = new Ed25519Signer(clientSeed);
  const principalId = filled(0x11, 16);
  const secretReceipt = filled(0xcd, 16);
  const secretBlobText = "SECRET_BLOB_SENTINEL_XYZ_987654321_CONFIDENTIAL";
  const secretBlobBytes = Uint8Array.from(
    Buffer.concat([
      Buffer.from(secretBlobText, "utf8"),
      Buffer.alloc(40, 0x5c),
    ]),
  );

  const opId = filled(0x91, 16);
  const state: MockServiceState = {
    endpoint,
    expectedPrincipalId: principalId,
    expectedPublicKey: signer.publicKey,
    secretBlobBytes,
    secretReceiptBytes: secretReceipt,
    operations: new Map([
      [
        Buffer.from(opId).toString("hex"),
        {
          state: OperationLifecycleState.DISPATCHED,
          cancelEpoch: 0n,
          receiptId: secretReceipt,
        },
      ],
    ]),
    llmUncertainSeen: new Set(),
    toolUncertainSeen: new Set(),
    blobs: new Map(),
    nextBehavior: { kind: "authenticated_sabi" },
  };

  const mockServer = await startLocalMockServer(state);
  const transportConfig: Partial<TransportConfig> = {
    connectTimeoutMs: 2_000,
    readTimeoutMs: 250,
    writeTimeoutMs: 2_000,
  };

  try {
    // 1. Positive round-trips for OperationClient, LlmGatewayClient, ToolSandboxClient, BlobReferenceClient
    //    all sharing ONE persistent authenticated socket connection!
    const { client: rpc, identity } = await connectAuthenticatedSession(
      endpoint,
      signer,
      principalId,
      { transport: transportConfig },
    );

    try {
      const opClient = new OperationClient(rpc, { identity });
      const opInitial = await opClient.query({ operationId: opId, generation: 1n });
      assert.equal(opInitial.state, OperationLifecycleState.DISPATCHED);
      assert.equal(opInitial.cancelEpoch, 0n);

      const opCancelled = await opClient.cancel(
        { operationId: opId, generation: 1n },
        0n,
      );
      assert.equal(
        opCancelled.state,
        OperationLifecycleState.CANCELLED_BEFORE_EFFECT,
      );
      assert.equal(opCancelled.cancelEpoch, 1n);

      // Stale expectedCancelEpoch -> typed Conflict
      await assert.rejects(
        opClient.cancel({ operationId: opId, generation: 1n }, 0n),
        (err: unknown) =>
          err instanceof OperationControlError &&
          err.code === SabiErrorCode.CONFLICT,
      );

      // LlmGatewayClient positive + replay
      const llm = new LlmGatewayClient(rpc, { identity });
      const budget = await llm.queryBudget();
      assert.equal(budget.remaining?.apiCalls, 99n);

      const chat = await llm.llmChat(
        {
          model: "unit-model",
          turns: [{ role: "user", content: "hello-v1.1" }],
          maxOutputTokens: 64,
          temperatureMilli: 200,
        },
        {
          idempotencyKey: filled(0x21, 16),
          deadlineMonotonicNs: 9_000_000_000n,
        },
      );
      assert.equal(chat.contentUtf8, "reply:hello-v1.1");

      const att = await llm.attenuateBudget(
        {
          childLimit: {
            apiCalls: 10n,
            promptTokens: 100n,
            completionTokens: 100n,
            totalTokens: 200n,
            costMicrocredits: 500n,
          },
          validUntilMs: 99_000n,
        },
        {
          idempotencyKey: filled(0x22, 16),
          deadlineMonotonicNs: 9_000_000_000n,
        },
      );
      assert.equal(att.grantedLimit?.apiCalls, 10n);

      const advKey = filled(0x23, 16);
      const adv1 = await llm.recordExternalUsage(
        {
          provider: "unit-provider",
          model: "unit-model",
          usage: {
            apiCalls: 1n,
            promptTokens: 2n,
            completionTokens: 3n,
            totalTokens: 5n,
            costMicrocredits: 1n,
          },
          recordedAtMs: 12_000n,
        },
        { idempotencyKey: advKey, deadlineMonotonicNs: 9_000_000_000n },
      );
      assert.equal(adv1.replayed, false);
      const adv2 = await llm.recordExternalUsage(
        {
          provider: "unit-provider",
          model: "unit-model",
          usage: {
            apiCalls: 1n,
            promptTokens: 2n,
            completionTokens: 3n,
            totalTokens: 5n,
            costMicrocredits: 1n,
          },
          recordedAtMs: 12_000n,
        },
        { idempotencyKey: advKey, deadlineMonotonicNs: 9_000_000_000n },
      );
      assert.equal(adv2.replayed, true);

      // ToolSandboxClient positive
      const tools = new ToolSandboxClient(rpc, { identity });
      const catalog = await tools.listTools({
        deadlineMonotonicNs: 9_000_000_000n,
      });
      assert.equal(catalog.tools.length, 2);
      assert.equal(catalog.tools[1]?.compensationHandler, "undo_write_note");
      const invoked = await tools.invokeTool(
        { tool: "read_status", argumentsJson: "{}" },
        {
          idempotencyKey: filled(0x24, 16),
          deadlineMonotonicNs: 9_000_000_000n,
        },
      );
      assert.deepEqual(JSON.parse(invoked.outputJson), {
        tool: "read_status",
        ok: true,
      });

      // BlobReferenceClient multi-chunk SHA-256 verified read + release
      const blob = new BlobReferenceClient(rpc, { identity });
      const chunkSize = 32n;
      const minted = await blob.mintReference(
        {
          pathHandle: "workspace/secret.bin",
          fileDigestSha256: sha256(secretBlobBytes),
          sizeBytes: BigInt(secretBlobBytes.byteLength),
          maxReads: 8n,
          maxChunkBytes: chunkSize,
          validUntilMs: 9_999_999n,
          recipientPrincipalId: principalId,
        },
        { deadlineMonotonicNs: 9_000_000_000n },
      );
      assert.equal(minted.referenceId.byteLength, 16);

      const chunks: Uint8Array[] = [];
      for (
        let offset = 0n;
        offset < BigInt(secretBlobBytes.byteLength);
        offset += chunkSize
      ) {
        const res = await blob.readReference({
          referenceId: minted.referenceId,
          offset,
          lengthBytes: chunkSize,
        });
        chunks.push(res.chunk);
      }
      assert.deepEqual(Buffer.concat(chunks), Buffer.from(secretBlobBytes));

      // Unaligned offset rejected fail-closed locally
      await assert.rejects(
        blob.readReference({
          referenceId: minted.referenceId,
          offset: 7n,
          lengthBytes: chunkSize,
        }),
        (err: unknown) =>
          err instanceof BlobReferenceValidationError &&
          err.code === "UNALIGNED_OFFSET",
      );

      // Corrupted chunk SHA-256 rejected fail-closed with zero raw blob leakage
      state.nextReadCorruption = "corrupt_digest";
      await assert.rejects(
        blob.readReference({
          referenceId: minted.referenceId,
          offset: 0n,
          lengthBytes: chunkSize,
        }),
        (err: unknown) => {
          assertNoSensitiveLeakage(
            err,
            [secretBlobBytes, secretBlobText, secretReceipt, clientSeed],
            "blob digest mismatch",
          );
          return (
            err instanceof BlobReferenceValidationError &&
            err.code === "CHUNK_DIGEST_MISMATCH"
          );
        },
      );

      // Offset mismatch rejected
      state.nextReadCorruption = "wrong_offset";
      await assert.rejects(
        blob.readReference({
          referenceId: minted.referenceId,
          offset: 0n,
          lengthBytes: chunkSize,
        }),
        (err: unknown) =>
          err instanceof BlobReferenceValidationError &&
          err.code === "OFFSET_MISMATCH",
      );

      // >65408 server chunk rejected with FRAME_TOO_LARGE
      state.nextReadCorruption = "oversized_chunk";
      await assert.rejects(
        blob.readReference({
          referenceId: minted.referenceId,
          offset: 0n,
          lengthBytes: chunkSize,
        }),
        (err: unknown) =>
          err instanceof IpcError && err.code === "FRAME_TOO_LARGE",
      );

      // >65536 complete encoded ReadReferenceResult rejected with FRAME_TOO_LARGE
      state.nextReadCorruption = "oversized_frame";
      await assert.rejects(
        blob.readReference({
          referenceId: minted.referenceId,
          offset: 0n,
          lengthBytes: BigInt(MAX_BLOB_CHUNK_BYTES),
        }),
        (err: unknown) =>
          err instanceof IpcError && err.code === "FRAME_TOO_LARGE",
      );

      const released = await blob.releaseReference(minted.referenceId);
      assert.equal(released.released, true);
      pass(
        "LlmGateway/ToolSandbox/BlobReference/Operation positive round-trips + chunk SHA-256 + bounds",
      );

      // 2. Mandatory idempotencyKey & deadline + BlobReference parameter bounds
      await assert.rejects(
        llm.llmChat(
          { model: "unit-model", turns: [{ role: "user", content: "hi" }] },
          { deadlineMonotonicNs: 9_000_000_000n },
        ),
        (err: unknown) =>
          err instanceof CommonSemanticsError &&
          err.code === "MISSING_IDEMPOTENCY_KEY",
      );
      await assert.rejects(
        llm.llmChat(
          { model: "unit-model", turns: [{ role: "user", content: "hi" }] },
          { idempotencyKey: filled(0x31, 16) },
        ),
        (err: unknown) =>
          err instanceof CommonSemanticsError && err.code === "MISSING_DEADLINE",
      );
      await assert.rejects(
        llm.attenuateBudget(
          {
            childLimit: {
              apiCalls: 1n,
              promptTokens: 1n,
              completionTokens: 1n,
              totalTokens: 2n,
              costMicrocredits: 1n,
            },
            validUntilMs: 5_000n,
          },
          { idempotencyKey: filled(0x31, 16) },
        ),
        (err: unknown) =>
          err instanceof CommonSemanticsError && err.code === "MISSING_DEADLINE",
      );
      await assert.rejects(
        llm.recordExternalUsage(
          {
            provider: "p",
            model: "m",
            usage: {
              apiCalls: 1n,
              promptTokens: 0n,
              completionTokens: 0n,
              totalTokens: 0n,
              costMicrocredits: 0n,
            },
            recordedAtMs: 1n,
          },
          { deadlineMonotonicNs: 9_000_000_000n },
        ),
        (err: unknown) =>
          err instanceof CommonSemanticsError &&
          err.code === "MISSING_IDEMPOTENCY_KEY",
      );
      await assert.rejects(
        tools.listTools(),
        (err: unknown) =>
          err instanceof CommonSemanticsError && err.code === "MISSING_DEADLINE",
      );
      await assert.rejects(
        tools.invokeTool(
          { tool: "read_status", argumentsJson: "{}" },
          { deadlineMonotonicNs: 9_000_000_000n },
        ),
        (err: unknown) =>
          err instanceof CommonSemanticsError &&
          err.code === "MISSING_IDEMPOTENCY_KEY",
      );
      await assert.rejects(
        tools.invokeTool(
          { tool: "read_status", argumentsJson: "{}" },
          { idempotencyKey: filled(0x32, 16) },
        ),
        (err: unknown) =>
          err instanceof CommonSemanticsError && err.code === "MISSING_DEADLINE",
      );

      // 64 KiB outbound frame rejections (without poisoning rpc)
      await assert.rejects(
        llm.llmChat(
          {
            model: "unit-model",
            turns: [{ role: "user", content: "a".repeat(MAX_LLM_GATEWAY_PAYLOAD_BYTES + 1) }],
          },
          {
            idempotencyKey: filled(0x33, 16),
            deadlineMonotonicNs: 9_000_000_000n,
          },
        ),
        (err: unknown) =>
          err instanceof IpcError && err.code === "FRAME_TOO_LARGE",
      );
      await assert.rejects(
        tools.invokeTool(
          {
            tool: "read_status",
            argumentsJson: JSON.stringify({
              blob: "b".repeat(MAX_TOOL_SANDBOX_PAYLOAD_BYTES + 1),
            }),
          },
          {
            idempotencyKey: filled(0x34, 16),
            deadlineMonotonicNs: 9_000_000_000n,
          },
        ),
        (err: unknown) =>
          err instanceof IpcError && err.code === "FRAME_TOO_LARGE",
      );

      // 64 KiB inbound response rejections
      state.nextLlmOversizedResponse = true;
      await assert.rejects(
        llm.llmChat(
          { model: "unit-model", turns: [{ role: "user", content: "ok" }] },
          {
            idempotencyKey: filled(0x35, 16),
            deadlineMonotonicNs: 9_000_000_000n,
          },
        ),
        (err: unknown) =>
          err instanceof IpcError && err.code === "FRAME_TOO_LARGE",
      );
      state.nextToolOversizedResponse = true;
      await assert.rejects(
        tools.invokeTool(
          { tool: "read_status", argumentsJson: "{}" },
          {
            idempotencyKey: filled(0x36, 16),
            deadlineMonotonicNs: 9_000_000_000n,
          },
        ),
        (err: unknown) =>
          err instanceof IpcError && err.code === "FRAME_TOO_LARGE",
      );

      // BlobReference mint bounds (recipientPrincipalId != 16B, chunk > 65408, chunk == 0)
      for (const badRecipient of [new Uint8Array(0), filled(0x01, 15), filled(0x01, 17)]) {
        await assert.rejects(
          blob.mintReference(
            {
              pathHandle: "a.bin",
              fileDigestSha256: sha256(secretBlobBytes),
              sizeBytes: BigInt(secretBlobBytes.byteLength),
              maxReads: 1n,
              maxChunkBytes: 32n,
              validUntilMs: 9_999_999n,
              recipientPrincipalId: badRecipient,
            },
            { deadlineMonotonicNs: 9_000_000_000n },
          ),
          (err: unknown) =>
            err instanceof BlobReferenceValidationError &&
            err.code === "INVALID_RECIPIENT_PRINCIPAL",
        );
      }
      await assert.rejects(
        blob.mintReference(
          {
            pathHandle: "a.bin",
            fileDigestSha256: sha256(secretBlobBytes),
            sizeBytes: BigInt(secretBlobBytes.byteLength),
            maxReads: 1n,
            maxChunkBytes: BigInt(MAX_BLOB_CHUNK_BYTES + 1),
            validUntilMs: 9_999_999n,
            recipientPrincipalId: principalId,
          },
          { deadlineMonotonicNs: 9_000_000_000n },
        ),
        (err: unknown) =>
          err instanceof IpcError && err.code === "FRAME_TOO_LARGE",
      );
      await assert.rejects(
        blob.mintReference(
          {
            pathHandle: "a.bin",
            fileDigestSha256: sha256(secretBlobBytes),
            sizeBytes: BigInt(secretBlobBytes.byteLength),
            maxReads: 1n,
            maxChunkBytes: 0n,
            validUntilMs: 9_999_999n,
            recipientPrincipalId: principalId,
          },
          { deadlineMonotonicNs: 9_000_000_000n },
        ),
        (err: unknown) =>
          err instanceof BlobReferenceValidationError &&
          err.code === "INVALID_CHUNK_BOUND",
      );
      await assert.rejects(
        blob.readReference({
          referenceId: filled(0x55, 16),
          offset: 0n,
          lengthBytes: BigInt(MAX_BLOB_CHUNK_BYTES + 1),
        }),
        (err: unknown) =>
          err instanceof IpcError && err.code === "FRAME_TOO_LARGE",
      );

      // Holder/recipient mismatch (both local client-side and remote server-enforced)
      const otherPrincipal = filled(0x77, 16);
      const mintedForOther = await blob.mintReference(
        {
          pathHandle: "other/secret.bin",
          fileDigestSha256: sha256(secretBlobBytes),
          sizeBytes: BigInt(secretBlobBytes.byteLength),
          maxReads: 4n,
          maxChunkBytes: 32n,
          validUntilMs: 9_999_999n,
          recipientPrincipalId: otherPrincipal,
        },
        { deadlineMonotonicNs: 9_000_000_000n },
      );
      await assert.rejects(
        blob.readReference({
          referenceId: mintedForOther.referenceId,
          offset: 0n,
          lengthBytes: 32n,
        }),
        (err: unknown) => {
          assertNoSensitiveLeakage(
            err,
            [secretBlobBytes, secretBlobText, secretReceipt, clientSeed],
            "holder mismatch local",
          );
          return (
            err instanceof BlobReferenceValidationError &&
            err.code === "HOLDER_MISMATCH"
          );
        },
      );
      // Remote server-enforced holder mismatch (fresh BlobReferenceClient without local token cache)
      const blindBlobClient = new BlobReferenceClient(rpc, { identity });
      await assert.rejects(
        blindBlobClient.readReference({
          referenceId: mintedForOther.referenceId,
          offset: 0n,
          lengthBytes: 32n,
        }),
        (err: unknown) => {
          assertNoSensitiveLeakage(
            err,
            [secretBlobBytes, secretBlobText, secretReceipt, clientSeed],
            "holder mismatch remote",
          );
          return (
            err instanceof BlobReferenceError &&
            err.code === SabiErrorCode.RIGHTS
          );
        },
      );
      pass(
        "Mandatory idempotency/deadline, 64 KiB frame limits, and holder/recipient mismatch fail-closed",
      );

      // 3. Uncertain outcome + same idempotencyKey retry + unsafe response rejection
      const uncertainLlmKey = filled(0xaa, 16);
      await assert.rejects(
        llm.llmChat(
          { model: "unit-model", turns: [{ role: "user", content: "retry-me" }] },
          {
            idempotencyKey: uncertainLlmKey,
            deadlineMonotonicNs: 9_000_000_000n,
          },
        ),
        (err: unknown) => {
          assertNoSensitiveLeakage(
            err,
            [secretReceipt, clientSeed],
            "uncertain llm",
          );
          return (
            err instanceof LlmGatewayError &&
            err.code === SabiErrorCode.UNCERTAIN &&
            err.retry ===
              RetryDirective.QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY &&
            err.operation !== undefined
          );
        },
      );
      const llmRecovered = await llm.llmChat(
        { model: "unit-model", turns: [{ role: "user", content: "retry-me" }] },
        {
          idempotencyKey: uncertainLlmKey,
          deadlineMonotonicNs: 9_000_000_000n,
        },
      );
      assert.equal(llmRecovered.contentUtf8, "reply:retry-me");

      const uncertainToolKey = filled(0xbb, 16);
      await assert.rejects(
        tools.invokeTool(
          { tool: "write_note", argumentsJson: "{\"n\":1}" },
          {
            idempotencyKey: uncertainToolKey,
            deadlineMonotonicNs: 9_000_000_000n,
          },
        ),
        (err: unknown) =>
          err instanceof ToolSandboxError &&
          err.code === SabiErrorCode.EFFECT_UNKNOWN &&
          err.retry ===
            RetryDirective.QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY &&
          err.operation !== undefined,
      );
      const toolRecovered = await tools.invokeTool(
        { tool: "write_note", argumentsJson: "{\"n\":1}" },
        {
          idempotencyKey: uncertainToolKey,
          deadlineMonotonicNs: 9_000_000_000n,
        },
      );
      assert.deepEqual(JSON.parse(toolRecovered.outputJson), {
        tool: "write_note",
        ok: true,
      });

      // Malformed common response semantics (missing effect evidence / unsafe retry / missing operation)
      state.nextMutationMissingEvidence = true;
      await assert.rejects(
        llm.llmChat(
          { model: "unit-model", turns: [{ role: "user", content: "no-ev" }] },
          {
            idempotencyKey: filled(0x45, 16),
            deadlineMonotonicNs: 9_000_000_000n,
          },
        ),
        (err: unknown) =>
          err instanceof CommonSemanticsError &&
          err.code === "MISSING_EFFECT_EVIDENCE",
      );
      state.nextUnsafeUncertainRetry = true;
      await assert.rejects(
        llm.llmChat(
          { model: "unit-model", turns: [{ role: "user", content: "bad-retry" }] },
          {
            idempotencyKey: filled(0x46, 16),
            deadlineMonotonicNs: 9_000_000_000n,
          },
        ),
        (err: unknown) =>
          err instanceof CommonSemanticsError && err.code === "UNSAFE_RETRY",
      );
      state.nextUncertainMissingOp = true;
      await assert.rejects(
        llm.llmChat(
          { model: "unit-model", turns: [{ role: "user", content: "no-op" }] },
          {
            idempotencyKey: filled(0x47, 16),
            deadlineMonotonicNs: 9_000_000_000n,
          },
        ),
        (err: unknown) =>
          err instanceof CommonSemanticsError && err.code === "MISSING_OPERATION",
      );
      pass(
        "Uncertain/EffectUnknown same-idempotencyKey safe retry + response semantics guard",
      );
    } finally {
      rpc.close();
    }

    // 4. Transport faults -> poison -> reject reuse -> reconnect recovery
    const faultCases: {
      behavior: SocketConnectionBehavior;
      expectedCode: IpcError["code"];
    }[] = [
      { behavior: { kind: "timeout" }, expectedCode: "TIMEOUT" },
      { behavior: { kind: "half_header" }, expectedCode: "READ" },
      { behavior: { kind: "half_body" }, expectedCode: "READ" },
      { behavior: { kind: "eof_immediate" }, expectedCode: "READ" },
      { behavior: { kind: "trailing_bytes" }, expectedCode: "READ" },
      { behavior: { kind: "malformed_protobuf" }, expectedCode: "COMPATIBILITY" },
      { behavior: { kind: "wrong_request_id" }, expectedCode: "REQUEST_ID_MISMATCH" },
      { behavior: { kind: "wrong_correlation_id" }, expectedCode: "CORRELATION_ID_MISMATCH" },
      { behavior: { kind: "bad_response_schema_major" }, expectedCode: "COMPATIBILITY" },
      { behavior: { kind: "bad_response_critical_ext" }, expectedCode: "COMPATIBILITY" },
    ];

    for (const fault of faultCases) {
      state.nextBehavior = fault.behavior;
      const faultyRpc = await connectAuthenticated(
        endpoint,
        signer,
        principalId,
        transportConfig,
      );
      try {
        const llmFault = new LlmGatewayClient(faultyRpc, {
          identity: SessionIdentity.derive(principalId),
        });
        await assert.rejects(
          llmFault.queryBudget(),
          (err: unknown) => {
            assertNoSensitiveLeakage(
              err,
              [clientSeed, secretReceipt, secretBlobBytes],
              `fault ${fault.behavior.kind}`,
            );
            return err instanceof IpcError && err.code === fault.expectedCode;
          },
        );
        assert.equal(faultyRpc.isUsable, false);
        await assert.rejects(
          llmFault.queryBudget(),
          (err: unknown) =>
            err instanceof IpcError && err.code === "CONNECTION_UNUSABLE",
        );
      } finally {
        faultyRpc.close();
      }
    }

    // Handshake-phase fail-closed tests
    for (const hsFault of [
      { kind: "bad_challenge_schema" },
      { kind: "bad_challenge_nonce" },
      { kind: "handshake_eof" },
    ] as const) {
      state.nextBehavior = hsFault;
      await assert.rejects(
        connectAuthenticated(endpoint, signer, principalId, transportConfig),
        (err: unknown) => {
          assertNoSensitiveLeakage(err, [clientSeed], `handshake ${hsFault.kind}`);
          return (
            err instanceof IpcError &&
            (err.code === "COMPATIBILITY" || err.code === "READ")
          );
        },
      );
    }

    // Reconnect recovery after poisoned connections
    const recoveredRpc = await connectAuthenticated(
      endpoint,
      signer,
      principalId,
      transportConfig,
    );
    try {
      const recoveredLlm = new LlmGatewayClient(recoveredRpc, {
        identity: SessionIdentity.derive(principalId),
      });
      const recoveredBudget = await recoveredLlm.queryBudget();
      assert.equal(recoveredBudget.remaining?.apiCalls, 99n);
    } finally {
      recoveredRpc.close();
    }
    pass(
      "Transport faults (timeout/half-frame/EOF/trailing/corrupt/mismatch), handshake fail-closed, poison & reconnect recovery",
    );
  } finally {
    await mockServer.close();
    if (process.platform !== "win32") {
      await rm(endpoint, { force: true });
    }
  }
}

await runStaticChecks();
await runRustDaemonSuite();
await runLocalFrameSecuritySuite();
console.log(`[sabi_v1_1_typescript] ALL ${passedCases} SUITES PASSED`);
