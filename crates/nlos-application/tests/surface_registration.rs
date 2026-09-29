//! W32-F / B2-2 manifest half: the additive `surfaces` declaration
//! segment and its durable registration face (schema v8).
//!
//! - **Additivity golden**: a surface registration moves nothing in the
//!   pre-existing durable state — the application row (generation,
//!   status, timestamps) is bitwise untouched, and every legacy face
//!   (inspect/list) reads the same facts with the segment present.
//! - Registration is exactly-once by idempotency key: byte-equal
//!   replay, typed conflict on any shape drift (segment content,
//!   registrant, timestamp, bound digest).
//! - The content binding: a declaration naming a manifest digest other
//!   than the current installation's is a typed refusal, never a
//!   silent rebind; a generation advance re-opens admission for the
//!   same surface identities (the generation fence).
//! - Inspect is the presenter's discovery face: registration order,
//!   then declaration order, bitwise declared content.

mod support;

use nlos_application::{
    ApplicationAuthorityError, ApplicationStatus, InstallApplicationRequest, InstallDecision,
    PackageSurfaceDeclaration, PackageSurfaceKind, RegisterSurfacesDecision,
    RegisterSurfacesRequest, SurfaceSegmentError,
};
use nlos_types::{IdempotencyKey, PackageId, PrincipalId};

use support::{TestStack, open_authority};

const VERIFY_AT_MS: u64 = 6_000;
const INSTALL_AT_MS: u64 = 7_000;
const REGISTER_AT_MS: u64 = 7_500;

fn declaration(
    id: [u8; 16],
    kind: PackageSurfaceKind,
    title: &str,
    entry_name: Option<&str>,
) -> PackageSurfaceDeclaration {
    PackageSurfaceDeclaration {
        surface_id: id,
        kind,
        title: title.to_string(),
        entry_name: entry_name.map(str::to_string),
    }
}

fn sample_segment() -> Vec<PackageSurfaceDeclaration> {
    vec![
        declaration([0x01; 16], PackageSurfaceKind::Window, "样板主窗口", None),
        declaration(
            [0x02; 16],
            PackageSurfaceKind::Panel,
            "侧栏面板",
            Some("assets/panel.bin"),
        ),
    ]
}

fn register_request(
    package_id: PackageId,
    manifest_digest: nlos_artifact::ContentDigest,
    surfaces: Vec<PackageSurfaceDeclaration>,
    key: u8,
    at_ms: u64,
) -> RegisterSurfacesRequest {
    RegisterSurfacesRequest {
        package_id,
        declared_manifest_digest: manifest_digest,
        surfaces,
        registrant_principal: PrincipalId::from_bytes([0x71; 16]),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        registered_at_ms: at_ms,
    }
}

/// install → register → inspect: the durable chain the desktop
/// presenter consumes, plus the additivity golden (the application row
/// is untouched by a surface registration).
#[test]
fn surfaces_register_replay_and_inspect_read_back_declared_content() {
    let stack = TestStack::new("w32f-register", 0xA1);
    let authority = open_authority(stack.root.root());
    let receipt = stack.verify_package(
        0x60,
        1,
        IdempotencyKey::from_bytes([0x91; 16]),
        VERIFY_AT_MS,
    );
    let installed = match authority
        .install_application(
            &stack.artifacts,
            InstallApplicationRequest {
                package_verification_receipt_id: receipt.receipt_id,
                idempotency_key: IdempotencyKey::from_bytes([0x92; 16]),
                installed_at_ms: INSTALL_AT_MS,
            },
        )
        .expect("install must succeed")
    {
        InstallDecision::Installed(receipt) => receipt,
        InstallDecision::Replayed(receipt) => panic!("fresh key cannot replay, got {receipt:?}"),
    };

    let registered = match authority
        .register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            sample_segment(),
            0x93,
            REGISTER_AT_MS,
        ))
        .expect("surface registration must succeed")
    {
        RegisterSurfacesDecision::Registered(receipts) => receipts,
        RegisterSurfacesDecision::Replayed(receipts) => {
            panic!("fresh key cannot replay, got {receipts:?}")
        }
    };
    assert_eq!(registered.len(), 2);
    assert_eq!(registered[0].surface_id, [0x01; 16]);
    assert_eq!(registered[0].surface_index, 0);
    assert_eq!(registered[0].kind, PackageSurfaceKind::Window);
    assert_eq!(registered[0].title, "样板主窗口");
    assert_eq!(registered[0].entry_name, None);
    assert_eq!(
        registered[0].application_id, installed.application_id,
        "registration binds the authority-derived application identity"
    );
    assert_eq!(registered[0].application_generation.get(), 1);
    assert_eq!(
        registered[0].package_manifest_digest, installed.package_manifest_digest,
        "the durable fact carries the installed content binding"
    );
    assert_eq!(registered[1].surface_id, [0x02; 16]);
    assert_eq!(registered[1].surface_index, 1);
    assert_eq!(
        registered[1].entry_name.as_deref(),
        Some("assets/panel.bin")
    );

    // Additivity golden: the application row moved nothing.
    let application = authority
        .inspect_application(receipt.package_id)
        .expect("inspect application")
        .expect("application exists");
    assert_eq!(application.current_installation_generation.get(), 1);
    assert_eq!(application.status, ApplicationStatus::Installed);
    assert_eq!(application.updated_at_ms, INSTALL_AT_MS);

    // Byte-equal durable replay under the same key.
    let replayed = match authority
        .register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            sample_segment(),
            0x93,
            REGISTER_AT_MS,
        ))
        .expect("replay must succeed")
    {
        RegisterSurfacesDecision::Replayed(receipts) => receipts,
        RegisterSurfacesDecision::Registered(receipts) => {
            panic!("expected replay, got fresh registration {receipts:?}")
        }
    };
    assert_eq!(registered, replayed, "byte-identical durable replay");

    // The discovery face reads exactly the declared set back.
    let inspected = authority
        .inspect_surfaces(receipt.package_id)
        .expect("inspect surfaces");
    assert_eq!(inspected, registered);
}

/// The same idempotency key with any drifted shape is a typed conflict:
/// segment content, registrant, timestamp, and bound digest all
/// participate.
#[test]
fn surface_replay_conflicts_on_every_shape_dimension() {
    let stack = TestStack::new("w32f-conflict", 0xA2);
    let authority = open_authority(stack.root.root());
    let receipt = stack.verify_package(
        0x61,
        1,
        IdempotencyKey::from_bytes([0x91; 16]),
        VERIFY_AT_MS,
    );
    let installed = support::installed(
        &authority,
        &stack.artifacts,
        receipt.receipt_id,
        0x92,
        INSTALL_AT_MS,
    );

    authority
        .register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            sample_segment(),
            0x93,
            REGISTER_AT_MS,
        ))
        .expect("first registration");

    let mut drifted_title = register_request(
        receipt.package_id,
        installed.package_manifest_digest,
        sample_segment(),
        0x93,
        REGISTER_AT_MS,
    );
    drifted_title.surfaces[0].title = "改过的标题".to_string();
    assert!(matches!(
        authority.register_surfaces(&drifted_title),
        Err(ApplicationAuthorityError::IdempotencyConflict)
    ));

    let mut drifted_order = register_request(
        receipt.package_id,
        installed.package_manifest_digest,
        sample_segment(),
        0x93,
        REGISTER_AT_MS,
    );
    drifted_order.surfaces.swap(0, 1);
    assert!(
        matches!(
            authority.register_surfaces(&drifted_order),
            Err(ApplicationAuthorityError::IdempotencyConflict)
        ),
        "declaration order participates in the request shape"
    );

    let mut drifted_registrant = register_request(
        receipt.package_id,
        installed.package_manifest_digest,
        sample_segment(),
        0x93,
        REGISTER_AT_MS,
    );
    drifted_registrant.registrant_principal = PrincipalId::from_bytes([0x72; 16]);
    assert!(matches!(
        authority.register_surfaces(&drifted_registrant),
        Err(ApplicationAuthorityError::IdempotencyConflict)
    ));

    let mut drifted_digest = register_request(
        receipt.package_id,
        installed.package_manifest_digest,
        sample_segment(),
        0x93,
        REGISTER_AT_MS,
    );
    drifted_digest.declared_manifest_digest =
        nlos_artifact::ContentDigest::of_bytes(b"other-content");
    assert!(matches!(
        authority.register_surfaces(&drifted_digest),
        Err(ApplicationAuthorityError::IdempotencyConflict)
    ));
}

/// Fail-closed refusals: unknown package, stale/foreign manifest digest,
/// malformed segment, duplicate identity at the same generation, and a
/// timestamp preceding the application's last update.
#[test]
fn surface_registration_refuses_typed_failures_with_zero_durable_state() {
    let stack = TestStack::new("w32f-negative", 0xA3);
    let authority = open_authority(stack.root.root());
    let receipt = stack.verify_package(
        0x62,
        1,
        IdempotencyKey::from_bytes([0x91; 16]),
        VERIFY_AT_MS,
    );
    let installed = support::installed(
        &authority,
        &stack.artifacts,
        receipt.receipt_id,
        0x92,
        INSTALL_AT_MS,
    );

    // Unknown package: nothing was ever installed.
    assert!(matches!(
        authority.register_surfaces(&register_request(
            PackageId::from_bytes([0xEE; 16]),
            installed.package_manifest_digest,
            sample_segment(),
            0x93,
            REGISTER_AT_MS,
        )),
        Err(ApplicationAuthorityError::ApplicationNotFound { .. })
    ));

    // Manifest digest mismatch: the declaration names other content.
    assert!(matches!(
        authority.register_surfaces(&register_request(
            receipt.package_id,
            nlos_artifact::ContentDigest::of_bytes(b"stale-manifest"),
            sample_segment(),
            0x93,
            REGISTER_AT_MS,
        )),
        Err(ApplicationAuthorityError::SurfaceManifestMismatch { .. })
    ));

    // Malformed segment: empty.
    assert!(matches!(
        authority.register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            Vec::new(),
            0x93,
            REGISTER_AT_MS,
        )),
        Err(ApplicationAuthorityError::SurfaceSegment(
            SurfaceSegmentError::Empty
        ))
    ));

    // Malformed segment: duplicate identity within the declaration.
    let duplicated = vec![
        declaration([0x05; 16], PackageSurfaceKind::Window, "a", None),
        declaration([0x05; 16], PackageSurfaceKind::Panel, "b", None),
    ];
    assert!(matches!(
        authority.register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            duplicated,
            0x93,
            REGISTER_AT_MS,
        )),
        Err(ApplicationAuthorityError::SurfaceSegment(
            SurfaceSegmentError::DuplicateSurfaceId
        ))
    ));

    // Timestamp preceding the application row's last update.
    assert!(matches!(
        authority.register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            sample_segment(),
            0x93,
            INSTALL_AT_MS - 1,
        )),
        Err(ApplicationAuthorityError::RegistrationPrecedesLastUpdate { .. })
    ));

    // Every refusal above left zero durable state.
    assert!(
        authority
            .inspect_surfaces(receipt.package_id)
            .expect("inspect after refusals")
            .is_empty()
    );

    // First valid registration, then the same-generation duplicate guard.
    authority
        .register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            sample_segment(),
            0x93,
            REGISTER_AT_MS,
        ))
        .expect("first registration");
}

/// A surface identity already registered at the current generation is a
/// typed refusal under a fresh key; the refused key adds nothing.
#[test]
fn surface_identity_is_unique_within_one_generation() {
    let stack = TestStack::new("w32f-duplicate", 0xA7);
    let authority = open_authority(stack.root.root());
    let receipt = stack.verify_package(
        0x65,
        1,
        IdempotencyKey::from_bytes([0x91; 16]),
        VERIFY_AT_MS,
    );
    let installed = support::installed(
        &authority,
        &stack.artifacts,
        receipt.receipt_id,
        0x92,
        INSTALL_AT_MS,
    );
    authority
        .register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            sample_segment(),
            0x93,
            REGISTER_AT_MS,
        ))
        .expect("first registration");
    assert!(matches!(
        authority.register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            sample_segment(),
            0x94,
            REGISTER_AT_MS,
        )),
        Err(ApplicationAuthorityError::SurfaceAlreadyRegistered { .. })
    ));
    assert_eq!(
        authority
            .inspect_surfaces(receipt.package_id)
            .expect("inspect")
            .len(),
        2,
        "the refused fresh key added nothing"
    );
}

/// A generation advance (content update) re-opens admission: the same
/// surface identities register at generation 2 against the new manifest
/// digest, and inspect keeps both generations as durable facts
/// (registration order, declaration order within a registration).
#[test]
fn generation_advance_reopens_surface_admission_and_keeps_history() {
    let stack = TestStack::new("w32f-generation", 0xA4);
    let authority = open_authority(stack.root.root());
    let first = stack.verify_package(
        0x63,
        1,
        IdempotencyKey::from_bytes([0x91; 16]),
        VERIFY_AT_MS,
    );
    let installed = support::installed(
        &authority,
        &stack.artifacts,
        first.receipt_id,
        0x92,
        INSTALL_AT_MS,
    );
    authority
        .register_surfaces(&register_request(
            first.package_id,
            installed.package_manifest_digest,
            sample_segment(),
            0x93,
            REGISTER_AT_MS,
        ))
        .expect("generation-1 registration");

    let second = stack.verify_package(0x63, 2, IdempotencyKey::from_bytes([0x95; 16]), 8_000);
    let updated = support::updated(
        &authority,
        &stack.artifacts,
        first.package_id,
        second.receipt_id,
        0x96,
        8_500,
    );
    assert_eq!(updated.installation_generation.get(), 2);
    assert_ne!(
        updated.package_manifest_digest, installed.package_manifest_digest,
        "the update changed the installed content"
    );

    // Declaring the old generation's digest against the new content is
    // the typed mismatch, never a silent rebind.
    assert!(matches!(
        authority.register_surfaces(&register_request(
            first.package_id,
            installed.package_manifest_digest,
            sample_segment(),
            0x97,
            9_000,
        )),
        Err(ApplicationAuthorityError::SurfaceManifestMismatch { .. })
    ));

    let reregistered = match authority
        .register_surfaces(&register_request(
            first.package_id,
            updated.package_manifest_digest,
            vec![declaration(
                [0x01; 16],
                PackageSurfaceKind::Window,
                "样板主窗口 v2",
                None,
            )],
            0x97,
            9_000,
        ))
        .expect("generation-2 re-declaration must succeed")
    {
        RegisterSurfacesDecision::Registered(receipts) => receipts,
        RegisterSurfacesDecision::Replayed(receipts) => {
            panic!("fresh key cannot replay, got {receipts:?}")
        }
    };
    assert_eq!(reregistered.len(), 1);
    assert_eq!(reregistered[0].application_generation.get(), 2);
    assert_eq!(
        reregistered[0].package_manifest_digest, updated.package_manifest_digest,
        "the re-declaration binds the generation-2 content"
    );

    // History: both generations' registrations remain durable facts in
    // registration order, declaration order within each.
    let inspected = authority
        .inspect_surfaces(first.package_id)
        .expect("inspect surfaces");
    assert_eq!(inspected.len(), 3);
    assert_eq!(inspected[0].surface_id, [0x01; 16]);
    assert_eq!(inspected[0].application_generation.get(), 1);
    assert_eq!(inspected[1].surface_id, [0x02; 16]);
    assert_eq!(inspected[1].application_generation.get(), 1);
    assert_eq!(inspected[2].surface_id, [0x01; 16]);
    assert_eq!(inspected[2].application_generation.get(), 2);
    assert_eq!(inspected[2].title, "样板主窗口 v2");
}

/// Uninstall fences further registrations (typed refusal; the DDL
/// state-bounds guard backs the same rule), and the durable surface
/// facts survive the uninstall as evidence.
#[test]
fn uninstalled_application_refuses_surface_registration_and_keeps_facts() {
    let stack = TestStack::new("w32f-uninstalled", 0xA8);
    let authority = open_authority(stack.root.root());
    let receipt = stack.verify_package(
        0x66,
        1,
        IdempotencyKey::from_bytes([0x91; 16]),
        VERIFY_AT_MS,
    );
    let installed = support::installed(
        &authority,
        &stack.artifacts,
        receipt.receipt_id,
        0x92,
        INSTALL_AT_MS,
    );
    authority
        .register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            sample_segment(),
            0x93,
            REGISTER_AT_MS,
        ))
        .expect("registration before uninstall");

    support::uninstalled(&authority, receipt.package_id, 0x98, 9_500);
    assert!(matches!(
        authority.register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            vec![declaration(
                [0x03; 16],
                PackageSurfaceKind::Panel,
                "迟到的面板",
                None,
            )],
            0x99,
            9_600,
        )),
        Err(ApplicationAuthorityError::ApplicationUninstalled { .. })
    ));
    // The durable facts survive the uninstall as evidence.
    assert_eq!(
        authority
            .inspect_surfaces(receipt.package_id)
            .expect("inspect after uninstall")
            .len(),
        2,
        "the pre-uninstall registration's declared rows remain durable facts"
    );
}

/// A disabled application refuses registrations with the same typed
/// refusal the other registration faces use.
#[test]
fn disabled_application_refuses_surface_registration() {
    let stack = TestStack::new("w32f-disabled", 0xA5);
    let authority = open_authority(stack.root.root());
    let receipt = stack.verify_package(
        0x64,
        1,
        IdempotencyKey::from_bytes([0x91; 16]),
        VERIFY_AT_MS,
    );
    let installed = support::installed(
        &authority,
        &stack.artifacts,
        receipt.receipt_id,
        0x92,
        INSTALL_AT_MS,
    );
    support::disabled(&authority, receipt.package_id, 0x93, REGISTER_AT_MS);
    assert!(matches!(
        authority.register_surfaces(&register_request(
            receipt.package_id,
            installed.package_manifest_digest,
            sample_segment(),
            0x94,
            REGISTER_AT_MS,
        )),
        Err(ApplicationAuthorityError::ApplicationDisabled { .. })
    ));
}

/// An unknown application lists as empty — a legitimate read outcome.
#[test]
fn inspect_surfaces_of_unknown_package_lists_empty() {
    let stack = TestStack::new("w32f-unknown", 0xA6);
    let authority = open_authority(stack.root.root());
    assert!(
        authority
            .inspect_surfaces(PackageId::from_bytes([0xEF; 16]))
            .expect("inspect unknown package")
            .is_empty()
    );
}
