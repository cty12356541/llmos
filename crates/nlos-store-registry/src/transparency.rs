//! Permission and purpose transparency ledger plus cross-version privilege
//! escalation analyzer (`ADR-0027` section 2.3.3; `[TUI-CONSENT-001]`,
//! `[TCB-EGRESS-001]`).

use std::collections::BTreeMap;

pub use nlos_migrate::EffectClass;
use nlos_publisher_trust::{PublisherId, PublisherTrustTier};
use nlos_types::PackageId;

use crate::catalog::{StoreCatalogTrack, StoreChannel};

/// Encodes an [`EffectClass`] into its closed `SQLite` `STRICT` integer code (`1..=4`).
#[must_use]
pub const fn encode_effect_class(effect_class: EffectClass) -> i64 {
    match effect_class {
        EffectClass::ReadOnly => 1,
        EffectClass::Reversible => 2,
        EffectClass::Compensatable => 3,
        EffectClass::Irreversible => 4,
    }
}

/// Decodes a `SQLite` integer column into a closed [`EffectClass`].
#[must_use]
pub const fn decode_effect_class(code: i64) -> Option<EffectClass> {
    match code {
        1 => Some(EffectClass::ReadOnly),
        2 => Some(EffectClass::Reversible),
        3 => Some(EffectClass::Compensatable),
        4 => Some(EffectClass::Irreversible),
        _ => None,
    }
}

/// Monotonic severity rank of an [`EffectClass`] (`ReadOnly < Reversible <
/// Compensatable < Irreversible`).
#[must_use]
pub const fn effect_severity_rank(effect_class: EffectClass) -> u8 {
    match effect_class {
        EffectClass::ReadOnly => 1,
        EffectClass::Reversible => 2,
        EffectClass::Compensatable => 3,
        EffectClass::Irreversible => 4,
    }
}

/// One declared capability and purpose transparency record persisted for a store
/// release (`ADR-0027` section 2.3.3; `[TUI-CONSENT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclaredPermissionEntry {
    /// Scoped capability identifier requested by the package (e.g. `sabi.artifact.write`).
    pub capability_scope: String,
    /// Human-readable purpose string displayed on the `Z4` Trusted UI consent screen.
    pub human_purpose: String,
    /// Declared side-effect reversibility class (`READ_ONLY` .. `IRREVERSIBLE`).
    pub effect_class: EffectClass,
    /// Whether this permission scope opens external network egress (`[TCB-EGRESS-001]`).
    pub network_egress_declared: bool,
    /// Declared execution sandbox profile (for example `dual-layer-wasmtime-strict`).
    pub sandbox_profile: String,
}

/// Durable transparency view for one store release (`[TUI-CONSENT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionTransparencyReport {
    /// Package identity.
    pub package_id: PackageId,
    /// Packed package version.
    pub package_version: u64,
    /// Verified publisher identity.
    pub publisher_id: PublisherId,
    /// Verified publisher trust domain (`PLATFORM_CORE`, `CERTIFIED_PUBLISHER`, `COMMUNITY_SANDBOXED`).
    pub publisher_trust_tier: PublisherTrustTier,
    /// Current release channel (`Candidate`, `Stable`, `Recalled`).
    pub channel: StoreChannel,
    /// Catalog isolation track (`OfficialTrack` vs `CommunityTrack`).
    pub track: StoreCatalogTrack,
    /// Ordered declared permission entries.
    pub permissions: Vec<DeclaredPermissionEntry>,
    /// `true` if any declared permission enables external network egress.
    pub any_network_egress: bool,
    /// `true` if any declared permission carries `EffectClass::Irreversible`.
    pub any_irreversible_effect: bool,
}

/// Records an effect-class upgrade on an existing `capability_scope`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectEscalationRecord {
    /// Affected capability scope.
    pub capability_scope: String,
    /// Effect class declared by the baseline release.
    pub old_effect: EffectClass,
    /// Higher-risk effect class declared by the candidate upgrade release.
    pub new_effect: EffectClass,
}

/// Records a human-purpose mutation on an existing `capability_scope`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PurposeChangeRecord {
    /// Affected capability scope.
    pub capability_scope: String,
    /// Purpose declared by the baseline release.
    pub old_purpose: String,
    /// Modified purpose declared by the candidate upgrade release.
    pub new_purpose: String,
}

/// Records a sandbox-profile mutation on an existing `capability_scope`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SandboxProfileChangeRecord {
    /// Affected capability scope.
    pub capability_scope: String,
    /// Sandbox profile declared by the baseline release.
    pub old_sandbox_profile: String,
    /// Modified sandbox profile declared by the candidate upgrade release.
    pub new_sandbox_profile: String,
}

/// Cross-version permission escalation diff determining whether an application
/// or driver update must block silent upgrade and trigger `[TUI-CONSENT-001]`
/// `Z4` Trusted UI re-authorization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionEscalationDiff {
    /// Brand-new `capability_scope` entries not requested in `old_release`.
    pub added_capabilities: Vec<DeclaredPermissionEntry>,
    /// `capability_scope` entries dropped in `new_release` (attenuation).
    pub removed_capability_scopes: Vec<String>,
    /// Scopes that newly enable external network egress (either flipped `false -> true`
    /// on an existing scope or newly added with `network_egress_declared = true`).
    pub newly_enabled_network_egress: Vec<String>,
    /// Scopes whose `effect_class` increased in severity rank.
    pub escalated_effects: Vec<EffectEscalationRecord>,
    /// Scopes that newly entered `EffectClass::Irreversible` (either upgraded from
    /// a non-irreversible class or newly added as `Irreversible`).
    pub escalated_to_irreversible: Vec<EffectEscalationRecord>,
    /// Existing scopes whose `human_purpose` changed across versions.
    pub purpose_changes: Vec<PurposeChangeRecord>,
    /// Existing scopes whose `sandbox_profile` changed across versions.
    pub sandbox_profile_changes: Vec<SandboxProfileChangeRecord>,
    /// `true` whenever silent upgrade is forbidden and `[TUI-CONSENT-001]` `Z4`
    /// user confirmation must be re-triggered.
    pub requires_tui_reconsent: bool,
}

/// Computes the deterministic cross-version transparency diff between
/// `old_release` and `new_release` (`[TUI-CONSENT-001]`).
///
/// Silent privilege escalation is strictly forbidden: any newly added
/// capability, newly enabled network egress, effect-class escalation (especially
/// toward `EffectClass::Irreversible`), purpose change, or sandbox profile
/// change sets `requires_tui_reconsent = true`.
#[must_use]
pub fn diff_permission_transparency(
    old_release: &[DeclaredPermissionEntry],
    new_release: &[DeclaredPermissionEntry],
) -> PermissionEscalationDiff {
    let mut old_by_scope = BTreeMap::new();
    for entry in old_release {
        old_by_scope.insert(entry.capability_scope.as_str(), entry);
    }

    let mut new_by_scope = BTreeMap::new();
    for entry in new_release {
        new_by_scope.insert(entry.capability_scope.as_str(), entry);
    }

    let mut added_capabilities = Vec::new();
    let mut removed_capability_scopes = Vec::new();
    let mut newly_enabled_network_egress = Vec::new();
    let mut escalated_effects = Vec::new();
    let mut escalated_to_irreversible = Vec::new();
    let mut purpose_changes = Vec::new();
    let mut sandbox_profile_changes = Vec::new();

    for (scope, old_entry) in &old_by_scope {
        if !new_by_scope.contains_key(scope) {
            removed_capability_scopes.push((*scope).to_owned());
            let _ = old_entry;
        }
    }

    for (scope, new_entry) in &new_by_scope {
        if let Some(old_entry) = old_by_scope.get(scope) {
            if !old_entry.network_egress_declared && new_entry.network_egress_declared {
                newly_enabled_network_egress.push((*scope).to_owned());
            }
            if effect_severity_rank(new_entry.effect_class)
                > effect_severity_rank(old_entry.effect_class)
            {
                let record = EffectEscalationRecord {
                    capability_scope: (*scope).to_owned(),
                    old_effect: old_entry.effect_class,
                    new_effect: new_entry.effect_class,
                };
                if new_entry.effect_class == EffectClass::Irreversible {
                    escalated_to_irreversible.push(record.clone());
                }
                escalated_effects.push(record);
            }
            if old_entry.human_purpose != new_entry.human_purpose {
                purpose_changes.push(PurposeChangeRecord {
                    capability_scope: (*scope).to_owned(),
                    old_purpose: old_entry.human_purpose.clone(),
                    new_purpose: new_entry.human_purpose.clone(),
                });
            }
            if old_entry.sandbox_profile != new_entry.sandbox_profile {
                sandbox_profile_changes.push(SandboxProfileChangeRecord {
                    capability_scope: (*scope).to_owned(),
                    old_sandbox_profile: old_entry.sandbox_profile.clone(),
                    new_sandbox_profile: new_entry.sandbox_profile.clone(),
                });
            }
        } else {
            added_capabilities.push((*new_entry).clone());
            if new_entry.network_egress_declared {
                newly_enabled_network_egress.push((*scope).to_owned());
            }
            if new_entry.effect_class == EffectClass::Irreversible {
                escalated_to_irreversible.push(EffectEscalationRecord {
                    capability_scope: (*scope).to_owned(),
                    old_effect: EffectClass::ReadOnly,
                    new_effect: EffectClass::Irreversible,
                });
            }
        }
    }

    let requires_tui_reconsent = !added_capabilities.is_empty()
        || !newly_enabled_network_egress.is_empty()
        || !escalated_effects.is_empty()
        || !escalated_to_irreversible.is_empty()
        || !purpose_changes.is_empty()
        || !sandbox_profile_changes.is_empty();

    PermissionEscalationDiff {
        added_capabilities,
        removed_capability_scopes,
        newly_enabled_network_egress,
        escalated_effects,
        escalated_to_irreversible,
        purpose_changes,
        sandbox_profile_changes,
        requires_tui_reconsent,
    }
}
