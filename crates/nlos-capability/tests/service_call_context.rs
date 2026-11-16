use std::sync::atomic::{AtomicUsize, Ordering};

use nlos_capability::CapabilityHandle;
use nlos_capability::service_call::{
    AuthorizationError, CallBinding, DenyAllAuthorizer, PolicyGrant, RuntimeCaller, ServiceAction,
    ServiceAuthorizationBinding, ServiceAuthorizer, ServiceMethod, ServiceObject, ServiceTarget,
    ServiceTime, authorize_call,
};
use nlos_types::{
    ApplicationId, CapabilityId, Generation, IdempotencyKey, NamespaceId, PrincipalId, ProcessId,
    ResourceAccountId, SessionId, WorkspaceId,
};

const NOW: ServiceTime = ServiceTime {
    wall_ms: 1_000,
    monotonic_ns: 100,
};

fn handle(marker: u8) -> CapabilityHandle {
    CapabilityHandle {
        capability_id: CapabilityId::from_bytes([marker; 16]),
        generation: Generation::INITIAL,
    }
}

fn call() -> CallBinding {
    CallBinding {
        caller: RuntimeCaller {
            principal: PrincipalId::from_bytes([1; 16]),
            application: ApplicationId::from_bytes([2; 16]),
            process: ProcessId::from_bytes([3; 16]),
            process_generation: Generation::INITIAL,
            session: SessionId::from_bytes([4; 16]),
        },
        idempotency_key: Some(IdempotencyKey::from_bytes([5; 16])),
        deadline_monotonic_ns: Some(101),
        capabilities: vec![handle(6), handle(7)],
    }
}

fn action(method: ServiceMethod) -> ServiceAction {
    ServiceAction {
        method,
        target: ServiceTarget {
            namespace: NamespaceId::from_bytes([8; 16]),
            object: ServiceObject::GatewayAccount(ResourceAccountId::from_bytes([9; 16])),
            generation: Generation::INITIAL,
        },
        input_digest: [10; 32],
    }
}

struct PolicyFixture<F> {
    calls: AtomicUsize,
    respond: F,
}

impl<F> PolicyFixture<F>
where
    F: Fn(&ServiceAuthorizationBinding, ServiceTime) -> Result<PolicyGrant, AuthorizationError>,
{
    fn new(respond: F) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            respond,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }
}

impl<F> ServiceAuthorizer for PolicyFixture<F>
where
    F: Fn(&ServiceAuthorizationBinding, ServiceTime) -> Result<PolicyGrant, AuthorizationError>
        + Send
        + Sync,
{
    fn authorize(
        &self,
        binding: &ServiceAuthorizationBinding,
        now: ServiceTime,
    ) -> Result<PolicyGrant, AuthorizationError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        (self.respond)(binding, now)
    }
}

fn grant(binding: &ServiceAuthorizationBinding) -> PolicyGrant {
    PolicyGrant {
        binding: binding.clone(),
        capability: binding.call.capabilities[0],
        valid_from_ms: 900,
        valid_until_ms: 1_100,
    }
}

#[test]
fn default_authorizer_denies_a_well_formed_call() {
    let authorizer = <DenyAllAuthorizer as Default>::default();
    assert_eq!(
        authorize_call(&authorizer, &call(), &action(ServiceMethod::LlmChat), NOW).unwrap_err(),
        AuthorizationError::Denied
    );
}

#[test]
fn successful_result_preserves_the_exact_authorized_binding_and_selected_handle() {
    let call = call();
    let action = action(ServiceMethod::LlmChat);
    let expected = ServiceAuthorizationBinding {
        call: call.clone(),
        action,
    };
    let authorizer = PolicyFixture::new(|binding, now| {
        assert_eq!(binding, &expected);
        assert_eq!(now, NOW);
        let mut grant = grant(binding);
        grant.capability = handle(7);
        Ok(grant)
    });

    let checked = authorize_call(&authorizer, &call, &action, NOW).unwrap();

    assert_eq!(checked.binding(), &expected);
    assert_eq!(checked.capability(), handle(7));
    assert_eq!(checked.valid_from_ms(), 900);
    assert_eq!(checked.valid_until_ms(), 1_100);
    assert_eq!(authorizer.calls(), 1);
}

#[test]
fn all_methods_enforce_their_key_and_deadline_requirements() {
    // Method, idempotency key required, deadline required.
    let cases = [
        (ServiceMethod::LlmChat, true, true),
        (ServiceMethod::QueryBudget, false, false),
        (ServiceMethod::AttenuateBudget, true, false),
        (ServiceMethod::RecordExternalUsage, true, false),
        (ServiceMethod::ListTools, false, true),
        (ServiceMethod::InvokeTool, true, true),
        (ServiceMethod::MintReference, true, true),
        (ServiceMethod::ReadReference, true, false),
        (ServiceMethod::ReleaseReference, true, false),
        (ServiceMethod::QueryToolOperation, false, false),
        (ServiceMethod::CancelToolOperation, true, false),
    ];
    for (method, needs_key, needs_deadline) in cases {
        let action = action(method);
        let authorizer = PolicyFixture::new(|binding, _| Ok(grant(binding)));
        assert_eq!(method.requires_idempotency_key(), needs_key, "{method:?}");
        assert_eq!(method.requires_deadline(), needs_deadline, "{method:?}");

        let mut no_key = call();
        no_key.idempotency_key = None;
        let result = authorize_call(&authorizer, &no_key, &action, NOW);
        if needs_key {
            assert_eq!(
                result.unwrap_err(),
                AuthorizationError::MissingIdempotencyKey,
                "{method:?}"
            );
        } else {
            assert!(result.is_ok(), "{method:?}");
        }
        assert_eq!(authorizer.calls(), usize::from(!needs_key), "{method:?}");

        let mut no_deadline = call();
        no_deadline.deadline_monotonic_ns = None;
        let result = authorize_call(&authorizer, &no_deadline, &action, NOW);
        if needs_deadline {
            assert_eq!(
                result.unwrap_err(),
                AuthorizationError::MissingDeadline,
                "{method:?}"
            );
        } else {
            assert!(result.is_ok(), "{method:?}");
        }
        assert_eq!(
            authorizer.calls(),
            usize::from(!needs_key) + usize::from(!needs_deadline),
            "{method:?}"
        );
    }
}

#[test]
fn supplied_deadlines_must_strictly_exceed_the_host_monotonic_time() {
    let authorizer = PolicyFixture::new(|binding, _| Ok(grant(binding)));
    for method in [ServiceMethod::LlmChat, ServiceMethod::QueryBudget] {
        for deadline in [0, 99, 100] {
            let mut call = call();
            call.deadline_monotonic_ns = Some(deadline);
            assert_eq!(
                authorize_call(&authorizer, &call, &action(method), NOW).unwrap_err(),
                AuthorizationError::DeadlineExpired,
                "{method:?} with deadline {deadline}"
            );
        }
    }
    assert_eq!(authorizer.calls(), 0);
    // Zero must also be rejected at the origin of the host monotonic domain.
    let mut zero_deadline = call();
    zero_deadline.deadline_monotonic_ns = Some(0);
    assert_eq!(
        authorize_call(
            &authorizer,
            &zero_deadline,
            &action(ServiceMethod::QueryBudget),
            ServiceTime {
                monotonic_ns: 0,
                ..NOW
            },
        )
        .unwrap_err(),
        AuthorizationError::DeadlineExpired
    );
    assert_eq!(authorizer.calls(), 0);
}

#[test]
fn malformed_calls_fail_in_preflight_order_without_consulting_policy() {
    let authorizer = PolicyFixture::new(|_, _| Err(AuthorizationError::Denied));
    let action = action(ServiceMethod::LlmChat);
    let mut call = call();
    call.idempotency_key = None;
    call.deadline_monotonic_ns = None;
    call.capabilities.clear();
    assert_eq!(
        authorize_call(&authorizer, &call, &action, NOW).unwrap_err(),
        AuthorizationError::MissingIdempotencyKey
    );
    call.idempotency_key = Some(IdempotencyKey::from_bytes([5; 16]));
    assert_eq!(
        authorize_call(&authorizer, &call, &action, NOW).unwrap_err(),
        AuthorizationError::MissingDeadline
    );
    call.deadline_monotonic_ns = Some(NOW.monotonic_ns);
    assert_eq!(
        authorize_call(&authorizer, &call, &action, NOW).unwrap_err(),
        AuthorizationError::DeadlineExpired
    );
    call.deadline_monotonic_ns = Some(101);
    assert_eq!(
        authorize_call(&authorizer, &call, &action, NOW).unwrap_err(),
        AuthorizationError::EmptyCapabilities
    );
    assert_eq!(authorizer.calls(), 0);
}

#[test]
fn capability_sets_enforce_cardinality_and_exact_duplicate_identity() {
    let authorizer = PolicyFixture::new(|binding, _| Ok(grant(binding)));
    let action = action(ServiceMethod::QueryBudget);
    let mut call = call();
    call.capabilities = (0..65).map(handle).collect();
    assert_eq!(
        authorize_call(&authorizer, &call, &action, NOW).unwrap_err(),
        AuthorizationError::TooManyCapabilities
    );
    call.capabilities = vec![handle(7), handle(6), handle(7)];
    assert_eq!(
        authorize_call(&authorizer, &call, &action, NOW).unwrap_err(),
        AuthorizationError::DuplicateCapability
    );
    assert_eq!(authorizer.calls(), 0);
    call.capabilities = (0..64).map(handle).collect();
    assert!(authorize_call(&authorizer, &call, &action, NOW).is_ok());
    assert_eq!(authorizer.calls(), 1);
}

#[test]
fn grants_select_the_exact_presented_handle_generation() {
    let mut call = call();
    let action = action(ServiceMethod::QueryBudget);
    let new_incarnation = CapabilityHandle {
        generation: Generation::INITIAL.checked_next().unwrap(),
        ..handle(6)
    };
    let authorizer = PolicyFixture::new(|binding, _| {
        let mut grant = grant(binding);
        grant.capability = new_incarnation;
        Ok(grant)
    });
    assert_eq!(
        authorize_call(&authorizer, &call, &action, NOW).unwrap_err(),
        AuthorizationError::CapabilityNotPresented
    );
    call.capabilities.push(new_incarnation);
    assert_eq!(
        authorize_call(&authorizer, &call, &action, NOW)
            .unwrap()
            .capability(),
        new_incarnation
    );
}

type Substitution = (&'static str, fn(&mut ServiceAuthorizationBinding));

fn assert_binding_substitutions_rejected(action: &ServiceAction, substitutions: &[Substitution]) {
    for &(field, substitute) in substitutions {
        let authorizer = PolicyFixture::new(|binding, _| {
            let mut grant = grant(binding);
            substitute(&mut grant.binding);
            Ok(grant)
        });
        assert_eq!(
            authorize_call(&authorizer, &call(), action, NOW).unwrap_err(),
            AuthorizationError::BindingMismatch,
            "substituted {field}"
        );
        assert_eq!(authorizer.calls(), 1, "{field}");
    }
}

#[test]
fn policy_cannot_substitute_any_caller_or_call_field() {
    let substitutions: &[Substitution] = &[
        ("principal", |b| {
            b.call.caller.principal = PrincipalId::from_bytes([11; 16]);
        }),
        ("application", |b| {
            b.call.caller.application = ApplicationId::from_bytes([12; 16]);
        }),
        ("process", |b| {
            b.call.caller.process = ProcessId::from_bytes([13; 16]);
        }),
        ("process generation", |b| {
            b.call.caller.process_generation = Generation::INITIAL.checked_next().unwrap();
        }),
        ("session", |b| {
            b.call.caller.session = SessionId::from_bytes([14; 16]);
        }),
        ("idempotency key", |b| b.call.idempotency_key = None),
        ("deadline", |b| b.call.deadline_monotonic_ns = Some(102)),
        ("presented capabilities", |b| {
            b.call.capabilities.push(handle(15));
        }),
        ("presented capabilities order", |b| {
            b.call.capabilities.swap(0, 1);
        }),
    ];
    assert_binding_substitutions_rejected(&action(ServiceMethod::LlmChat), substitutions);
}

#[test]
fn policy_cannot_substitute_any_action_or_target_field() {
    let substitutions: &[Substitution] = &[
        ("method", |b| b.action.method = ServiceMethod::QueryBudget),
        ("namespace", |b| {
            b.action.target.namespace = NamespaceId::from_bytes([16; 16]);
        }),
        ("object kind", |b| {
            b.action.target.object = ServiceObject::Workspace(WorkspaceId::from_bytes([9; 16]));
        }),
        ("object id", |b| {
            b.action.target.object =
                ServiceObject::GatewayAccount(ResourceAccountId::from_bytes([17; 16]));
        }),
        ("target generation", |b| {
            b.action.target.generation = Generation::INITIAL.checked_next().unwrap();
        }),
        ("input digest", |b| b.action.input_digest = [18; 32]),
    ];
    assert_binding_substitutions_rejected(&action(ServiceMethod::LlmChat), substitutions);
}

#[test]
fn policy_grants_must_select_a_presented_capability_before_window_validation() {
    let authorizer = PolicyFixture::new(|binding, _| {
        let mut grant = grant(binding);
        grant.capability = handle(19);
        grant.valid_from_ms = 1_100;
        grant.valid_until_ms = 900;
        Ok(grant)
    });

    assert_eq!(
        authorize_call(
            &authorizer,
            &call(),
            &action(ServiceMethod::QueryBudget),
            NOW
        )
        .unwrap_err(),
        AuthorizationError::CapabilityNotPresented
    );
}

#[test]
fn grant_windows_use_wall_time_and_accept_both_endpoints() {
    let cases = [
        (1_100, 900, Err(AuthorizationError::InvalidGrantWindow)),
        (1_001, 1_100, Err(AuthorizationError::GrantNotYetValid)),
        (900, 999, Err(AuthorizationError::GrantExpired)),
        (1_000, 1_100, Ok(())),
        (900, 1_000, Ok(())),
        (1_000, 1_000, Ok(())),
    ];
    let call = call();
    let action = action(ServiceMethod::LlmChat);
    for (valid_from_ms, valid_until_ms, expected) in cases {
        let authorizer = PolicyFixture::new(|binding, _| {
            Ok(PolicyGrant {
                valid_from_ms,
                valid_until_ms,
                ..grant(binding)
            })
        });
        assert_eq!(
            authorize_call(&authorizer, &call, &action, NOW).map(|_| ()),
            expected,
            "window {valid_from_ms}..={valid_until_ms}"
        );
    }
}

#[test]
fn policy_errors_are_preserved_without_exposing_call_data_in_display() {
    let call = call();
    let action = action(ServiceMethod::LlmChat);
    let other_call = CallBinding {
        caller: RuntimeCaller {
            principal: PrincipalId::from_bytes(*b"private-marker!!"),
            ..call.caller
        },
        ..call.clone()
    };
    for error in [
        AuthorizationError::Denied,
        AuthorizationError::TimeUnavailable,
    ] {
        let authorizer = PolicyFixture::new(|_, _| Err(error));
        let first = authorize_call(&authorizer, &call, &action, NOW).unwrap_err();
        let second = authorize_call(&authorizer, &other_call, &action, NOW).unwrap_err();
        assert_eq!(first, error);
        assert_eq!(second, error);
        let display = second.to_string();
        assert_ne!(display, "");
        assert_eq!(first.to_string(), display);
        assert!(!display.contains("private-marker!!"));
        assert!(!display.contains("707269766174652d6d61726b65722121"));
        assert_eq!(authorizer.calls(), 2);
    }
}
