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
