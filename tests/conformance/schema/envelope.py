from pathlib import Path
import sys

from google.protobuf.message import DecodeError

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "gen" / "python"))

from nlos.sabi.v1 import envelope_pb2, service_directory_pb2  # noqa: E402
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
