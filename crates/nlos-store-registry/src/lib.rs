//! Store governance catalog, content-addressed `OCI` `CAS` verifier, opaque
//! `Ed25519`-signed entitlement token verification seam, and three-tier
//! malicious package recall ledger (`ADR-0027` section 2.3; `[PKG-OCI-001]`,
//! `[PKG-MULTI-001]`, `[PKG-APPID-001]`, `[TUI-CONSENT-001]`,
//! `[CAP-REVOKE-001]`, `[NLOS-DEFER-001]`).
//!
//! Architectural separation (`[PKG-OCI-001]`):
//! - [`LocalOciCasStore`] owns content-addressed `OCI` blob and layout
//!   integrity (`SHA-256` + byte length + canonical `oci_manifest_digest`).
//!   Writing layers or manifests into `CAS` never creates a store listing or
//!   installation permission.
//! - [`SqliteStoreRegistry`] owns durable (`WAL + FULL`) governance state:
//!   multi-component release onboarding via [`nlos_publisher_trust::PublisherTrustVerifier`],
//!   `ApplicationId -> PublisherId` anti-hijack enforcement, `OfficialTrack` vs
//!   `CommunityTrack` channel isolation, four-gate [`SignedCertificationAttestation`]
//!   promotion (`Candidate -> Stable`), cross-version [`PermissionEscalationDiff`]
//!   transparency checks, offline [`SignedEntitlementToken`] verification and
//!   seat accounting, and three-tier [`SignedRecallAdvisory`] capability
//!   revocation and installation quarantine.

#![forbid(unsafe_code)]

pub mod catalog;
pub mod entitlement;
pub mod error;
pub mod oci_cas;
pub mod recall;
pub mod registry;
mod schema;
pub mod transparency;

pub use catalog::{
    CERTIFICATION_ATTESTATION_SIGNING_DOMAIN, RELEASE_SUBMISSION_SIGNING_DOMAIN,
    REQUIRED_CERTIFICATION_GATES_MASK, SignedCertificationAttestation, StoreCatalogTrack,
    StoreCertificationPromotionReceipt, StoreChannel, StoreComponentKind, StoreComponentSlot,
    StorePromotionDecision, StoreReleaseRecord, StoreReleaseSubmission,
    UnsignedStoreReleaseSubmission, compute_release_submission_digest,
};
pub use entitlement::{
    ENTITLEMENT_TOKEN_SIGNING_DOMAIN, EntitlementGrantReceipt, EntitlementRedeemDecision,
    RedeemEntitlementRequest, SignedEntitlementToken, compute_entitlement_token_digest,
};
pub use error::{OciCasError, StoreRegistryError};
pub use oci_cas::{
    LocalOciCasStore, NLOS_OCI_CONFIG_MEDIA_TYPE, OCI_IMAGE_MANIFEST_SCHEMA_V2, OciLayerDescriptor,
    OciPackageLayout, compute_canonical_oci_manifest_digest, compute_sha256,
};
pub use recall::{
    CapabilityRevocationAuthorizer, DirectEd25519RevocationAuthorizer,
    GrantedCapabilityBindingRecord, LocalLaunchDisposition, RECALL_ADVISORY_SIGNING_DOMAIN,
    RecallApplyDecision, RecallEnforcementAuthorities, RecallExecutionReceipt, RecallSeverity,
    RegisterGrantedCapabilityRequest, SignedRecallAdvisory, compute_recall_advisory_digest,
};
pub use registry::{SqliteStoreRegistry, StoreGovernanceAnchors};
pub use transparency::{
    DeclaredPermissionEntry, EffectClass, EffectEscalationRecord, PermissionEscalationDiff,
    PermissionTransparencyReport, PurposeChangeRecord, SandboxProfileChangeRecord,
    decode_effect_class, diff_permission_transparency, effect_severity_rank, encode_effect_class,
};

/// Major schema generation of the `nlos-store-registry` governance catalog.
pub const STORE_REGISTRY_SCHEMA_MAJOR: u16 = 1;

#[cfg(test)]
mod tests {
    use nlos_migrate::ComponentRole;

    use super::{
        DeclaredPermissionEntry, EffectClass, OCI_IMAGE_MANIFEST_SCHEMA_V2, OciLayerDescriptor,
        RecallSeverity, STORE_REGISTRY_SCHEMA_MAJOR, StoreCatalogTrack, StoreChannel,
        StoreComponentKind, compute_canonical_oci_manifest_digest, decode_effect_class,
        diff_permission_transparency, encode_effect_class,
    };
    use nlos_publisher_trust::PublisherTrustTier;

    #[test]
    fn store_registry_schema_major_is_v1() {
        assert_eq!(STORE_REGISTRY_SCHEMA_MAJOR, 1);
    }

    #[test]
    fn closed_discriminant_codecs_roundtrip_and_fail_closed() {
        for channel in [
            StoreChannel::Candidate,
            StoreChannel::Stable,
            StoreChannel::Recalled,
        ] {
            assert_eq!(
                StoreChannel::decode(i64::from(channel.encode())),
                Some(channel)
            );
        }
        assert_eq!(StoreChannel::decode(0), None);
        assert_eq!(StoreChannel::decode(4), None);

        for track in [
            StoreCatalogTrack::OfficialTrack,
            StoreCatalogTrack::CommunityTrack,
        ] {
            assert_eq!(
                StoreCatalogTrack::decode(i64::from(track.encode())),
                Some(track)
            );
        }
        assert_eq!(StoreCatalogTrack::decode(0), None);
        assert_eq!(StoreCatalogTrack::decode(3), None);
        assert_eq!(
            StoreCatalogTrack::for_trust_tier(PublisherTrustTier::PlatformCore),
            StoreCatalogTrack::OfficialTrack
        );
        assert_eq!(
            StoreCatalogTrack::for_trust_tier(PublisherTrustTier::CertifiedPublisher),
            StoreCatalogTrack::OfficialTrack
        );
        assert_eq!(
            StoreCatalogTrack::for_trust_tier(PublisherTrustTier::CommunitySandboxed),
            StoreCatalogTrack::CommunityTrack
        );

        for kind in [
            StoreComponentKind::Executable,
            StoreComponentKind::Driver,
            StoreComponentKind::UiSurface,
            StoreComponentKind::BackgroundService,
            StoreComponentKind::ReadOnlySeedTemplate,
        ] {
            assert_eq!(
                StoreComponentKind::decode(i64::from(kind.encode())),
                Some(kind)
            );
        }
        assert_eq!(StoreComponentKind::decode(0), None);
        assert_eq!(StoreComponentKind::decode(6), None);
        assert_eq!(
            StoreComponentKind::from_manifest_role(ComponentRole::Driver),
            StoreComponentKind::Driver
        );
        assert_eq!(
            StoreComponentKind::from_manifest_role(ComponentRole::SeedTemplate),
            StoreComponentKind::ReadOnlySeedTemplate
        );

        for severity in [
            RecallSeverity::WarnNewInstall,
            RecallSeverity::BlockLaunchAndRevokeCap,
            RecallSeverity::QuarantineInstallation,
        ] {
            assert_eq!(
                RecallSeverity::decode(i64::from(severity.encode())),
                Some(severity)
            );
        }
        assert_eq!(RecallSeverity::decode(0), None);
        assert_eq!(RecallSeverity::decode(4), None);

        for effect in [
            EffectClass::ReadOnly,
            EffectClass::Reversible,
            EffectClass::Compensatable,
            EffectClass::Irreversible,
        ] {
            assert_eq!(
                decode_effect_class(encode_effect_class(effect)),
                Some(effect)
            );
        }
        assert_eq!(decode_effect_class(0), None);
        assert_eq!(decode_effect_class(5), None);
    }

    #[test]
    fn canonical_oci_manifest_digest_is_order_and_content_sensitive() {
        let config =
            OciLayerDescriptor::from_layer_bytes("application/vnd.nlos.config+json", b"{}")
                .expect("valid config");
        let layer_a =
            OciLayerDescriptor::from_layer_bytes("application/vnd.nlos.wasm.v1", b"wasm-a")
                .expect("valid layer a");
        let layer_b =
            OciLayerDescriptor::from_layer_bytes("application/vnd.nlos.wasm.v1", b"wasm-b")
                .expect("valid layer b");

        let d1 = compute_canonical_oci_manifest_digest(
            OCI_IMAGE_MANIFEST_SCHEMA_V2,
            &config,
            &[layer_a.clone(), layer_b.clone()],
        );
        let d2 = compute_canonical_oci_manifest_digest(
            OCI_IMAGE_MANIFEST_SCHEMA_V2,
            &config,
            &[layer_b, layer_a],
        );
        assert_ne!(d1, d2);
    }

    #[test]
    fn permission_diff_allows_pure_attenuation_without_tui_reconsent() {
        let v1 = vec![
            DeclaredPermissionEntry {
                capability_scope: "sabi.artifact.read".to_owned(),
                human_purpose: "Read user notes".to_owned(),
                effect_class: EffectClass::Reversible,
                network_egress_declared: true,
                sandbox_profile: "wasm-strict-v1".to_owned(),
            },
            DeclaredPermissionEntry {
                capability_scope: "sabi.clipboard.read".to_owned(),
                human_purpose: "Paste snippets".to_owned(),
                effect_class: EffectClass::ReadOnly,
                network_egress_declared: false,
                sandbox_profile: "wasm-strict-v1".to_owned(),
            },
        ];
        let v2 = vec![DeclaredPermissionEntry {
            capability_scope: "sabi.artifact.read".to_owned(),
            human_purpose: "Read user notes".to_owned(),
            effect_class: EffectClass::ReadOnly,
            network_egress_declared: false,
            sandbox_profile: "wasm-strict-v1".to_owned(),
        }];

        let diff = diff_permission_transparency(&v1, &v2);
        assert!(!diff.requires_tui_reconsent);
        assert_eq!(diff.removed_capability_scopes, vec!["sabi.clipboard.read"]);
        assert_eq!(diff.added_capabilities, Vec::new());
        assert_eq!(diff.newly_enabled_network_egress, Vec::<String>::new());
        assert_eq!(diff.escalated_effects, Vec::new());
    }
}
