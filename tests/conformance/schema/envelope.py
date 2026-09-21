from pathlib import Path
import sys

from google.protobuf.message import DecodeError

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "gen" / "python"))

from nlos.sabi.v1 import (  # noqa: E402
    envelope_pb2,
    service_directory_pb2,
    system_control_pb2,
)
sys.path.insert(0, str(ROOT / "sdk" / "python"))
from nlos_sdk.common import (  # noqa: E402
    CommonSemanticsError,
    MethodSemantics,
    validate_request_context,
    validate_response_context,
)


SCHEMA_NAME = "nlos.sabi.Envelope"
GOLDEN = bytes.fromhex(
    (ROOT / "schema/golden/nlos.sabi.Envelope-v1.hex").read_text().strip()
)


def validate(envelope: envelope_pb2.Envelope) -> None:
    assert envelope.HasField("schema")
    assert envelope.schema.name == SCHEMA_NAME
    assert envelope.schema.major == 1, "unknown major must fail closed"
    assert not envelope.schema.critical_extension_ids, (
        "unknown critical extensions must fail closed"
    )
    assert len(envelope.request_id) == 16
    assert envelope.service
    assert envelope.method


decoded = envelope_pb2.Envelope.FromString(GOLDEN)
validate(decoded)
assert decoded.schema.minor == 0
assert list(decoded.schema.non_critical_extension_ids) == [42]
assert decoded.service == "operation"
assert decoded.method == "get"
assert decoded.payload == b"abc"
assert decoded.SerializeToString(deterministic=True) == GOLDEN

compatible = envelope_pb2.Envelope.FromString(GOLDEN)
compatible.schema.minor = 99
compatible.schema.non_critical_extension_ids.append(7_001)
validate(compatible)

wrong_major = envelope_pb2.Envelope.FromString(GOLDEN)
wrong_major.schema.major = 2
try:
    validate(wrong_major)
except AssertionError as error:
    assert "unknown major" in str(error)
else:
    raise AssertionError("unknown major was accepted")

unknown_critical = envelope_pb2.Envelope.FromString(GOLDEN)
unknown_critical.schema.critical_extension_ids.append(7_001)
try:
    validate(unknown_critical)
except AssertionError as error:
    assert "unknown critical" in str(error)
else:
    raise AssertionError("unknown critical extension was accepted")

with_unknown_field = GOLDEN + bytes((0xA0, 0x06, 0x07))
try:
    unknown_decoded = envelope_pb2.Envelope.FromString(with_unknown_field)
except DecodeError as error:
    raise AssertionError("unknown protobuf field was rejected") from error
assert unknown_decoded.SerializeToString(deterministic=True) == with_unknown_field

local_rpc = envelope_pb2.DESCRIPTOR.services_by_name["LocalRpcService"]
exchange = local_rpc.methods_by_name["Exchange"]
assert exchange.client_streaming is False
assert exchange.server_streaming is False
assert exchange.input_type.full_name == "nlos.sabi.v1.ExchangeRequest"
assert exchange.output_type.full_name == "nlos.sabi.v1.ExchangeResponse"

directory_golden = bytes.fromhex(
    (
        ROOT
        / "schema/golden/nlos.sabi.ServiceDirectory.ResolveRequest-v1.hex"
    ).read_text().strip()
)
resolve_request = service_directory_pb2.ResolveServiceRequest.FromString(
    directory_golden
)
assert resolve_request.schema.name == "nlos.sabi.ServiceDirectory"
assert resolve_request.schema.major == 1
assert resolve_request.service == "operation"
assert resolve_request.SerializeToString(deterministic=True) == directory_golden
assert service_directory_pb2.LOCAL_TRANSPORT_KIND_UNIX_SOCKET == 1
assert service_directory_pb2.LOCAL_TRANSPORT_KIND_WINDOWS_NAMED_PIPE == 2

common_request_golden = bytes.fromhex(
    (
        ROOT
        / "schema/golden/nlos.sabi.Envelope-common-request-v1.hex"
    ).read_text().strip()
)
common_request = envelope_pb2.Envelope.FromString(common_request_golden)
request_context = validate_request_context(
    common_request,
    MethodSemantics(side_effecting=True, long_running=True),
    123_455,
)
assert common_request.schema.minor == 1
assert request_context.caller.process_generation == 7
assert request_context.idempotency_key == bytes([6]) * 16
assert common_request.SerializeToString(deterministic=True) == common_request_golden

request_context.idempotency_key = b""
try:
    validate_request_context(
        common_request,
        MethodSemantics(side_effecting=True),
        0,
    )
except CommonSemanticsError as error:
    assert error.code == "MISSING_IDEMPOTENCY_KEY"
else:
    raise AssertionError("mutation without idempotency key was accepted")

uncertain_golden = bytes.fromhex(
    (
        ROOT
        / "schema/golden/nlos.sabi.Envelope-common-uncertain-v1.hex"
    ).read_text().strip()
)
uncertain = envelope_pb2.Envelope.FromString(uncertain_golden)
response_context = validate_response_context(
    uncertain,
    MethodSemantics(side_effecting=True, long_running=True),
)
assert response_context.operation.generation == 4
assert response_context.failure.code == envelope_pb2.SABI_ERROR_CODE_UNCERTAIN
assert (
    response_context.failure.retry
    == envelope_pb2.RETRY_DIRECTIVE_QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY
)
assert uncertain.SerializeToString(deterministic=True) == uncertain_golden

terminal_rejection = envelope_pb2.Envelope.FromString(
    uncertain.SerializeToString(deterministic=True)
)
terminal_rejection.response_context.ClearField("operation")
del terminal_rejection.response_context.receipts[:]
terminal_rejection.response_context.failure.code = envelope_pb2.SABI_ERROR_CODE_RIGHTS
terminal_rejection.response_context.failure.retry = (
    envelope_pb2.RETRY_DIRECTIVE_DO_NOT_RETRY
)
terminal_rejection.response_context.failure.safe_message = "authorization denied"
validate_response_context(
    terminal_rejection,
    MethodSemantics(side_effecting=True, long_running=False),
)

response_context.failure.retry = (
    envelope_pb2.RETRY_DIRECTIVE_RETRY_SAME_IDEMPOTENCY_KEY
)
try:
    validate_response_context(
        uncertain,
        MethodSemantics(side_effecting=True, long_running=True),
    )
except CommonSemanticsError as error:
    assert error.code == "UNSAFE_RETRY"
else:
    raise AssertionError("unsafe uncertain retry directive was accepted")


ARTIFACT_RECOVERY_SNAPSHOT_GOLDEN_HEX = (
    "0a1b0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001125708"
    "0310111817200b280230f40338044003480150095a140a101111111111111111"
    "111111111111111110015a140a10222222222222222222222222222222221002"
    "5a140a106666666666666666666666666666666610031a330a10333333333333"
    "333333333333333333331004180320e80728b00930940a3a120a104444444444"
    "44444444444444444444441a1f0a105555555555555555555555555555555510"
    "01180420d00f28b4103098112001"
)


def artifact_recovery_snapshot() -> system_control_pb2.ArtifactRecoveryOperationsSnapshot:
    return system_control_pb2.ArtifactRecoveryOperationsSnapshot(
        schema=envelope_pb2.SchemaIdentity(
            name="nlos.sabi.SystemControl",
            major=1,
            minor=0,
        ),
        metrics=system_control_pb2.ArtifactRecoveryMetrics(
            worker_state=(
                system_control_pb2.RECOVERY_WORKER_LIFECYCLE_STATE_BACKING_OFF
            ),
            completed_cycles=17,
            total_inspected=23,
            total_finalized=11,
            consecutive_failed_cycles=2,
            retry_delay_ms=500,
            durable_retrying=4,
            durable_escalated=3,
            durable_unacknowledged_escalated=1,
            durable_resolved=9,
            last_failures=[
                system_control_pb2.RecoveryFailureSummary(
                    plan_id=bytes([0x11]) * 16,
                    authority=system_control_pb2.RECOVERY_FAILURE_AUTHORITY_TASK,
                ),
                system_control_pb2.RecoveryFailureSummary(
                    plan_id=bytes([0x22]) * 16,
                    authority=system_control_pb2.RECOVERY_FAILURE_AUTHORITY_ARTIFACT,
                ),
                system_control_pb2.RecoveryFailureSummary(
                    plan_id=bytes([0x66]) * 16,
                    authority=system_control_pb2.RECOVERY_FAILURE_AUTHORITY_COORDINATOR,
                ),
            ],
        ),
        alerts=[
            system_control_pb2.ArtifactRecoveryAlertStatus(
                plan_id=bytes([0x33]) * 16,
                total_failures=4,
                last_failure_authority=(
                    system_control_pb2.RECOVERY_FAILURE_AUTHORITY_COORDINATOR
                ),
                first_failed_at_ms=1000,
                last_failed_at_ms=1200,
                escalated_at_ms=1300,
                acknowledgement_receipt=envelope_pb2.ReceiptReference(
                    receipt_id=bytes([0x44]) * 16
                ),
            ),
            system_control_pb2.ArtifactRecoveryAlertStatus(
                plan_id=bytes([0x55]) * 16,
                total_failures=1,
                last_failure_authority=(
                    system_control_pb2.RECOVERY_FAILURE_AUTHORITY_WORKER
                ),
                first_failed_at_ms=2000,
                last_failed_at_ms=2100,
                escalated_at_ms=2200,
            ),
        ],
        alerts_truncated=True,
    )


artifact_snapshot = artifact_recovery_snapshot()
artifact_snapshot_golden = bytes.fromhex(ARTIFACT_RECOVERY_SNAPSHOT_GOLDEN_HEX)
assert artifact_snapshot.SerializeToString(deterministic=True) == artifact_snapshot_golden

decoded_artifact_snapshot = (
    system_control_pb2.ArtifactRecoveryOperationsSnapshot.FromString(
        artifact_snapshot_golden
    )
)
assert decoded_artifact_snapshot.schema.name == "nlos.sabi.SystemControl"
assert decoded_artifact_snapshot.metrics.worker_state == (
    system_control_pb2.RECOVERY_WORKER_LIFECYCLE_STATE_BACKING_OFF
)
assert decoded_artifact_snapshot.metrics.HasField("retry_delay_ms")
assert decoded_artifact_snapshot.metrics.retry_delay_ms == 500
assert [failure.authority for failure in decoded_artifact_snapshot.metrics.last_failures] == [
    system_control_pb2.RECOVERY_FAILURE_AUTHORITY_TASK,
    system_control_pb2.RECOVERY_FAILURE_AUTHORITY_ARTIFACT,
    system_control_pb2.RECOVERY_FAILURE_AUTHORITY_COORDINATOR,
]
assert decoded_artifact_snapshot.metrics.last_failures[2].plan_id == bytes([0x66]) * 16
assert decoded_artifact_snapshot.alerts[0].HasField("acknowledgement_receipt")
assert not decoded_artifact_snapshot.alerts[1].HasField("acknowledgement_receipt")
assert decoded_artifact_snapshot.alerts_truncated is True
assert (
    decoded_artifact_snapshot.SerializeToString(deterministic=True)
    == artifact_snapshot_golden
)

without_retry_delay = system_control_pb2.ArtifactRecoveryOperationsSnapshot()
without_retry_delay.CopyFrom(decoded_artifact_snapshot)
without_retry_delay.metrics.ClearField("retry_delay_ms")
assert not without_retry_delay.metrics.HasField("retry_delay_ms")
assert (
    without_retry_delay.SerializeToString(deterministic=True)
    != artifact_snapshot_golden
)


SEMANTIC_RECOVERY_SNAPSHOT_GOLDEN_HEX = (
    "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c10011801"
    "1210080d10061802200428033001380940011a330a10717171717171717171"
    "717171717171711008180520e80728f80a30dc0b3a120a1072727272727272"
    "7272727272727272721a1f0a10737373737373737373737373737373731008"
    "180320d00f28e01230c4132001"
)


def semantic_recovery_snapshot() -> system_control_pb2.SemanticRecoveryOperationsSnapshot:
    return system_control_pb2.SemanticRecoveryOperationsSnapshot(
        schema=envelope_pb2.SchemaIdentity(
            name="nlos.sabi.SystemControl",
            major=1,
            minor=1,
        ),
        metrics=system_control_pb2.SemanticRecoveryMetrics(
            total_inspected=13,
            total_finalized=6,
            consecutive_failed_cycles=2,
            durable_retrying=4,
            durable_escalated=3,
            durable_unacknowledged_escalated=1,
            durable_resolved=9,
            domain_faulted=True,
        ),
        alerts=[
            system_control_pb2.SemanticRecoveryAlertStatus(
                plan_id=bytes([0x71]) * 16,
                total_failures=8,
                last_failure_authority=(
                    system_control_pb2.RECOVERY_FAILURE_AUTHORITY_SEMANTIC
                ),
                first_failed_at_ms=1000,
                last_failed_at_ms=1400,
                escalated_at_ms=1500,
                acknowledgement_receipt=envelope_pb2.ReceiptReference(
                    receipt_id=bytes([0x72]) * 16
                ),
            ),
            system_control_pb2.SemanticRecoveryAlertStatus(
                plan_id=bytes([0x73]) * 16,
                total_failures=8,
                last_failure_authority=(
                    system_control_pb2.RECOVERY_FAILURE_AUTHORITY_COORDINATOR
                ),
                first_failed_at_ms=2000,
                last_failed_at_ms=2400,
                escalated_at_ms=2500,
            ),
        ],
        alerts_truncated=True,
    )


semantic_snapshot = semantic_recovery_snapshot()
semantic_snapshot_golden = bytes.fromhex(SEMANTIC_RECOVERY_SNAPSHOT_GOLDEN_HEX)
assert semantic_snapshot.SerializeToString(deterministic=True) == semantic_snapshot_golden

decoded_semantic_snapshot = (
    system_control_pb2.SemanticRecoveryOperationsSnapshot.FromString(
        semantic_snapshot_golden
    )
)
assert decoded_semantic_snapshot.schema.minor == 1
assert decoded_semantic_snapshot.metrics.total_inspected == 13
assert decoded_semantic_snapshot.metrics.domain_faulted is True
assert decoded_semantic_snapshot.metrics.durable_unacknowledged_escalated == 1
assert decoded_semantic_snapshot.alerts[0].last_failure_authority == (
    system_control_pb2.RECOVERY_FAILURE_AUTHORITY_SEMANTIC
)
assert decoded_semantic_snapshot.alerts[0].HasField("acknowledgement_receipt")
assert not decoded_semantic_snapshot.alerts[1].HasField("acknowledgement_receipt")
assert decoded_semantic_snapshot.alerts_truncated is True
assert (
    decoded_semantic_snapshot.SerializeToString(deterministic=True)
    == semantic_snapshot_golden
)


# ---------------------------------------------------------------------------
# Stage-B handover #13 (W34-A/W29-G deferred minor): pin the TS/Python
# conformance goldens for the SABI v1.2–v1.5 additive SystemControl surface
# against the Rust-derived golden bytes (crates/nlos-schema
# tests/compatibility.rs stays the byte source of truth).
#
# One documented divergence: prost emits ControlCommand fields in proto
# declaration order (oneof arm before `reason` field 8), while protobuf-es
# and the Python runtime emit field-number order (`reason` before arms 9+).
# Both are valid protobuf wire forms (field order is not significant), so
# each command golden is pinned twice: the literal prost-order bytes are the
# decode anchor (decode + re-encode must reach the canonical form), and the
# canonical TS/Python bytes are the encode anchor. The TS and Python
# canonical bytes are identical to each other.
# ---------------------------------------------------------------------------

assert system_control_pb2.SYSTEM_CONTROL_VIEW_RESOURCE_COMMIT_RECOVERY == 3
assert system_control_pb2.SYSTEM_CONTROL_VIEW_TASK_GROUP == 4
assert system_control_pb2.SYSTEM_CONTROL_VIEW_TASK_NODE == 5
assert system_control_pb2.SYSTEM_CONTROL_VIEW_EXECUTION_FIBER == 6
assert system_control_pb2.SYSTEM_CONTROL_VIEW_TOPIC == 7
assert system_control_pb2.SYSTEM_CONTROL_VIEW_OPERATION == 8
assert system_control_pb2.RECOVERY_FAILURE_AUTHORITY_RESOURCE == 6

# Shared hex pieces mirror the Rust constants verbatim: the v1.2 W28-D
# submit prefix (schema identity + ControlCommand addressing through the
# CAS expectation), the v1.3 W29-D head/body split (per-arm command length
# varies), and the shared reason string.
SUBMIT_PREFIX_V1_2_HEX = (
        "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180212670a10616161" +
    "61616161616161616161616161121032323232323232323232323232323232180320022a1081" +
    "8181818181818181818181818181813005"
)
SUBMIT_HEAD_V1_3_HEX =     "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180312"
SUBMIT_BODY_V1_3_HEX = (
        "0a10616161616161616161616161616161611210323232323232323232323232323232321803" +
    "20022a10818181818181818181818181818181813005"
)
SUBMIT_REASON_HEX = (
        "42276f70657261746f72207061757365732074686520657363616c61746564206f7065726174" +
    "696f6e"
)

COMMAND_ARM_CASES = [
    {
        "label": "pause",
        "minor": 2,
        "command_len_hex": "67",
        "arm_hex": "5a00",
        "arm_field": "pause_operation",
        "arm_message": system_control_pb2.PauseCommand(),
    },
    {
        "label": "resume",
        "minor": 2,
        "command_len_hex": "67",
        "arm_hex": "6200",
        "arm_field": "resume_operation",
        "arm_message": system_control_pb2.ResumeCommand(),
    },
    {
        "label": "cancel",
        "minor": 2,
        "command_len_hex": "67",
        "arm_hex": "6a00",
        "arm_field": "cancel_operation",
        "arm_message": system_control_pb2.CancelCommand(),
    },
    {
        "label": "kill",
        "minor": 3,
        "command_len_hex": "67",
        "arm_hex": "7200",
        "arm_field": "kill_operation",
        "arm_message": system_control_pb2.KillCommand(),
    },
    {
        "label": "throttle",
        "minor": 3,
        "command_len_hex": "69",
        "arm_hex": "7a020832",
        "arm_field": "throttle_operation",
        "arm_message": system_control_pb2.ThrottleCommand(throttle_percent=50),
    },
    {
        "label": "reclaim",
        "minor": 3,
        "command_len_hex": "68",
        "arm_hex": "820100",
        "arm_field": "reclaim_operation",
        "arm_message": system_control_pb2.ReclaimCommand(),
    },
]

for case in COMMAND_ARM_CASES:
    request = system_control_pb2.SubmitControlCommandRequest(
        schema=envelope_pb2.SchemaIdentity(
            name="nlos.sabi.SystemControl", major=1, minor=case["minor"]
        ),
        command=system_control_pb2.ControlCommand(
            control_command_id=bytes([0x61]) * 16,
            issuer_principal_id=bytes([0x32]) * 16,
            source=system_control_pb2.CONTROL_COMMAND_SOURCE_CLI,
            scope=system_control_pb2.CONTROL_SCOPE_OPERATION,
            target_id=bytes([0x81]) * 16,
            expected_generation_or_revision=5,
            reason="operator pauses the escalated operation",
            **{case["arm_field"]: case["arm_message"]},
        ),
    )
    prefix_hex = (
        SUBMIT_PREFIX_V1_2_HEX
        if case["minor"] == 2
        else SUBMIT_HEAD_V1_3_HEX + case["command_len_hex"] + SUBMIT_BODY_V1_3_HEX
    )
    canonical = bytes.fromhex(prefix_hex + SUBMIT_REASON_HEX + case["arm_hex"])
    prost_order = bytes.fromhex(prefix_hex + case["arm_hex"] + SUBMIT_REASON_HEX)

    assert request.SerializeToString(deterministic=True) == canonical, (
        f"{case['label']} arm must encode to the canonical Python bytes"
    )
    decoded = system_control_pb2.SubmitControlCommandRequest.FromString(prost_order)
    assert decoded.schema.minor == case["minor"]
    assert decoded.command.WhichOneof("command") == case["arm_field"], (
        f"{case['label']} oneof arm"
    )
    if case["arm_field"] == "throttle_operation":
        assert decoded.command.throttle_operation.throttle_percent == 50
    assert decoded.command.reason == "operator pauses the escalated operation"
    assert decoded.command.target_id == bytes([0x81]) * 16
    assert decoded.command.expected_generation_or_revision == 5
    assert decoded.SerializeToString(deterministic=True) == canonical, (
        f"{case['label']} prost-order golden must decode and re-encode canonically"
    )

# Divergence witness: the prost declaration-order bytes and the canonical
# TS/Python bytes are wire-equivalent but not identical. If a runtime
# upgrade ever makes these converge, this assert forces a conscious re-pin.
assert bytes.fromhex(
    SUBMIT_PREFIX_V1_2_HEX + "5a00" + SUBMIT_REASON_HEX
) != bytes.fromhex(SUBMIT_PREFIX_V1_2_HEX + SUBMIT_REASON_HEX + "5a00"), (
    "prost declaration order and Python canonical order must stay distinct bytes"
)

# W28-C-3b (v1.4) resource-domain recovery snapshot golden — byte-equal to
# the Rust RESOURCE_RECOVERY_SNAPSHOT_GOLDEN_HEX (no oneof; ascending field
# numbers, so declaration order and field-number order coincide).
RESOURCE_RECOVERY_SNAPSHOT_GOLDEN_HEX = (
        "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c100118041210080f100718" +
    "02200528043002380b40011a330a10818181818181818181818181818181811008180620e807" +
    "28f80a30dc0b3a120a10828282828282828282828282828282821a1f0a108383838383838383" +
    "83838383838383831009180320b81728c81a30ac1b2001"
)


def resource_recovery_snapshot() -> (
    system_control_pb2.ResourceRecoveryOperationsSnapshot
):
    return system_control_pb2.ResourceRecoveryOperationsSnapshot(
        schema=envelope_pb2.SchemaIdentity(
            name="nlos.sabi.SystemControl",
            major=1,
            minor=4,
        ),
        metrics=system_control_pb2.ResourceRecoveryMetrics(
            total_inspected=15,
            total_finalized=7,
            consecutive_failed_cycles=2,
            durable_retrying=5,
            durable_escalated=4,
            durable_unacknowledged_escalated=2,
            durable_resolved=11,
            domain_faulted=True,
        ),
        alerts=[
            system_control_pb2.ResourceRecoveryAlertStatus(
                plan_id=bytes([0x81]) * 16,
                total_failures=8,
                last_failure_authority=(
                    system_control_pb2.RECOVERY_FAILURE_AUTHORITY_RESOURCE
                ),
                first_failed_at_ms=1000,
                last_failed_at_ms=1400,
                escalated_at_ms=1500,
                acknowledgement_receipt=envelope_pb2.ReceiptReference(
                    receipt_id=bytes([0x82]) * 16
                ),
            ),
            system_control_pb2.ResourceRecoveryAlertStatus(
                plan_id=bytes([0x83]) * 16,
                total_failures=9,
                last_failure_authority=(
                    system_control_pb2.RECOVERY_FAILURE_AUTHORITY_COORDINATOR
                ),
                first_failed_at_ms=3000,
                last_failed_at_ms=3400,
                escalated_at_ms=3500,
            ),
        ],
        alerts_truncated=True,
    )


resource_snapshot = resource_recovery_snapshot()
resource_snapshot_golden = bytes.fromhex(RESOURCE_RECOVERY_SNAPSHOT_GOLDEN_HEX)
assert resource_snapshot.SerializeToString(deterministic=True) == (
    resource_snapshot_golden
)

decoded_resource_snapshot = (
    system_control_pb2.ResourceRecoveryOperationsSnapshot.FromString(
        resource_snapshot_golden
    )
)
assert decoded_resource_snapshot.schema.minor == 4
assert decoded_resource_snapshot.metrics.total_inspected == 15
assert decoded_resource_snapshot.metrics.durable_resolved == 11
assert decoded_resource_snapshot.metrics.domain_faulted is True
assert decoded_resource_snapshot.alerts[0].last_failure_authority == (
    system_control_pb2.RECOVERY_FAILURE_AUTHORITY_RESOURCE
)
assert decoded_resource_snapshot.alerts[0].HasField("acknowledgement_receipt")
assert not decoded_resource_snapshot.alerts[1].HasField("acknowledgement_receipt")
assert decoded_resource_snapshot.alerts_truncated is True
assert (
    decoded_resource_snapshot.SerializeToString(deterministic=True)
    == resource_snapshot_golden
)

# Recovery-domain acknowledge/resume command submits round-trip with the
# oneof arm addressed (mirroring the Rust fixtures; the Rust lane pins no
# byte golden for these, so wire-level round-trip + decode is the pinned
# surface here). The schema identity stays at each arm's freeze-point minor.
RECOVERY_COMMAND_CASES = [
    {
        "label": "acknowledge_semantic",
        "minor": 1,
        "control_id": 0x51,
        "target_byte": 0x71,
        "arm_field": "acknowledge_semantic_recovery_alert",
        "arm_message": system_control_pb2.AcknowledgeSemanticRecoveryAlertCommand(),
        "reason": "operator inspected durable semantic recovery state",
    },
    {
        "label": "resume_semantic",
        "minor": 1,
        "control_id": 0x52,
        "target_byte": 0x71,
        "arm_field": "resume_semantic_recovery",
        "arm_message": system_control_pb2.ResumeSemanticRecoveryCommand(),
        "reason": "operator resumes the escalated semantic plan",
    },
    {
        "label": "acknowledge_resource",
        "minor": 4,
        "control_id": 0x55,
        "target_byte": 0x81,
        "arm_field": "acknowledge_resource_recovery_alert",
        "arm_message": system_control_pb2.AcknowledgeResourceRecoveryAlertCommand(),
        "reason": "operator inspected durable resource recovery state",
    },
    {
        "label": "resume_resource",
        "minor": 4,
        "control_id": 0x56,
        "target_byte": 0x81,
        "arm_field": "resume_resource_recovery",
        "arm_message": system_control_pb2.ResumeResourceRecoveryCommand(),
        "reason": "operator resumes the escalated resource plan",
    },
]

for case in RECOVERY_COMMAND_CASES:
    request = system_control_pb2.SubmitControlCommandRequest(
        schema=envelope_pb2.SchemaIdentity(
            name="nlos.sabi.SystemControl", major=1, minor=case["minor"]
        ),
        command=system_control_pb2.ControlCommand(
            control_command_id=bytes([case["control_id"]]) * 16,
            issuer_principal_id=bytes([0x32]) * 16,
            source=system_control_pb2.CONTROL_COMMAND_SOURCE_CLI,
            scope=system_control_pb2.CONTROL_SCOPE_OPERATION,
            target_id=bytes([case["target_byte"]]) * 16,
            expected_generation_or_revision=8,
            reason=case["reason"],
            **{case["arm_field"]: case["arm_message"]},
        ),
    )
    wire = request.SerializeToString(deterministic=True)
    decoded = system_control_pb2.SubmitControlCommandRequest.FromString(wire)
    assert decoded.command.WhichOneof("command") == case["arm_field"], (
        f"{case['label']} oneof arm"
    )
    assert decoded.command.reason == case["reason"]
    assert decoded.command.target_id == bytes([case["target_byte"]]) * 16
    assert decoded.SerializeToString(deterministic=True) == wire, (
        f"{case['label']} submit must round-trip byte-identically"
    )

# W32-G (v1.5) per-layer inspect requests: the additive addressing fields
# (target_id / plan_id / target_generation) must survive the wire (the Rust
# lane enforces the fail-closed addressing rules; TS/Python pin the wire
# round-trip). The identity stays pinned at the v1.5 freeze point.
def layer_get(view, target_id, plan_id, target_generation):
    return system_control_pb2.GetSystemControlRequest(
        schema=envelope_pb2.SchemaIdentity(
            name="nlos.sabi.SystemControl", major=1, minor=5
        ),
        view=view,
        alert_limit=8,
        target_id=target_id,
        plan_id=plan_id,
        target_generation=target_generation,
    )


LAYER_VIEW_CASES = [
    {
        "label": "task_group",
        "view": system_control_pb2.SYSTEM_CONTROL_VIEW_TASK_GROUP,
        "target_byte": 0x91,
        "plan_id": b"",
        "target_generation": 0,
    },
    {
        "label": "task_node",
        "view": system_control_pb2.SYSTEM_CONTROL_VIEW_TASK_NODE,
        "target_byte": 0xA2,
        "plan_id": bytes([0xA1]) * 16,
        "target_generation": 0,
    },
    {
        "label": "execution_fiber",
        "view": system_control_pb2.SYSTEM_CONTROL_VIEW_EXECUTION_FIBER,
        "target_byte": 0xB1,
        "plan_id": b"",
        "target_generation": 2,
    },
    {
        "label": "topic",
        "view": system_control_pb2.SYSTEM_CONTROL_VIEW_TOPIC,
        "target_byte": 0xC1,
        "plan_id": b"",
        "target_generation": 0,
    },
    {
        "label": "operation",
        "view": system_control_pb2.SYSTEM_CONTROL_VIEW_OPERATION,
        "target_byte": 0xD1,
        "plan_id": b"",
        "target_generation": 1,
    },
]

for case in LAYER_VIEW_CASES:
    request = layer_get(
        case["view"],
        bytes([case["target_byte"]]) * 16,
        case["plan_id"],
        case["target_generation"],
    )
    wire = request.SerializeToString(deterministic=True)
    decoded = system_control_pb2.GetSystemControlRequest.FromString(wire)
    assert decoded.view == case["view"], f"{case['label']} view"
    assert decoded.target_id == bytes([case["target_byte"]]) * 16, (
        f"{case['label']} target_id"
    )
    assert decoded.plan_id == case["plan_id"], f"{case['label']} plan_id"
    assert decoded.target_generation == case["target_generation"], (
        f"{case['label']} target_generation"
    )
    assert decoded.SerializeToString(deterministic=True) == wire, (
        f"{case['label']} get request must round-trip byte-identically"
    )

for view in (
    system_control_pb2.SYSTEM_CONTROL_VIEW_ARTIFACT_COMMIT_RECOVERY,
    system_control_pb2.SYSTEM_CONTROL_VIEW_SEMANTIC_COMMIT_RECOVERY,
    system_control_pb2.SYSTEM_CONTROL_VIEW_RESOURCE_COMMIT_RECOVERY,
):
    recovery_get = layer_get(view, b"", b"", 0)
    wire = recovery_get.SerializeToString(deterministic=True)
    decoded = system_control_pb2.GetSystemControlRequest.FromString(wire)
    assert decoded.view == view
    assert len(decoded.target_id) == 0, "recovery views carry no target"
    assert len(decoded.plan_id) == 0
    assert decoded.target_generation == 0


# W32-G per-layer snapshot goldens — byte-equal to the Rust
# w32g_layer_snapshots_pin_the_deterministic_golden_bytes vectors.
def layer_identity() -> envelope_pb2.SchemaIdentity:
    return envelope_pb2.SchemaIdentity(
        name="nlos.sabi.SystemControl",
        major=1,
        minor=5,
    )


def task_group_snapshot() -> system_control_pb2.TaskGroupOperationsSnapshot:
    return system_control_pb2.TaskGroupOperationsSnapshot(
        schema=layer_identity(),
        group=system_control_pb2.TaskGroupStatus(
            group_id=bytes([0x91]) * 16,
            task_id=bytes([0x92]) * 16,
            state=system_control_pb2.TASK_GROUP_LIFECYCLE_STATE_OPEN,
            membership_generation=3,
            state_seq=1,
            created_at_ms=1000,
            updated_at_ms=1500,
        ),
        members=[
            system_control_pb2.TaskGroupMemberStatus(
                member_type=system_control_pb2.TASK_GROUP_MEMBER_TYPE_TASK_ATTEMPT,
                member_id=bytes([0x93]) * 16,
                membership_state=system_control_pb2.TASK_GROUP_MEMBERSHIP_STATE_ACTIVE,
                membership_generation=1,
                admission_receipt=envelope_pb2.ReceiptReference(
                    receipt_id=bytes([0x94]) * 16
                ),
            ),
            system_control_pb2.TaskGroupMemberStatus(
                member_type=system_control_pb2.TASK_GROUP_MEMBER_TYPE_CHILD_GROUP,
                member_id=bytes([0x95]) * 16,
                membership_state=system_control_pb2.TASK_GROUP_MEMBERSHIP_STATE_REMOVED,
                membership_generation=2,
                admission_receipt=envelope_pb2.ReceiptReference(
                    receipt_id=bytes([0x96]) * 16
                ),
                removal_receipt=envelope_pb2.ReceiptReference(
                    receipt_id=bytes([0x97]) * 16
                ),
            ),
        ],
    )


def task_node_snapshot() -> system_control_pb2.TaskNodeOperationsSnapshot:
    return system_control_pb2.TaskNodeOperationsSnapshot(
        schema=layer_identity(),
        node=system_control_pb2.TaskNodeStatus(
            plan_id=bytes([0xA1]) * 16,
            node_id=bytes([0xA2]) * 16,
            kind=system_control_pb2.PLAN_NODE_KIND_EXECUTABLE,
            state=system_control_pb2.PLAN_NODE_LIFECYCLE_STATE_ELIGIBLE,
            declared_revision=4,
            node_digest=bytes([0xA3]) * 32,
            transition_count=2,
            residency_tier=system_control_pb2.CONTEXT_RESIDENCY_TIER_METADATA_ONLY,
            first_declared_at_ms=2000,
            updated_at_ms=2400,
        ),
    )


def execution_fiber_snapshot() -> (
    system_control_pb2.ExecutionFiberOperationsSnapshot
):
    return system_control_pb2.ExecutionFiberOperationsSnapshot(
        schema=layer_identity(),
        fiber=system_control_pb2.ExecutionFiberStatus(
            fiber_id=bytes([0xB1]) * 16,
            generation=2,
            state=system_control_pb2.EXECUTION_FIBER_LIFECYCLE_STATE_RUNNING,
            lifecycle_phase=system_control_pb2.EXECUTION_FIBER_PHASE_WAITING_EXTERNAL,
            active_cpu_ms=11,
            elapsed_wall_ms=40,
            scheduler_wait_ms=3,
            external_wait_ms=20,
            backpressure_wait_ms=1,
        ),
    )


def topic_snapshot() -> system_control_pb2.TopicOperationsSnapshot:
    return system_control_pb2.TopicOperationsSnapshot(
        schema=layer_identity(),
        topic=system_control_pb2.TopicStatus(
            topic_id=bytes([0xC1]) * 16,
            channel_id=bytes([0xC2]) * 16,
            channel_generation=5,
            name=b"stage-b/inspect",
            active_subscriptions=2,
            policy_digest=bytes([0xC3]) * 32,
            created_at_ms=3000,
        ),
    )


def durable_operation_snapshot() -> system_control_pb2.DurableOperationSnapshot:
    return system_control_pb2.DurableOperationSnapshot(
        schema=layer_identity(),
        operation=system_control_pb2.DurableOperationStatus(
            operation_id=bytes([0xD1]) * 16,
            generation=1,
            state=system_control_pb2.DURABLE_OPERATION_STATE_DISPATCHED,
            owner_fiber_id=bytes([0xB1]) * 16,
            owner_fiber_generation=2,
        ),
    )


TASK_GROUP_SNAPSHOT_GOLDEN_HEX = (
        "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180512300a10919191" +
    "9191919191919191919191919112109292929292929292929292929292929220012803300148" +
    "e80750dc0b1a2c0802121093939393939393939393939393939393180120012a120a10949494" +
    "949494949494949494949494941a400801121095959595959595959595959595959595180220" +
    "022a120a109696969696969696969696969696969632120a1097979797979797979797979797" +
    "979797"
)
TASK_NODE_SNAPSHOT_GOLDEN_HEX = (
        "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180512560a10a1a1a1" +
    "a1a1a1a1a1a1a1a1a1a1a1a1a11210a2a2a2a2a2a2a2a2a2a2a2a2a2a2a2a218022003280432" +
    "20a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a3a33802400150" +
    "d00f58e012"
)
EXECUTION_FIBER_SNAPSHOT_GOLDEN_HEX = (
        "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180512220a10b1b1b1" +
    "b1b1b1b1b1b1b1b1b1b1b1b1b1100218032002280b3028380340144801"
)
TOPIC_SNAPSHOT_GOLDEN_HEX = (
        "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c10011805125e0a10c1c1c1" +
    "c1c1c1c1c1c1c1c1c1c1c1c1c11210c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c21805220f737461" +
    "67652d622f696e737065637428023220c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3" +
    "c3c3c3c3c3c3c3c3c3c338b817"
)
DURABLE_OPERATION_SNAPSHOT_GOLDEN_HEX = (
        "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c10011805122a0a10d1d1d1" +
    "d1d1d1d1d1d1d1d1d1d1d1d1d1100118022a10b1b1b1b1b1b1b1b1b1b1b1b1b1b1b1b13002"
)

layer_snapshot_messages = {
    "task_group": task_group_snapshot(),
    "task_node": task_node_snapshot(),
    "execution_fiber": execution_fiber_snapshot(),
    "topic": topic_snapshot(),
    "operation": durable_operation_snapshot(),
}
layer_snapshot_goldens = {
    "task_group": TASK_GROUP_SNAPSHOT_GOLDEN_HEX,
    "task_node": TASK_NODE_SNAPSHOT_GOLDEN_HEX,
    "execution_fiber": EXECUTION_FIBER_SNAPSHOT_GOLDEN_HEX,
    "topic": TOPIC_SNAPSHOT_GOLDEN_HEX,
    "operation": DURABLE_OPERATION_SNAPSHOT_GOLDEN_HEX,
}
for label, message in layer_snapshot_messages.items():
    golden = bytes.fromhex(layer_snapshot_goldens[label])
    assert message.SerializeToString(deterministic=True) == golden, (
        f"{label} snapshot golden bytes"
    )

decoded_task_group = system_control_pb2.TaskGroupOperationsSnapshot.FromString(
    bytes.fromhex(TASK_GROUP_SNAPSHOT_GOLDEN_HEX)
)
assert decoded_task_group.group.state == (
    system_control_pb2.TASK_GROUP_LIFECYCLE_STATE_OPEN
)
assert decoded_task_group.group.membership_generation == 3
assert [member.member_type for member in decoded_task_group.members] == [
    system_control_pb2.TASK_GROUP_MEMBER_TYPE_TASK_ATTEMPT,
    system_control_pb2.TASK_GROUP_MEMBER_TYPE_CHILD_GROUP,
]
assert decoded_task_group.members[0].HasField("admission_receipt")
assert not decoded_task_group.members[0].HasField("removal_receipt")
assert decoded_task_group.members[1].HasField("removal_receipt")
assert decoded_task_group.members_truncated is False
assert (
    decoded_task_group.SerializeToString(deterministic=True)
    == bytes.fromhex(TASK_GROUP_SNAPSHOT_GOLDEN_HEX)
)

decoded_task_node = system_control_pb2.TaskNodeOperationsSnapshot.FromString(
    bytes.fromhex(TASK_NODE_SNAPSHOT_GOLDEN_HEX)
)
assert decoded_task_node.node.kind == system_control_pb2.PLAN_NODE_KIND_EXECUTABLE
assert decoded_task_node.node.state == (
    system_control_pb2.PLAN_NODE_LIFECYCLE_STATE_ELIGIBLE
)
assert decoded_task_node.node.node_digest == bytes([0xA3]) * 32
assert decoded_task_node.node.residency_tier == (
    system_control_pb2.CONTEXT_RESIDENCY_TIER_METADATA_ONLY
)
assert (
    decoded_task_node.SerializeToString(deterministic=True)
    == bytes.fromhex(TASK_NODE_SNAPSHOT_GOLDEN_HEX)
)

decoded_fiber = system_control_pb2.ExecutionFiberOperationsSnapshot.FromString(
    bytes.fromhex(EXECUTION_FIBER_SNAPSHOT_GOLDEN_HEX)
)
assert decoded_fiber.fiber.state == (
    system_control_pb2.EXECUTION_FIBER_LIFECYCLE_STATE_RUNNING
)
assert decoded_fiber.fiber.lifecycle_phase == (
    system_control_pb2.EXECUTION_FIBER_PHASE_WAITING_EXTERNAL
)
assert decoded_fiber.fiber.generation == 2
assert decoded_fiber.fiber.external_wait_ms == 20
assert (
    decoded_fiber.SerializeToString(deterministic=True)
    == bytes.fromhex(EXECUTION_FIBER_SNAPSHOT_GOLDEN_HEX)
)

decoded_topic = system_control_pb2.TopicOperationsSnapshot.FromString(
    bytes.fromhex(TOPIC_SNAPSHOT_GOLDEN_HEX)
)
assert decoded_topic.topic.name == b"stage-b/inspect"
assert decoded_topic.topic.channel_generation == 5
assert decoded_topic.topic.policy_digest == bytes([0xC3]) * 32
assert (
    decoded_topic.SerializeToString(deterministic=True)
    == bytes.fromhex(TOPIC_SNAPSHOT_GOLDEN_HEX)
)

decoded_operation = system_control_pb2.DurableOperationSnapshot.FromString(
    bytes.fromhex(DURABLE_OPERATION_SNAPSHOT_GOLDEN_HEX)
)
assert decoded_operation.operation.state == (
    system_control_pb2.DURABLE_OPERATION_STATE_DISPATCHED
)
assert decoded_operation.operation.generation == 1
assert decoded_operation.operation.owner_fiber_id == bytes([0xB1]) * 16
assert not decoded_operation.operation.HasField("outcome_receipt")
assert (
    decoded_operation.SerializeToString(deterministic=True)
    == bytes.fromhex(DURABLE_OPERATION_SNAPSHOT_GOLDEN_HEX)
)
