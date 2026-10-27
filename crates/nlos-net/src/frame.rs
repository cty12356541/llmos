//! 4-byte big-endian length-prefixed multiplexed wire framing separating
//! high-priority `ControlRpc` envelopes from `<= 64 KiB` `BulkChunk` data
//! streams (`ADR-0026` §3.1).

use std::error::Error;
use std::fmt;
use std::io;
use std::time::Duration;

use nlos_cell::CellEpoch;
use nlos_types::IdempotencyKey;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::failure::{SocketFailureKind, WriteProgressPhase};

/// Normative upper bound for a single `BulkChunk` payload (`64 KiB`,
/// `ADR-0026` §3.1).
pub const MAX_BULK_CHUNK_BYTES: usize = 64 * 1024;

/// Default maximum `ControlRpc` payload size (`256 KiB`).
pub const DEFAULT_MAX_CONTROL_PAYLOAD_BYTES: usize = 256 * 1024;

/// Default per-stream bulk flow-control window (`256 KiB` = `4 * 64 KiB`
/// chunks), guaranteeing bounded memory under slow consumers.
pub const DEFAULT_BULK_WINDOW_BYTES: usize = 256 * 1024;

const MUX_PROTOCOL_VERSION: u8 = 1;
const LENGTH_PREFIX_BYTES: usize = 4;
const MAX_MUX_WIRE_FRAME_BYTES: usize = 512 * 1024;

const KIND_CONTROL_RPC_REQ: u8 = 0x01;
const KIND_CONTROL_RPC_RESP: u8 = 0x02;
const KIND_BULK_OPEN: u8 = 0x10;
const KIND_BULK_CHUNK: u8 = 0x11;
const KIND_BULK_WINDOW_CREDIT: u8 = 0x12;
const KIND_BULK_FINISH: u8 = 0x13;
const KIND_BULK_COMPLETE_ACK: u8 = 0x14;

/// Priority class governing multiplexer transmit scheduling (`ADR-0026` §3.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FramePriorityClass {
    /// High-priority control/RPC and credit-grant frames; always scheduled
    /// ahead of queued bulk data chunks.
    HighControl,
    /// Low-priority `<= 64 KiB` bulk stream frames subject to window credits.
    BulkData,
}

/// Wire status code carried on a `ControlRpcResponse`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RpcWireStatus {
    /// Peer executed (or idempotently replayed) the request and returned payload.
    Ok = 0,
    /// Peer cleanly rejected the request before executing any side effect
    /// (e.g. stale fence admission reject before dispatch).
    PreExecutionUnavailable = 1,
}

/// Multiplexed wire frame transmitted inside a 4-byte big-endian length prefix.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MuxFrame {
    /// High-priority RPC request frame.
    ControlRpcRequest {
        /// Multiplexed logical request stream id.
        stream_id: u64,
        /// Stable 16-byte `RemoteCallContract.call_id`.
        call_id: [u8; 16],
        /// Mandatory `IdempotencyKey` preserved across retries.
        idempotency_key: IdempotencyKey,
        /// Caller logical deadline tick.
        deadline_tick: u64,
        /// Whether this call mutates remote state.
        side_effecting: bool,
        /// Opaque request payload (`<= max_control_payload_bytes`).
        payload: Vec<u8>,
    },
    /// High-priority RPC response frame.
    ControlRpcResponse {
        /// Matching request `stream_id`.
        stream_id: u64,
        /// Authoritative `CellEpoch` observed by the responder.
        observed_epoch: CellEpoch,
        /// Whether the responder served an idempotent replay.
        idempotency_replayed: bool,
        /// Execution status.
        status: RpcWireStatus,
        /// Opaque response payload.
        payload: Vec<u8>,
    },
    /// Opens a bounded-window bulk transfer stream.
    BulkStreamOpen {
        /// Multiplexed bulk stream id.
        stream_id: u64,
        /// Logical call id.
        call_id: [u8; 16],
        /// Mandatory `IdempotencyKey`.
        idempotency_key: IdempotencyKey,
        /// Declared total byte count across all chunks.
        total_bytes: u64,
    },
    /// One `<= 64 KiB` chunk of a bulk transfer stream.
    BulkChunk {
        /// Multiplexed bulk stream id.
        stream_id: u64,
        /// Zero-based sequential chunk index.
        chunk_seq: u32,
        /// Chunk payload (`1..=MAX_BULK_CHUNK_BYTES`).
        data: Vec<u8>,
    },
    /// Receiver-issued flow-control credit replenishment once buffered bytes
    /// are drained by the application consumer.
    BulkWindowCredit {
        /// Multiplexed bulk stream id.
        stream_id: u64,
        /// Byte credit replenished (`> 0`).
        granted_bytes: u32,
    },
    /// Marks completion of a bulk stream and commits its end-to-end `SHA-256`.
    BulkStreamFinish {
        /// Multiplexed bulk stream id.
        stream_id: u64,
        /// Total chunk count transmitted.
        total_chunks: u32,
        /// Total byte count transmitted.
        total_bytes: u64,
        /// End-to-end `SHA-256` digest over all chunk bytes in order.
        sha256_digest: [u8; 32],
    },
    /// Responder acknowledgement after verifying all chunks and `SHA-256` digest.
    BulkStreamCompleteAck {
        /// Multiplexed bulk stream id.
        stream_id: u64,
        /// Responder's authoritative `CellEpoch`.
        observed_epoch: CellEpoch,
        /// Total verified bytes received.
        verified_bytes: u64,
        /// Verified end-to-end `SHA-256` digest.
        verified_sha256: [u8; 32],
        /// Whether this bulk transfer was idempotently replayed.
        idempotency_replayed: bool,
    },
}

impl MuxFrame {
    /// Scheduling priority class of this frame.
    #[must_use]
    pub const fn priority_class(&self) -> FramePriorityClass {
        match self {
            Self::ControlRpcRequest { .. }
            | Self::ControlRpcResponse { .. }
            | Self::BulkWindowCredit { .. }
            | Self::BulkStreamCompleteAck { .. } => FramePriorityClass::HighControl,
            Self::BulkStreamOpen { .. }
            | Self::BulkChunk { .. }
            | Self::BulkStreamFinish { .. } => FramePriorityClass::BulkData,
        }
    }

    /// Encodes `self` into the inner frame body (without the 4B outer prefix),
    /// enforcing `MAX_BULK_CHUNK_BYTES` (`64 KiB`) and control payload bounds.
    ///
    /// # Errors
    ///
    /// Fails closed on oversized `BulkChunk` (`> 64 KiB`), empty chunk, zero
    /// credit grant, or oversized control payload.
    #[allow(clippy::missing_panics_doc, clippy::too_many_lines)]
    pub fn encode_body(
        &self,
        max_control_payload_bytes: usize,
    ) -> Result<Vec<u8>, FrameCodecError> {
        match self {
            Self::ControlRpcRequest {
                stream_id,
                call_id,
                idempotency_key,
                deadline_tick,
                side_effecting,
                payload,
            } => {
                if payload.len() > max_control_payload_bytes {
                    return Err(FrameCodecError::ControlPayloadTooLarge {
                        actual: payload.len(),
                        maximum: max_control_payload_bytes,
                    });
                }
                let mut out = Vec::with_capacity(2 + 8 + 16 + 16 + 8 + 1 + 4 + payload.len());
                out.push(MUX_PROTOCOL_VERSION);
                out.push(KIND_CONTROL_RPC_REQ);
                out.extend_from_slice(&stream_id.to_be_bytes());
                out.extend_from_slice(call_id);
                out.extend_from_slice(idempotency_key.as_bytes());
                out.extend_from_slice(&deadline_tick.to_be_bytes());
                out.push(u8::from(*side_effecting));
                out.extend_from_slice(
                    &u32::try_from(payload.len())
                        .expect("validated <= max")
                        .to_be_bytes(),
                );
                out.extend_from_slice(payload);
                Ok(out)
            }
            Self::ControlRpcResponse {
                stream_id,
                observed_epoch,
                idempotency_replayed,
                status,
                payload,
            } => {
                if payload.len() > max_control_payload_bytes {
                    return Err(FrameCodecError::ControlPayloadTooLarge {
                        actual: payload.len(),
                        maximum: max_control_payload_bytes,
                    });
                }
                let mut out = Vec::with_capacity(2 + 8 + 8 + 1 + 1 + 4 + payload.len());
                out.push(MUX_PROTOCOL_VERSION);
                out.push(KIND_CONTROL_RPC_RESP);
                out.extend_from_slice(&stream_id.to_be_bytes());
                out.extend_from_slice(&observed_epoch.get().to_be_bytes());
                out.push(u8::from(*idempotency_replayed));
                out.push(*status as u8);
                out.extend_from_slice(
                    &u32::try_from(payload.len())
                        .expect("validated <= max")
                        .to_be_bytes(),
                );
                out.extend_from_slice(payload);
                Ok(out)
            }
            Self::BulkStreamOpen {
                stream_id,
                call_id,
                idempotency_key,
                total_bytes,
            } => {
                let mut out = Vec::with_capacity(2 + 8 + 16 + 16 + 8);
                out.push(MUX_PROTOCOL_VERSION);
                out.push(KIND_BULK_OPEN);
                out.extend_from_slice(&stream_id.to_be_bytes());
                out.extend_from_slice(call_id);
                out.extend_from_slice(idempotency_key.as_bytes());
                out.extend_from_slice(&total_bytes.to_be_bytes());
                Ok(out)
            }
            Self::BulkChunk {
                stream_id,
                chunk_seq,
                data,
            } => {
                if data.is_empty() || data.len() > MAX_BULK_CHUNK_BYTES {
                    return Err(FrameCodecError::BulkChunkOutOfBounds {
                        actual: data.len(),
                        maximum: MAX_BULK_CHUNK_BYTES,
                    });
                }
                let mut out = Vec::with_capacity(2 + 8 + 4 + 4 + data.len());
                out.push(MUX_PROTOCOL_VERSION);
                out.push(KIND_BULK_CHUNK);
                out.extend_from_slice(&stream_id.to_be_bytes());
                out.extend_from_slice(&chunk_seq.to_be_bytes());
                out.extend_from_slice(&u32::try_from(data.len()).expect("<= 64 KiB").to_be_bytes());
                out.extend_from_slice(data);
                Ok(out)
            }
            Self::BulkWindowCredit {
                stream_id,
                granted_bytes,
            } => {
                if *granted_bytes == 0 {
                    return Err(FrameCodecError::MalformedFrame("zero BulkWindowCredit"));
                }
                let mut out = Vec::with_capacity(2 + 8 + 4);
                out.push(MUX_PROTOCOL_VERSION);
                out.push(KIND_BULK_WINDOW_CREDIT);
                out.extend_from_slice(&stream_id.to_be_bytes());
                out.extend_from_slice(&granted_bytes.to_be_bytes());
                Ok(out)
            }
            Self::BulkStreamFinish {
                stream_id,
                total_chunks,
                total_bytes,
                sha256_digest,
            } => {
                let mut out = Vec::with_capacity(2 + 8 + 4 + 8 + 32);
                out.push(MUX_PROTOCOL_VERSION);
                out.push(KIND_BULK_FINISH);
                out.extend_from_slice(&stream_id.to_be_bytes());
                out.extend_from_slice(&total_chunks.to_be_bytes());
                out.extend_from_slice(&total_bytes.to_be_bytes());
                out.extend_from_slice(sha256_digest);
                Ok(out)
            }
            Self::BulkStreamCompleteAck {
                stream_id,
                observed_epoch,
                verified_bytes,
                verified_sha256,
                idempotency_replayed,
            } => {
                let mut out = Vec::with_capacity(2 + 8 + 8 + 8 + 32 + 1);
                out.push(MUX_PROTOCOL_VERSION);
                out.push(KIND_BULK_COMPLETE_ACK);
                out.extend_from_slice(&stream_id.to_be_bytes());
                out.extend_from_slice(&observed_epoch.get().to_be_bytes());
                out.extend_from_slice(&verified_bytes.to_be_bytes());
                out.extend_from_slice(verified_sha256);
                out.push(u8::from(*idempotency_replayed));
                Ok(out)
            }
        }
    }

    /// Decodes an inner frame body (after stripping the 4-byte outer length).
    ///
    /// # Errors
    ///
    /// Fails closed on unknown version/tag, length mismatch, or `BulkChunk > 64 KiB`.
    #[allow(clippy::missing_panics_doc, clippy::too_many_lines)]
    pub fn decode_body(
        wire: &[u8],
        max_control_payload_bytes: usize,
    ) -> Result<Self, FrameCodecError> {
        if wire.len() < 2 {
            return Err(FrameCodecError::MalformedFrame(
                "frame shorter than 2B header",
            ));
        }
        if wire[0] != MUX_PROTOCOL_VERSION {
            return Err(FrameCodecError::UnsupportedVersion(wire[0]));
        }
        let kind = wire[1];
        let body = &wire[2..];

        match kind {
            KIND_CONTROL_RPC_REQ => {
                if body.len() < 8 + 16 + 16 + 8 + 1 + 4 {
                    return Err(FrameCodecError::MalformedFrame(
                        "truncated ControlRpcRequest",
                    ));
                }
                let stream_id = u64::from_be_bytes(body[0..8].try_into().expect("8B"));
                let call_id: [u8; 16] = body[8..24].try_into().expect("16B");
                let idem_bytes: [u8; 16] = body[24..40].try_into().expect("16B");
                let deadline_tick = u64::from_be_bytes(body[40..48].try_into().expect("8B"));
                let side_effecting = match body[48] {
                    0 => false,
                    1 => true,
                    _ => return Err(FrameCodecError::MalformedFrame("invalid bool byte")),
                };
                let payload_len = u32::from_be_bytes(body[49..53].try_into().expect("4B")) as usize;
                if payload_len > max_control_payload_bytes {
                    return Err(FrameCodecError::ControlPayloadTooLarge {
                        actual: payload_len,
                        maximum: max_control_payload_bytes,
                    });
                }
                if body.len() != 53 + payload_len {
                    return Err(FrameCodecError::MalformedFrame(
                        "ControlRpcRequest len mismatch",
                    ));
                }
                Ok(Self::ControlRpcRequest {
                    stream_id,
                    call_id,
                    idempotency_key: IdempotencyKey::from_bytes(idem_bytes),
                    deadline_tick,
                    side_effecting,
                    payload: body[53..].to_vec(),
                })
            }
            KIND_CONTROL_RPC_RESP => {
                if body.len() < 8 + 8 + 1 + 1 + 4 {
                    return Err(FrameCodecError::MalformedFrame(
                        "truncated ControlRpcResponse",
                    ));
                }
                let stream_id = u64::from_be_bytes(body[0..8].try_into().expect("8B"));
                let epoch_raw = u64::from_be_bytes(body[8..16].try_into().expect("8B"));
                let observed_epoch = CellEpoch::from_u64(epoch_raw)
                    .ok_or(FrameCodecError::MalformedFrame("zero observed_epoch"))?;
                let idempotency_replayed = match body[16] {
                    0 => false,
                    1 => true,
                    _ => return Err(FrameCodecError::MalformedFrame("invalid replay bool")),
                };
                let status = match body[17] {
                    0 => RpcWireStatus::Ok,
                    1 => RpcWireStatus::PreExecutionUnavailable,
                    _ => return Err(FrameCodecError::MalformedFrame("unknown RpcWireStatus")),
                };
                let payload_len = u32::from_be_bytes(body[18..22].try_into().expect("4B")) as usize;
                if payload_len > max_control_payload_bytes {
                    return Err(FrameCodecError::ControlPayloadTooLarge {
                        actual: payload_len,
                        maximum: max_control_payload_bytes,
                    });
                }
                if body.len() != 22 + payload_len {
                    return Err(FrameCodecError::MalformedFrame(
                        "ControlRpcResponse len mismatch",
                    ));
                }
                Ok(Self::ControlRpcResponse {
                    stream_id,
                    observed_epoch,
                    idempotency_replayed,
                    status,
                    payload: body[22..].to_vec(),
                })
            }
            KIND_BULK_OPEN => {
                if body.len() != 8 + 16 + 16 + 8 {
                    return Err(FrameCodecError::MalformedFrame("BulkStreamOpen len != 48"));
                }
                let stream_id = u64::from_be_bytes(body[0..8].try_into().expect("8B"));
                let call_id: [u8; 16] = body[8..24].try_into().expect("16B");
                let idem: [u8; 16] = body[24..40].try_into().expect("16B");
                let total_bytes = u64::from_be_bytes(body[40..48].try_into().expect("8B"));
                Ok(Self::BulkStreamOpen {
                    stream_id,
                    call_id,
                    idempotency_key: IdempotencyKey::from_bytes(idem),
                    total_bytes,
                })
            }
            KIND_BULK_CHUNK => {
                if body.len() < 8 + 4 + 4 {
                    return Err(FrameCodecError::MalformedFrame("truncated BulkChunk"));
                }
                let stream_id = u64::from_be_bytes(body[0..8].try_into().expect("8B"));
                let chunk_seq = u32::from_be_bytes(body[8..12].try_into().expect("4B"));
                let chunk_len = u32::from_be_bytes(body[12..16].try_into().expect("4B")) as usize;
                if chunk_len == 0 || chunk_len > MAX_BULK_CHUNK_BYTES {
                    return Err(FrameCodecError::BulkChunkOutOfBounds {
                        actual: chunk_len,
                        maximum: MAX_BULK_CHUNK_BYTES,
                    });
                }
                if body.len() != 16 + chunk_len {
                    return Err(FrameCodecError::MalformedFrame("BulkChunk len mismatch"));
                }
                Ok(Self::BulkChunk {
                    stream_id,
                    chunk_seq,
                    data: body[16..].to_vec(),
                })
            }
            KIND_BULK_WINDOW_CREDIT => {
                if body.len() != 12 {
                    return Err(FrameCodecError::MalformedFrame(
                        "BulkWindowCredit len != 12",
                    ));
                }
                let stream_id = u64::from_be_bytes(body[0..8].try_into().expect("8B"));
                let granted_bytes = u32::from_be_bytes(body[8..12].try_into().expect("4B"));
                if granted_bytes == 0 {
                    return Err(FrameCodecError::MalformedFrame("zero BulkWindowCredit"));
                }
                Ok(Self::BulkWindowCredit {
                    stream_id,
                    granted_bytes,
                })
            }
            KIND_BULK_FINISH => {
                if body.len() != 8 + 4 + 8 + 32 {
                    return Err(FrameCodecError::MalformedFrame(
                        "BulkStreamFinish len != 52",
                    ));
                }
                let stream_id = u64::from_be_bytes(body[0..8].try_into().expect("8B"));
                let total_chunks = u32::from_be_bytes(body[8..12].try_into().expect("4B"));
                let total_bytes = u64::from_be_bytes(body[12..20].try_into().expect("8B"));
                let sha256_digest: [u8; 32] = body[20..52].try_into().expect("32B");
                Ok(Self::BulkStreamFinish {
                    stream_id,
                    total_chunks,
                    total_bytes,
                    sha256_digest,
                })
            }
            KIND_BULK_COMPLETE_ACK => {
                if body.len() != 8 + 8 + 8 + 32 + 1 {
                    return Err(FrameCodecError::MalformedFrame(
                        "BulkStreamCompleteAck len != 57",
                    ));
                }
                let stream_id = u64::from_be_bytes(body[0..8].try_into().expect("8B"));
                let epoch_raw = u64::from_be_bytes(body[8..16].try_into().expect("8B"));
                let observed_epoch = CellEpoch::from_u64(epoch_raw)
                    .ok_or(FrameCodecError::MalformedFrame("zero observed_epoch"))?;
                let verified_bytes = u64::from_be_bytes(body[16..24].try_into().expect("8B"));
                let verified_sha256: [u8; 32] = body[24..56].try_into().expect("32B");
                let idempotency_replayed = match body[56] {
                    0 => false,
                    1 => true,
                    _ => return Err(FrameCodecError::MalformedFrame("invalid ack replay bool")),
                };
                Ok(Self::BulkStreamCompleteAck {
                    stream_id,
                    observed_epoch,
                    verified_bytes,
                    verified_sha256,
                    idempotency_replayed,
                })
            }
            _ => Err(FrameCodecError::MalformedFrame("unknown MuxFrame kind tag")),
        }
    }
}

/// Fail-closed codec and framing errors.
#[derive(Debug)]
pub enum FrameCodecError {
    /// Unsupported wire protocol version.
    UnsupportedVersion(u8),
    /// `ControlRpc` payload exceeded `max_control_payload_bytes`.
    ControlPayloadTooLarge {
        /// Actual byte length.
        actual: usize,
        /// Configured upper bound.
        maximum: usize,
    },
    /// `BulkChunk` violated `1..=64 KiB` bound.
    BulkChunkOutOfBounds {
        /// Actual byte length.
        actual: usize,
        /// Mandatory `64 KiB` upper bound.
        maximum: usize,
    },
    /// Outer 4-byte length prefix exceeded wire maximum.
    OuterFrameTooLarge {
        /// Declared byte length.
        actual: usize,
        /// Configured wire maximum.
        maximum: usize,
    },
    /// Structural frame parse violation.
    MalformedFrame(&'static str),
    /// Read or write operation timed out at `phase`.
    Timeout {
        /// Write progress phase when timeout fired.
        phase: WriteProgressPhase,
        /// Structured failure kind (`ReadTimeout` or `WriteTimeout`).
        kind: SocketFailureKind,
    },
    /// Underlying socket/TLS I/O error tagged with exact `WriteProgressPhase`.
    Io {
        /// Write progress phase when error occurred.
        phase: WriteProgressPhase,
        /// Structured socket failure kind.
        kind: SocketFailureKind,
        /// Raw OS/TLS `io::Error`.
        source: io::Error,
    },
}

impl fmt::Display for FrameCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedVersion(v) => write!(formatter, "unsupported mux frame version {v}"),
            Self::ControlPayloadTooLarge { actual, maximum } => write!(
                formatter,
                "control RPC payload {actual} B exceeds maximum {maximum} B"
            ),
            Self::BulkChunkOutOfBounds { actual, maximum } => write!(
                formatter,
                "bulk chunk size {actual} B outside allowed 1..={maximum} B"
            ),
            Self::OuterFrameTooLarge { actual, maximum } => write!(
                formatter,
                "outer wire frame {actual} B exceeds maximum {maximum} B"
            ),
            Self::MalformedFrame(msg) => write!(formatter, "malformed mux frame: {msg}"),
            Self::Timeout { phase, kind } => {
                write!(formatter, "mux frame {kind:?} at phase {phase:?}")
            }
            Self::Io {
                phase,
                kind,
                source,
            } => write!(
                formatter,
                "mux frame I/O failure ({kind:?} at {phase:?}): {source}"
            ),
        }
    }
}

impl Error for FrameCodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Writes one 4B big-endian length-prefixed [`MuxFrame`] onto `writer`,
/// tracking exact [`WriteProgressPhase`] (`BeforeFirstPayloadByte` prior to
/// encoding/first write vs `AfterFirstPayloadByte` once bytes enter the stream).
///
/// # Errors
///
/// Returns [`FrameCodecError`] with exact `WriteProgressPhase` for
/// `[DIST-FAIL-001]` classification.
pub async fn write_mux_frame<W>(
    writer: &mut W,
    frame: &MuxFrame,
    max_control_payload_bytes: usize,
    write_timeout: Duration,
) -> Result<(), FrameCodecError>
where
    W: AsyncWrite + Unpin,
{
    let body = frame.encode_body(max_control_payload_bytes)?;
    let max_wire = MAX_MUX_WIRE_FRAME_BYTES.max(max_control_payload_bytes + 128);
    if body.len() > max_wire {
        return Err(FrameCodecError::OuterFrameTooLarge {
            actual: body.len(),
            maximum: max_wire,
        });
    }
    let len = u32::try_from(body.len()).map_err(|_| FrameCodecError::OuterFrameTooLarge {
        actual: body.len(),
        maximum: max_wire,
    })?;

    let mut wire = Vec::with_capacity(LENGTH_PREFIX_BYTES + body.len());
    wire.extend_from_slice(&len.to_be_bytes());
    wire.extend_from_slice(&body);

    let mut written = 0usize;
    let send_fut = async {
        while written < wire.len() {
            match writer.write(&wire[written..]).await {
                Ok(0) => {
                    let phase = if written == 0 {
                        WriteProgressPhase::BeforeFirstPayloadByte
                    } else {
                        WriteProgressPhase::AfterFirstPayloadByte
                    };
                    return Err(FrameCodecError::Io {
                        phase,
                        kind: SocketFailureKind::UnexpectedEofOrTlsTruncation,
                        source: io::Error::new(io::ErrorKind::WriteZero, "zero-byte TLS write"),
                    });
                }
                Ok(n) => {
                    written += n;
                }
                Err(source) => {
                    let phase = if written == 0 {
                        WriteProgressPhase::BeforeFirstPayloadByte
                    } else {
                        WriteProgressPhase::AfterFirstPayloadByte
                    };
                    return Err(FrameCodecError::Io {
                        phase,
                        kind: SocketFailureKind::from_stream_io_error(&source),
                        source,
                    });
                }
            }
        }
        writer.flush().await.map_err(|source| FrameCodecError::Io {
            phase: WriteProgressPhase::AfterFirstPayloadByte,
            kind: SocketFailureKind::from_stream_io_error(&source),
            source,
        })
    };

    timeout(write_timeout, send_fut)
        .await
        .map_err(|_| FrameCodecError::Timeout {
            // Once write_timeout expires while polling `write`/`flush`, bytes
            // may already have entered the OS/TLS buffer (`[DIST-FAIL-001]`).
            phase: WriteProgressPhase::AfterFirstPayloadByte,
            kind: SocketFailureKind::WriteTimeout,
        })?
}

/// Reads one 4B big-endian length-prefixed [`MuxFrame`] from `reader` without
/// allocating oversized buffers from untrusted length headers.
///
/// # Errors
///
/// Returns [`FrameCodecError`] (`AfterFirstPayloadByte` on client response
/// reads).
pub async fn read_mux_frame<R>(
    reader: &mut R,
    max_control_payload_bytes: usize,
    read_timeout: Duration,
) -> Result<MuxFrame, FrameCodecError>
where
    R: AsyncRead + Unpin,
{
    let max_wire = MAX_MUX_WIRE_FRAME_BYTES.max(max_control_payload_bytes + 128);
    let recv_fut = async {
        let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
        reader
            .read_exact(&mut prefix)
            .await
            .map_err(|source| FrameCodecError::Io {
                phase: WriteProgressPhase::AfterFirstPayloadByte,
                kind: SocketFailureKind::from_stream_io_error(&source),
                source,
            })?;
        let declared = u32::from_be_bytes(prefix) as usize;
        if declared == 0 || declared > max_wire {
            return Err(FrameCodecError::OuterFrameTooLarge {
                actual: declared,
                maximum: max_wire,
            });
        }
        let mut body = vec![0u8; declared];
        reader
            .read_exact(&mut body)
            .await
            .map_err(|source| FrameCodecError::Io {
                phase: WriteProgressPhase::AfterFirstPayloadByte,
                kind: SocketFailureKind::from_stream_io_error(&source),
                source,
            })?;
        MuxFrame::decode_body(&body, max_control_payload_bytes)
    };

    timeout(read_timeout, recv_fut)
        .await
        .map_err(|_| FrameCodecError::Timeout {
            phase: WriteProgressPhase::AfterFirstPayloadByte,
            kind: SocketFailureKind::ReadTimeout,
        })?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_frame_roundtrips_all_variants_and_enforces_64kib_ceiling() {
        let frames = vec![
            MuxFrame::ControlRpcRequest {
                stream_id: 7,
                call_id: [0x11; 16],
                idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
                deadline_tick: 99,
                side_effecting: true,
                payload: b"rpc-req".to_vec(),
            },
            MuxFrame::ControlRpcResponse {
                stream_id: 7,
                observed_epoch: CellEpoch::from_u64(3).expect("nz"),
                idempotency_replayed: true,
                status: RpcWireStatus::Ok,
                payload: b"rpc-resp".to_vec(),
            },
            MuxFrame::BulkStreamOpen {
                stream_id: 8,
                call_id: [0x33; 16],
                idempotency_key: IdempotencyKey::from_bytes([0x44; 16]),
                total_bytes: 67_108_864,
            },
            MuxFrame::BulkChunk {
                stream_id: 8,
                chunk_seq: 0,
                data: vec![0xa5; MAX_BULK_CHUNK_BYTES],
            },
            MuxFrame::BulkWindowCredit {
                stream_id: 8,
                granted_bytes: 65_536,
            },
            MuxFrame::BulkStreamFinish {
                stream_id: 8,
                total_chunks: 1024,
                total_bytes: 67_108_864,
                sha256_digest: [0xee; 32],
            },
            MuxFrame::BulkStreamCompleteAck {
                stream_id: 8,
                observed_epoch: CellEpoch::from_u64(3).expect("nz"),
                verified_bytes: 67_108_864,
                verified_sha256: [0xee; 32],
                idempotency_replayed: false,
            },
        ];

        for frame in frames {
            let encoded = frame
                .encode_body(DEFAULT_MAX_CONTROL_PAYLOAD_BYTES)
                .expect("encode frame");
            let decoded = MuxFrame::decode_body(&encoded, DEFAULT_MAX_CONTROL_PAYLOAD_BYTES)
                .expect("decode frame");
            assert_eq!(frame, decoded);
        }

        let oversize_bulk = MuxFrame::BulkChunk {
            stream_id: 9,
            chunk_seq: 0,
            data: vec![0u8; MAX_BULK_CHUNK_BYTES + 1],
        };
        assert!(matches!(
            oversize_bulk.encode_body(DEFAULT_MAX_CONTROL_PAYLOAD_BYTES),
            Err(FrameCodecError::BulkChunkOutOfBounds {
                actual: 65_537,
                maximum: 65_536
            })
        ));
    }
}
