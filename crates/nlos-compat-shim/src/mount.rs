//! Per-package compatibility shim mount evaluator (`CompatShimMountDecision`)
//! implementing `ADR-0027` section 2.2 dynamic out-of-core mounting and
//! fail-closed `nlos-migrate` rejection guidance.

use std::error::Error;
use std::fmt;

use nlos_schema::{
    DeprecationManifest, DeprecationManifestError, MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
    SABI_ENVELOPE_SCHEMA, SymbolAuditVerdict, canonical_deprecation_manifest, sabi,
};

use crate::driver_shim::{DriverProtocolCompatShim, LegacyDriverWireFormat};
use crate::manifest::{ManifestCompatibilityError, PackageCompatibilitySpec};
use crate::sabi_adapter::SabiEpochAdapterCell;

/// Host-side compatibility mounting policy applied when installing or launching
/// a `llmos.package` artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatShimMountPolicy {
    /// Current canonical SABI major epoch served natively by core authorities (`N`).
    pub host_epoch: u32,
    /// Rolling backward-compatibility window (`>= 2`, default `2` for `N-1`/`N-2`).
    pub max_backward_epochs: u32,
    /// Current canonical `DriverProvider` ABI major epoch (`N`).
    pub host_driver_abi_epoch: u32,
    /// Critical extension IDs supported by this host (`[COMPAT-VER-002]`).
    pub known_critical_extensions: Vec<u32>,
    /// Optional feature allowlist; when non-empty, `required_features` must be a subset.
    pub supported_features: Vec<String>,
    /// Default wire format assumed when mounting a legacy `DriverProtocolCompatShim`.
    pub legacy_driver_wire_format: LegacyDriverWireFormat,
}

impl Default for CompatShimMountPolicy {
    fn default() -> Self {
        Self::new(1)
    }
}

impl CompatShimMountPolicy {
    /// Constructs a default policy for `host_epoch` with statutory `N / N-1 / N-2` window (`2`).
    #[must_use]
    pub const fn new(host_epoch: u32) -> Self {
        Self {
            host_epoch,
            max_backward_epochs: MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
            host_driver_abi_epoch: host_epoch,
            known_critical_extensions: Vec::new(),
            supported_features: Vec::new(),
            legacy_driver_wire_format: LegacyDriverWireFormat::HistoricalDriverProviderProto,
        }
    }

    /// Attaches receiver-supported `critical_extension_ids`.
    #[must_use]
    pub fn with_known_critical_extensions(mut self, extensions: Vec<u32>) -> Self {
        self.known_critical_extensions = extensions;
        self
    }

    /// Attaches host-supported `required_features` tokens.
    #[must_use]
    pub fn with_supported_features(mut self, features: Vec<String>) -> Self {
        self.supported_features = features;
        self
    }

    /// Overrides the legacy driver wire format (`StageBMinicbor` vs `HistoricalDriverProviderProto`).
    #[must_use]
    pub const fn with_legacy_driver_wire_format(mut self, format: LegacyDriverWireFormat) -> Self {
        self.legacy_driver_wire_format = format;
        self
    }
}

/// Out-of-core unprivileged compatibility cell bundle dynamically mounted for an
/// `N-1` or `N-2` package (`ADR-0027` section 2.2).
#[derive(Clone, Debug)]
pub struct MountedCompatShim {
    /// Mounted `SabiEpochAdapterCell` when `1 <= host_epoch - target_sabi_epoch <= max_backward_epochs`.
    pub sabi_adapter: Option<SabiEpochAdapterCell>,
    /// Mounted `DriverProtocolCompatShim` when `driver_abi_epoch` requires protocol adaptation.
    pub driver_shim: Option<DriverProtocolCompatShim>,
    /// Retained `non_critical_extensions` forwarded verbatim (`[COMPAT-VER-002]`).
    pub retained_non_critical_extensions: Vec<u32>,
}

/// Deterministic mounting verdict produced for a [`PackageCompatibilitySpec`].
#[derive(Clone, Debug)]
pub enum CompatShimMountDecision {
    /// `target_sabi_epoch == host_epoch` (and no legacy driver shim needed):
    /// direct pass-through to core authorities with zero shim allocation or hot-path overhead.
    NativeDirect {
        /// Active host SABI epoch (`N`).
        host_epoch: u32,
        /// Package's declared SABI minor version.
        target_sabi_minor: u32,
        /// Non-critical extensions preserved verbatim (`[COMPAT-VER-002]`).
        retained_non_critical_extensions: Vec<u32>,
    },
    /// `1 <= host_epoch - target_sabi_epoch <= max_backward_epochs` (`N-1` / `N-2`):
    /// dynamically mounts an out-of-core [`MountedCompatShim`].
    MountOutOfCore(MountedCompatShim),
}

/// Fail-closed typed mount rejection mapped deterministically to
/// `SabiErrorCode::AbiVersion` + `RetryDirective::DoNotRetry` + `nlos-migrate` guidance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CompatMountRejection {
    /// Structural `PackageCompatibilitySpec` bounds failed validation.
    InvalidManifestSpec(ManifestCompatibilityError),
    /// Host policy had `host_epoch == 0` or `max_backward_epochs < 2`.
    InvalidHostPolicy {
        host_epoch: u32,
        max_backward_epochs: u32,
    },
    /// `target_sabi_epoch == 0` is invalid.
    ZeroTargetSabiEpoch,
    /// `target_sabi_epoch > host_epoch` requires a newer NLOS host.
    FutureTargetSabiEpoch { target_epoch: u32, host_epoch: u32 },
    /// `target_sabi_epoch <= host_epoch - 3` has completed its grace window and sunset.
    SunsetTargetSabiEpoch {
        target_epoch: u32,
        host_epoch: u32,
        max_backward_epochs: u32,
    },
    /// `driver_abi_epoch` was `0`, `> host_driver_abi_epoch`, or sunset beyond `max_backward_epochs`.
    IncompatibleDriverAbiEpoch {
        driver_epoch: u32,
        host_driver_epoch: u32,
        max_backward_epochs: u32,
    },
    /// Package required an unknown or zero `critical_extension` ID (`[COMPAT-VER-002]`).
    UnsupportedCriticalExtension { extension_id: u32 },
    /// Package required a feature unsupported by the host or retired as `SUNSET`/`RESERVED`.
    UnsupportedOrSunsetFeature {
        feature: String,
        reason: &'static str,
    },
    /// Embedded or supplied `DeprecationManifest` failed validation.
    InvalidDeprecationManifest(DeprecationManifestError),
}

impl CompatMountRejection {
    /// Deterministic wire failure (`SabiErrorCode::AbiVersion` + `RetryDirective::DoNotRetry`).
    #[must_use]
    pub fn to_sabi_failure(&self) -> sabi::v1::SabiFailure {
        let safe_message = match self {
            Self::InvalidManifestSpec(_) => {
                "package compatibility manifest exceeded bounded schema limits"
            }
            Self::InvalidHostPolicy { .. } | Self::InvalidDeprecationManifest(_) => {
                "host SABI compatibility policy window is below statutory minimum"
            }
            Self::ZeroTargetSabiEpoch => {
                "package target_sabi_epoch zero is invalid; run nlos-migrate"
            }
            Self::FutureTargetSabiEpoch { .. } => {
                "package target_sabi_epoch exceeds host SABI epoch; upgrade host or re-target via nlos-migrate"
            }
            Self::SunsetTargetSabiEpoch { .. } => {
                "package target_sabi_epoch is outside the N-2 compatibility window and has sunset; run nlos-migrate"
            }
            Self::IncompatibleDriverAbiEpoch { .. } => {
                "package driver_abi_epoch is incompatible or sunset; run nlos-migrate"
            }
            Self::UnsupportedCriticalExtension { .. } => {
                "package requires an unsupported critical SABI extension; run nlos-migrate"
            }
            Self::UnsupportedOrSunsetFeature { .. } => {
                "package requires an unsupported or sunset SABI feature; run nlos-migrate"
            }
        };
        sabi::v1::SabiFailure {
            code: sabi::v1::SabiErrorCode::AbiVersion.into(),
            retry: sabi::v1::RetryDirective::DoNotRetry.into(),
            safe_message: safe_message.to_owned(),
        }
    }

    /// Actionable developer upgrade guidance referencing `nlos-migrate`.
    #[must_use]
    pub fn migration_guidance(&self) -> String {
        match self {
            Self::ZeroTargetSabiEpoch => {
                "invalid `target_sabi_epoch = 0`; run `nlos-migrate check --manifest package.manifest` and set `compatibility.target-sabi-epoch >= 1`".to_owned()
            }
            Self::FutureTargetSabiEpoch {
                target_epoch,
                host_epoch,
            } => format!(
                "package targets future SABI epoch v{target_epoch} on host epoch v{host_epoch}; upgrade NLOS host to v{target_epoch}+ or run `nlos-migrate rewrite --to-epoch {host_epoch} --manifest package.manifest`"
            ),
            Self::SunsetTargetSabiEpoch {
                target_epoch,
                host_epoch,
                max_backward_epochs,
            } => {
                let min_supported = host_epoch.saturating_sub(*max_backward_epochs);
                format!(
                    "package SABI epoch v{target_epoch} has sunset on host v{host_epoch} (supported rolling window: v{min_supported}..=v{host_epoch}); run `nlos-migrate rewrite --from-epoch {target_epoch} --to-epoch {host_epoch} --manifest package.manifest`"
                )
            }
            Self::IncompatibleDriverAbiEpoch {
                driver_epoch,
                host_driver_epoch,
                ..
            } => format!(
                "driver_abi_epoch v{driver_epoch} is incompatible with host DriverProvider v{host_driver_epoch}; run `nlos-migrate rewrite --driver-to-epoch {host_driver_epoch} --manifest package.manifest`"
            ),
            Self::UnsupportedCriticalExtension { extension_id } => format!(
                "critical extension ID {extension_id} is unsupported by host; run `nlos-migrate audit-extensions --manifest package.manifest` to drop or downgrade non-essential critical extensions"
            ),
            Self::UnsupportedOrSunsetFeature { feature, reason } => format!(
                "required feature {feature:?} rejected ({reason}); run `nlos-migrate check --manifest package.manifest` to apply canonical replacement"
            ),
            Self::InvalidManifestSpec(err) => format!(
                "invalid compatibility manifest ({err}); run `nlos-migrate check --manifest package.manifest`"
            ),
            Self::InvalidHostPolicy {
                host_epoch,
                max_backward_epochs,
            } => format!(
                "host compatibility policy (host_epoch={host_epoch}, max_backward_epochs={max_backward_epochs}) violates statutory minimum window; verify host configuration with `nlos-migrate doctor`"
            ),
            Self::InvalidDeprecationManifest(err) => format!(
                "deprecation manifest validation failed ({err}); run `nlos-migrate doctor`"
            ),
        }
    }
}

impl fmt::Display for CompatMountRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.migration_guidance())
    }
}

impl Error for CompatMountRejection {}

/// Evaluates [`PackageCompatibilitySpec`] against `policy` and the embedded
/// [`canonical_deprecation_manifest`].
///
/// # Errors
///
/// Fails closed with [`CompatMountRejection`] on `target_sabi_epoch == 0`,
/// `> host_epoch`, `<= host_epoch - 3` (sunset), unknown `critical_extensions`,
/// or incompatible `driver_abi_epoch` / `required_features`.
pub fn evaluate_compat_shim_mount(
    spec: &PackageCompatibilitySpec,
    policy: &CompatShimMountPolicy,
) -> Result<CompatShimMountDecision, CompatMountRejection> {
    let manifest = canonical_deprecation_manifest()
        .map_err(CompatMountRejection::InvalidDeprecationManifest)?;
    evaluate_compat_shim_mount_with_manifest(spec, policy, &manifest)
}

/// Evaluates [`PackageCompatibilitySpec`] against `policy` and an explicit [`DeprecationManifest`].
///
/// # Errors
///
/// Fails closed with [`CompatMountRejection`] when any compatibility bound,
/// critical extension, feature gate, or rolling epoch window check fails.
pub fn evaluate_compat_shim_mount_with_manifest(
    spec: &PackageCompatibilitySpec,
    policy: &CompatShimMountPolicy,
    deprecation_manifest: &DeprecationManifest,
) -> Result<CompatShimMountDecision, CompatMountRejection> {
    spec.validate_bounds()
        .map_err(CompatMountRejection::InvalidManifestSpec)?;

    if policy.host_epoch == 0
        || policy.host_driver_abi_epoch == 0
        || policy.max_backward_epochs < MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS
    {
        return Err(CompatMountRejection::InvalidHostPolicy {
            host_epoch: policy.host_epoch,
            max_backward_epochs: policy.max_backward_epochs,
        });
    }

    if spec.target_sabi_epoch == 0 {
        return Err(CompatMountRejection::ZeroTargetSabiEpoch);
    }
    if spec.target_sabi_epoch > policy.host_epoch {
        return Err(CompatMountRejection::FutureTargetSabiEpoch {
            target_epoch: spec.target_sabi_epoch,
            host_epoch: policy.host_epoch,
        });
    }
    let sabi_distance = policy.host_epoch - spec.target_sabi_epoch;
    if sabi_distance > policy.max_backward_epochs {
        return Err(CompatMountRejection::SunsetTargetSabiEpoch {
            target_epoch: spec.target_sabi_epoch,
            host_epoch: policy.host_epoch,
            max_backward_epochs: policy.max_backward_epochs,
        });
    }

    for &ext_id in &spec.critical_extensions {
        if ext_id == 0 || !policy.known_critical_extensions.contains(&ext_id) {
            return Err(CompatMountRejection::UnsupportedCriticalExtension {
                extension_id: ext_id,
            });
        }
    }

    validate_required_features(spec, policy, deprecation_manifest)?;
    let driver_shim = evaluate_driver_shim(spec, policy)?;

    if sabi_distance == 0 && driver_shim.is_none() {
        return Ok(CompatShimMountDecision::NativeDirect {
            host_epoch: policy.host_epoch,
            target_sabi_minor: spec.target_sabi_minor,
            retained_non_critical_extensions: spec.non_critical_extensions.clone(),
        });
    }

    let sabi_adapter = if sabi_distance == 0 {
        None
    } else {
        let cell = SabiEpochAdapterCell::with_manifest(
            spec.target_sabi_epoch,
            policy.host_epoch,
            policy.max_backward_epochs,
            policy.known_critical_extensions.clone(),
            deprecation_manifest.clone(),
        )
        .map_err(|_| CompatMountRejection::SunsetTargetSabiEpoch {
            target_epoch: spec.target_sabi_epoch,
            host_epoch: policy.host_epoch,
            max_backward_epochs: policy.max_backward_epochs,
        })?;
        Some(cell)
    };

    Ok(CompatShimMountDecision::MountOutOfCore(MountedCompatShim {
        sabi_adapter,
        driver_shim,
        retained_non_critical_extensions: spec.non_critical_extensions.clone(),
    }))
}

fn validate_required_features(
    spec: &PackageCompatibilitySpec,
    policy: &CompatShimMountPolicy,
    deprecation_manifest: &DeprecationManifest,
) -> Result<(), CompatMountRejection> {
    for feature in &spec.required_features {
        if let SymbolAuditVerdict::SunsetOrReserved { reason, .. } =
            deprecation_manifest.audit_symbol(SABI_ENVELOPE_SCHEMA, feature)
        {
            return Err(CompatMountRejection::UnsupportedOrSunsetFeature {
                feature: feature.clone(),
                reason,
            });
        }
        if !policy.supported_features.is_empty() && !policy.supported_features.contains(feature) {
            return Err(CompatMountRejection::UnsupportedOrSunsetFeature {
                feature: feature.clone(),
                reason: "feature is not advertised by host policy",
            });
        }
    }
    Ok(())
}

fn evaluate_driver_shim(
    spec: &PackageCompatibilitySpec,
    policy: &CompatShimMountPolicy,
) -> Result<Option<DriverProtocolCompatShim>, CompatMountRejection> {
    let Some(driver_epoch) = spec.driver_abi_epoch else {
        return Ok(None);
    };
    if driver_epoch == 0
        || driver_epoch > policy.host_driver_abi_epoch
        || policy.host_driver_abi_epoch - driver_epoch > policy.max_backward_epochs
    {
        return Err(CompatMountRejection::IncompatibleDriverAbiEpoch {
            driver_epoch,
            host_driver_epoch: policy.host_driver_abi_epoch,
            max_backward_epochs: policy.max_backward_epochs,
        });
    }
    if driver_epoch == policy.host_driver_abi_epoch
        && policy.legacy_driver_wire_format != LegacyDriverWireFormat::StageBMinicbor
    {
        return Ok(None);
    }
    let shim = DriverProtocolCompatShim::new(
        driver_epoch,
        policy.host_driver_abi_epoch,
        policy.max_backward_epochs,
        policy.legacy_driver_wire_format,
    )
    .map_err(|_| CompatMountRejection::IncompatibleDriverAbiEpoch {
        driver_epoch,
        host_driver_epoch: policy.host_driver_abi_epoch,
        max_backward_epochs: policy.max_backward_epochs,
    })?;
    Ok(Some(shim))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode};

    #[test]
    fn native_epoch_routes_direct_and_legacy_epochs_mount_out_of_core_cell() {
        let policy = CompatShimMountPolicy::new(3);

        let current_spec = PackageCompatibilitySpec::new(3, 0);
        let direct = evaluate_compat_shim_mount(&current_spec, &policy).expect("native direct");
        assert!(matches!(
            direct,
            CompatShimMountDecision::NativeDirect { host_epoch: 3, .. }
        ));

        let legacy_spec = PackageCompatibilitySpec::new(1, 0).with_driver_abi_epoch(1);
        let mounted = evaluate_compat_shim_mount(&legacy_spec, &policy).expect("mount N-2");
        match mounted {
            CompatShimMountDecision::MountOutOfCore(cell) => {
                assert_eq!(
                    cell.sabi_adapter
                        .as_ref()
                        .expect("sabi adapter")
                        .epoch_distance(),
                    2
                );
                assert!(cell.driver_shim.is_some());
            }
            CompatShimMountDecision::NativeDirect { .. } => panic!("expected MountOutOfCore"),
        }
    }

    #[test]
    fn incompatible_driver_abi_epoch_fails_closed_with_migrate_guidance() {
        let policy = CompatShimMountPolicy::new(4);
        let bad_driver_spec = PackageCompatibilitySpec::new(3, 0).with_driver_abi_epoch(1);
        let err = evaluate_compat_shim_mount(&bad_driver_spec, &policy)
            .expect_err("driver_abi_epoch=1 at host_epoch=4 must fail closed");
        let failure = err.to_sabi_failure();
        assert_eq!(failure.code, i32::from(SabiErrorCode::AbiVersion));
        assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
        assert!(err.migration_guidance().contains("nlos-migrate"));
    }
}
