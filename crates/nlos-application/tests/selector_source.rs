//! Application selector-source adapter tests (W36-P7): the plan
//! resolver's `Application` ecosystem kind resolves through the real
//! application authority — a real install's generation advances the
//! pinned handle, a reinstall fences the stale generation on both the
//! resolve and verify faces, and unknown packages are typed misses.

mod support;

use std::sync::atomic::{AtomicU64, Ordering};

use nlos_application::ApplicationSelectorSource;
use nlos_plan::{
    EcosystemEntityKind, EcosystemResolutionError, EcosystemSelector, GenerationExpectation,
    PlanStoreError, ResolveEcosystemRequest, SqlitePlanAuthority,
};
use nlos_types::{IdempotencyKey, PackageId, ReceiptId};

use support::{TestStack, installed, open_authority};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn label(name: &str) -> String {
    format!(
        "selector-source-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn app_selector(package_id: PackageId, expectation: GenerationExpectation) -> EcosystemSelector {
    EcosystemSelector::Application {
        package_id,
        expectation,
    }
}

/// A real verified-package install resolves to generation 1 with the
/// manifest digest pinned; the second install advances the generation,
/// fencing the first handle stale on the verify face and the `At`
/// expectation on the resolve face; an unknown package is a typed miss;
/// the receipt stays inspectable after the world moved on.
#[test]
fn application_source_pins_install_generations_and_fences_on_reinstall() {
    let stack = TestStack::new(&label("pins"), 0x21);
    let plan_root = std::env::temp_dir().join(label("pins-plan"));
    std::fs::create_dir_all(&plan_root).expect("create plan db directory");
    let plan_authority =
        SqlitePlanAuthority::open(plan_root.join("plan.sqlite3")).expect("open plan authority");

    let first = stack.verify_package(0x41, 1, key(0xF0), 1_000);
    let authority = open_authority(stack.root.root());
    installed(&authority, &stack.artifacts, first.receipt_id, 0x01, 2_000);
    let source = ApplicationSelectorSource::new(&authority);

    let handle = plan_authority
        .resolve_ecosystem_selector(
            &source,
            ResolveEcosystemRequest {
                selector: app_selector(first.package_id, GenerationExpectation::Current),
                idempotency_key: key(0x51),
                resolved_at_ms: 3_000,
            },
        )
        .expect("resolve installed application")
        .handle();
    assert_eq!(handle.kind, EcosystemEntityKind::Application);
    assert_eq!(handle.generation, 1);
    assert_eq!(handle.content_digest, first.manifest_digest.into_bytes());
    assert!(matches!(
        plan_authority.verify_ecosystem_resolution_current(&source, handle.resolution_id),
        Ok(pinned) if pinned == handle
    ));

    let second = stack.verify_package(0x41, 2, key(0xF1), 4_000);
    installed(&authority, &stack.artifacts, second.receipt_id, 0x02, 5_000);
    assert!(matches!(
        plan_authority.verify_ecosystem_resolution_current(&source, handle.resolution_id),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::StaleEcosystemGeneration {
                expected: 1,
                current: 2,
                ..
            }
        ))
    ));
    assert!(matches!(
        plan_authority.resolve_ecosystem_selector(
            &source,
            ResolveEcosystemRequest {
                selector: app_selector(first.package_id, GenerationExpectation::At(1)),
                idempotency_key: key(0x52),
                resolved_at_ms: 6_000,
            },
        ),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::StaleEcosystemGeneration {
                expected: 1,
                current: 2,
                ..
            }
        ))
    ));

    let refreshed = plan_authority
        .resolve_ecosystem_selector(
            &source,
            ResolveEcosystemRequest {
                selector: app_selector(first.package_id, GenerationExpectation::Current),
                idempotency_key: key(0x53),
                resolved_at_ms: 7_000,
            },
        )
        .expect("resolve the advanced generation")
        .handle();
    assert_eq!(refreshed.generation, 2);
    assert_eq!(
        refreshed.content_digest,
        second.manifest_digest.into_bytes()
    );
    assert!(matches!(
        plan_authority.inspect_ecosystem_resolution(handle.resolution_id),
        Ok(Some(pinned)) if pinned == handle
    ));

    assert!(matches!(
        plan_authority.resolve_ecosystem_selector(
            &source,
            ResolveEcosystemRequest {
                selector: app_selector(
                    PackageId::from_bytes([0xee; 16]),
                    GenerationExpectation::Current
                ),
                idempotency_key: key(0x54),
                resolved_at_ms: 8_000,
            },
        ),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::EcosystemEntityNotFound { .. }
        ))
    ));
    assert!(matches!(
        plan_authority
            .verify_ecosystem_resolution_current(&source, ReceiptId::from_bytes([0xee; 16])),
        Err(EcosystemResolutionError::Plan(
            PlanStoreError::EcosystemResolutionNotFound(_)
        ))
    ));
    let _ = std::fs::remove_dir_all(&plan_root);
}
