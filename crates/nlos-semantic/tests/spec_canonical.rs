use nlos_semantic::{
    CriterionAggregation, CriterionEffect, EvaluatorKind, ImmutableEvaluatorReference,
    ImmutableEvaluatorReferenceKind, IntentConstraints, IntentCriterion, IntentCriticality,
    IntentSettlement, IntentSpecBody, SemanticAuthorityError, SettlementMode,
    SettlementTimeoutAction, SpecExtension, criterion_id, decode_intent_spec_body,
    encode_intent_spec_body, hard_criteria_digest, intent_spec_body_digest,
};
use nlos_types::NamespaceId;

fn criterion(seed: u8, effect: CriterionEffect) -> IntentCriterion {
    IntentCriterion {
        description_digest: [seed; 32],
        effect,
        evaluator_kind: EvaluatorKind::DeterministicTool,
        evaluator_ref: ImmutableEvaluatorReference {
            kind: ImmutableEvaluatorReferenceKind::Artifact,
            digest: [seed.wrapping_add(1); 32],
        },
        target_selector_digest: [seed.wrapping_add(2); 32],
        timeout_ms: Some(30_000),
        independence_policy_digest: None,
        authority_policy_digest: None,
        risk_policy_digest: None,
        aggregation: CriterionAggregation {
            pass_quorum: 1,
            fail_quorum: 1,
            veto_on_authorized_fail: true,
        },
    }
}

fn body() -> IntentSpecBody {
    let mut body = IntentSpecBody {
        goal_digest: [0x11; 32],
        acceptance: vec![
            criterion(0x31, CriterionEffect::Soft),
            criterion(0x21, CriterionEffect::Hard),
        ],
        constraints: IntentConstraints {
            resource_vector_digest: [0x41; 32],
            deadline_ms: Some(90_000),
            namespace_root: NamespaceId::from_bytes([0x42; 16]),
            allowed_capability_digests: vec![[0x51; 32], [0x52; 32]],
            forbidden_capability_digests: vec![[0x61; 32]],
        },
        criticality: IntentCriticality::High,
        settlement: IntentSettlement {
            mode: SettlementMode::Automatic,
            hard_criteria_digest: None,
            on_timeout: SettlementTimeoutAction::Dispute,
            challenge_window_ms: Some(5_000),
        },
        critical_extensions: Vec::new(),
        noncritical_extensions: vec![SpecExtension {
            id: 7,
            value: vec![1, 2, 3],
        }],
    };
    body.settlement.hard_criteria_digest = hard_criteria_digest(&body).unwrap();
    body
}

#[test]
fn body_round_trips_and_acceptance_order_does_not_change_identity() {
    let first = body();
    let bytes = encode_intent_spec_body(&first).unwrap();
    assert_eq!(decode_intent_spec_body(&bytes).unwrap(), first);

    let mut reordered = first.clone();
    reordered.acceptance.reverse();
    assert_eq!(encode_intent_spec_body(&reordered).unwrap(), bytes);
    assert_eq!(
        intent_spec_body_digest(&reordered).unwrap(),
        intent_spec_body_digest(&first).unwrap()
    );

    let mut trailing = bytes;
    trailing.push(0);
    assert!(matches!(
        decode_intent_spec_body(&trailing),
        Err(SemanticAuthorityError::CanonicalMismatch)
    ));
}

#[test]
fn criterion_and_body_identity_cover_every_semantic_change() {
    let original = body();
    let original_criterion = criterion_id(&original.acceptance[0]).unwrap();
    let original_body = intent_spec_body_digest(&original).unwrap();

    let mut changed = original.clone();
    changed.acceptance[0].aggregation.pass_quorum = 2;
    assert_ne!(
        criterion_id(&changed.acceptance[0]).unwrap(),
        original_criterion
    );
    assert_ne!(intent_spec_body_digest(&changed).unwrap(), original_body);
}

#[test]
fn automatic_settlement_binds_the_complete_nonempty_hard_set() {
    let mut invalid = body();
    invalid.settlement.hard_criteria_digest = Some([0xaa; 32]);
    assert!(matches!(
        encode_intent_spec_body(&invalid),
        Err(SemanticAuthorityError::InvalidSpecBody(_))
    ));

    invalid = body();
    invalid.acceptance[1].effect = CriterionEffect::Soft;
    invalid.settlement.hard_criteria_digest = None;
    assert!(matches!(
        encode_intent_spec_body(&invalid),
        Err(SemanticAuthorityError::InvalidSpecBody(_))
    ));
}

#[test]
fn invalid_quorum_policy_and_extensions_fail_closed() {
    let mut invalid = body();
    invalid.acceptance[0].aggregation.pass_quorum = 0;
    assert!(matches!(
        encode_intent_spec_body(&invalid),
        Err(SemanticAuthorityError::InvalidSpecBody(_))
    ));

    invalid = body();
    invalid.acceptance[1].evaluator_kind = EvaluatorKind::Human;
    assert!(matches!(
        encode_intent_spec_body(&invalid),
        Err(SemanticAuthorityError::InvalidSpecBody(_))
    ));

    invalid = body();
    invalid.critical_extensions.push(SpecExtension {
        id: 1,
        value: vec![1],
    });
    assert!(matches!(
        encode_intent_spec_body(&invalid),
        Err(SemanticAuthorityError::UnsupportedCriticalSpecExtension)
    ));
}

#[test]
fn capability_sets_and_none_settlement_are_strict() {
    let mut invalid = body();
    invalid.constraints.allowed_capability_digests.reverse();
    assert!(matches!(
        encode_intent_spec_body(&invalid),
        Err(SemanticAuthorityError::InvalidSpecBody(_))
    ));

    invalid = body();
    invalid.settlement.mode = SettlementMode::None;
    assert!(matches!(
        encode_intent_spec_body(&invalid),
        Err(SemanticAuthorityError::InvalidSpecBody(_))
    ));
}
