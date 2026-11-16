//! Canonical SABI envelope adapter for the four gateway methods.

use nlos_capability::service_call::{AuthorizationError, CallBinding, ServiceMethod};
use nlos_schema::sabi::v1::{
    AttenuateBudgetRequest, Envelope, LlmChatRequest, OperationReference, QueryBudgetRequest,
    ReceiptReference, RecordExternalUsageRequest, RetryDirective, SabiErrorCode, SabiFailure,
    SabiRequestContext, SabiResponseContext, envelope::CommonContext,
};
use nlos_schema::{
    CommonSemanticsError, MAX_LLM_GATEWAY_PAYLOAD_BYTES, MethodSemantics,
    decode_attenuate_budget_request, decode_llm_chat_request, decode_query_budget_request,
    decode_record_external_usage_request, decode_sabi_envelope, encode_attenuate_budget_result,
    encode_llm_chat_result, encode_query_budget_result, encode_record_external_usage_result,
    encode_sabi_envelope, llm_gateway_schema_identity, validate_sabi_request_context,
    validate_sabi_response_context,
};
use nlos_types::{Generation, OperationId, ReceiptId};

use super::{
    ChildBudgetBinding, ChildSessionBinding, Gateway, GatewayAccountBinding, GatewayError,
    SERVICE_NAME, WireReply,
};

const MAX_GATEWAY_FRAME_BYTES: usize = MAX_LLM_GATEWAY_PAYLOAD_BYTES + 8 * 1024;

impl Gateway {
    /// Dispatches the four frozen wire methods over a bounded SABI envelope.
    ///
    /// The host resolves the wire capability slots before entering this method,
    /// supplies those slots in order, and supplies the resulting independent
    /// runtime binding. Child bindings and replies are trusted host sidecars.
    ///
    /// # Errors
    /// Rejects frames that cannot be correlated or are not gateway requests.
    /// Correlatable method failures are returned as canonical SABI failures.
    pub async fn handle_wire(
        &self,
        frame: &[u8],
        call: &CallBinding,
        resolved_wire_slots: &[u64],
        account: &GatewayAccountBinding,
        child: Option<&ChildSessionBinding>,
    ) -> Result<WireReply, GatewayError> {
        if frame.len() > MAX_GATEWAY_FRAME_BYTES {
            return Err(GatewayError::FrameTooLarge);
        }
        let frame = decode_sabi_envelope(frame)?;
        let envelope = frame.envelope();
        if envelope.service != SERVICE_NAME
            || envelope
                .schema
                .as_ref()
                .is_none_or(|schema| schema.name != nlos_schema::SABI_LLM_GATEWAY_SCHEMA)
        {
            return Err(GatewayError::IncompatibleSchema);
        }
        let method = match envelope.method.as_str() {
            "llm_chat" => ServiceMethod::LlmChat,
            "query_budget" => ServiceMethod::QueryBudget,
            "attenuate_budget" => ServiceMethod::AttenuateBudget,
            "record_external_usage" => ServiceMethod::RecordExternalUsage,
            _ => return Err(GatewayError::UnsupportedMethod),
        };
        let semantics = MethodSemantics {
            side_effecting: method.requires_idempotency_key(),
            long_running: method.requires_deadline(),
        };
        let CommonContext::RequestContext(header) = envelope
            .common_context
            .as_ref()
            .ok_or(GatewayError::InvalidRequest)?
        else {
            return Err(GatewayError::InvalidRequest);
        };
        if header.correlation_id.len() != 16 {
            return Err(GatewayError::InvalidRequest);
        }
        let result = self
            .dispatch(
                envelope,
                method,
                semantics,
                call,
                resolved_wire_slots,
                account,
                child,
            )
            .await;
        wire_reply(envelope, semantics, header, result)
    }

    #[allow(clippy::too_many_arguments)]
    async fn dispatch(
        &self,
        envelope: &Envelope,
        method: ServiceMethod,
        semantics: MethodSemantics,
        call: &CallBinding,
        resolved_wire_slots: &[u64],
        account: &GatewayAccountBinding,
        child: Option<&ChildSessionBinding>,
    ) -> Result<WireParts, GatewayError> {
        let payload = &envelope.payload;
        if payload.len() > MAX_LLM_GATEWAY_PAYLOAD_BYTES {
            return Err(GatewayError::FrameTooLarge);
        }
        // Each branch uses the frozen method codec before trusting any common
        // metadata. The named method below fetches live time before policy.
        let request = match method {
            ServiceMethod::LlmChat => WireRequest::Chat(decode_llm_chat_request(payload)?),
            ServiceMethod::QueryBudget => WireRequest::Query(decode_query_budget_request(payload)?),
            ServiceMethod::AttenuateBudget => {
                WireRequest::Attenuate(decode_attenuate_budget_request(payload)?)
            }
            ServiceMethod::RecordExternalUsage => {
                WireRequest::Advisory(decode_record_external_usage_request(payload)?)
            }
            _ => return Err(GatewayError::UnsupportedMethod),
        };
        let header =
            validate_sabi_request_context(envelope, semantics, 0).map_err(|error| match error {
                CommonSemanticsError::MissingIdempotencyKey => {
                    AuthorizationError::MissingIdempotencyKey.into()
                }
                CommonSemanticsError::MissingDeadline => AuthorizationError::MissingDeadline.into(),
                _ => GatewayError::InvalidRequest,
            })?;
        verify_wire_binding(header, call, resolved_wire_slots)?;
        let digest = &header.proposal_or_input_digest_sha256;
        match request {
            WireRequest::Chat(request) => {
                let reply = self.chat(call, account, &request, digest).await?;
                Ok(WireParts {
                    payload: encode_llm_chat_result(&reply.result)?,
                    operation: Some(reply.operation),
                    receipt: Some(reply.receipt),
                    child: None,
                })
            }
            WireRequest::Query(request) => {
                let result = self.query(call, account, &request, digest)?;
                Ok(WireParts {
                    payload: encode_query_budget_result(&result)?,
                    ..WireParts::default()
                })
            }
            WireRequest::Attenuate(request) => {
                let reply = self.attenuate(call, account, child, &request, digest)?;
                Ok(WireParts {
                    payload: encode_attenuate_budget_result(&reply.result)?,
                    receipt: Some(reply.child.receipt),
                    child: Some(reply.child),
                    operation: None,
                })
            }
            WireRequest::Advisory(request) => {
                let reply = self.advisory(call, account, &request, digest)?;
                Ok(WireParts {
                    payload: encode_record_external_usage_result(&reply.result)?,
                    receipt: Some(reply.receipt),
                    ..WireParts::default()
                })
            }
        }
    }
}

enum WireRequest {
    Chat(LlmChatRequest),
    Query(QueryBudgetRequest),
    Attenuate(AttenuateBudgetRequest),
    Advisory(RecordExternalUsageRequest),
}

#[derive(Default)]
struct WireParts {
    payload: Vec<u8>,
    operation: Option<OperationId>,
    receipt: Option<ReceiptId>,
    child: Option<ChildBudgetBinding>,
}

pub(super) fn method_name(method: ServiceMethod) -> &'static str {
    match method {
        ServiceMethod::LlmChat => "llm_chat",
        ServiceMethod::QueryBudget => "query_budget",
        ServiceMethod::AttenuateBudget => "attenuate_budget",
        ServiceMethod::RecordExternalUsage => "record_external_usage",
        _ => unreachable!("driver only dispatches gateway methods"),
    }
}

fn verify_wire_binding(
    header: &SabiRequestContext,
    call: &CallBinding,
    slots: &[u64],
) -> Result<(), GatewayError> {
    let caller = header.caller.as_ref().ok_or(GatewayError::InvalidRequest)?;
    if caller.principal_id != call.caller.principal.as_bytes()
        || caller.application_id != call.caller.application.as_bytes()
        || caller.process_id != call.caller.process.as_bytes()
        || caller.process_generation != call.caller.process_generation.get()
    {
        return Err(AuthorizationError::BindingMismatch.into());
    }
    if header.task_execution_binding.is_some() || header.reservation_handle.is_some() {
        return Err(GatewayError::UnsupportedBinding);
    }
    let expected_key: &[u8] = call
        .idempotency_key
        .as_ref()
        .map_or(&[], |key| key.as_bytes());
    if header.idempotency_key != expected_key
        || header.deadline_monotonic_ns != call.deadline_monotonic_ns.unwrap_or(0)
        || header.capability_handles.len() != call.capabilities.len()
        || header.capability_handles.len() != slots.len()
        || header
            .capability_handles
            .iter()
            .zip(slots.iter().zip(&call.capabilities))
            .any(|(wire, (&slot, resolved))| {
                wire.slot != slot || wire.generation != resolved.generation.get()
            })
    {
        return Err(AuthorizationError::BindingMismatch.into());
    }
    Ok(())
}

fn wire_reply(
    request: &Envelope,
    semantics: MethodSemantics,
    header: &SabiRequestContext,
    dispatch: Result<WireParts, GatewayError>,
) -> Result<WireReply, GatewayError> {
    let (parts, failure) = match dispatch {
        Ok(parts) => (parts, None),
        Err(error) => {
            let (code, operation, retry) = wire_failure(error);
            (
                WireParts {
                    operation,
                    ..WireParts::default()
                },
                Some(SabiFailure {
                    code: code as i32,
                    retry: retry as i32,
                    safe_message: error.to_string(),
                }),
            )
        }
    };
    let response = Envelope {
        schema: Some(llm_gateway_schema_identity()),
        request_id: request.request_id.clone(),
        service: SERVICE_NAME.to_owned(),
        method: request.method.clone(),
        payload: parts.payload,
        common_context: Some(CommonContext::ResponseContext(SabiResponseContext {
            correlation_id: header.correlation_id.clone(),
            operation: parts.operation.map(|operation| OperationReference {
                operation_id: operation.as_bytes().to_vec(),
                generation: Generation::INITIAL.get(),
            }),
            receipts: parts
                .receipt
                .into_iter()
                .map(|receipt| ReceiptReference {
                    receipt_id: receipt.as_bytes().to_vec(),
                })
                .collect(),
            failure,
        })),
    };
    validate_sabi_response_context(&response, semantics).map_err(|_| GatewayError::Durability)?;
    let frame = encode_sabi_envelope(&response)?;
    if frame.len() > MAX_GATEWAY_FRAME_BYTES {
        return Err(GatewayError::FrameTooLarge);
    }
    Ok(WireReply {
        frame,
        child: parts.child,
    })
}

fn wire_failure(error: GatewayError) -> (SabiErrorCode, Option<OperationId>, RetryDirective) {
    if let GatewayError::EffectUnknown(operation) = error {
        return (
            SabiErrorCode::EffectUnknown,
            Some(operation),
            RetryDirective::QueryOperationOrRetrySameIdempotencyKey,
        );
    }
    let mut reconciliation = None;
    let code = match error {
        GatewayError::Authorization(
            AuthorizationError::DeadlineExpired | AuthorizationError::MissingDeadline,
        ) => SabiErrorCode::Deadline,
        GatewayError::Authorization(AuthorizationError::MissingIdempotencyKey) => {
            SabiErrorCode::InvalidArgument
        }
        GatewayError::Authorization(_) => SabiErrorCode::Rights,
        GatewayError::IncompatibleSchema => SabiErrorCode::AbiVersion,
        GatewayError::UnsupportedMethod
        | GatewayError::UnsupportedModel
        | GatewayError::UnsupportedBinding => SabiErrorCode::NotSupported,
        GatewayError::Budget(_) => SabiErrorCode::Budget,
        GatewayError::Quota | GatewayError::WriterBusy => SabiErrorCode::Quota,
        GatewayError::Conflict => SabiErrorCode::Conflict,
        GatewayError::Durability => SabiErrorCode::Durability,
        GatewayError::ProviderFailure | GatewayError::UsageUnknown => SabiErrorCode::Driver,
        GatewayError::SettlementRejected(operation) => {
            reconciliation = Some(operation);
            SabiErrorCode::Budget
        }
        _ => SabiErrorCode::InvalidArgument,
    };
    (code, reconciliation, RetryDirective::DoNotRetry)
}
