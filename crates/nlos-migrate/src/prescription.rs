//! Declarative SABI contract, extension, epoch-window, and driver
//! `EffectDescriptor` static adjudicator (`[COMPAT-VER-001]`,
//! `[COMPAT-VER-002]`, `[COMPAT-DEPRECATE-001]`, `[DRV-ABI-001]`,
//! `[DRV-LIFE-001]`).

use std::collections::HashMap;

use nlos_schema::{
    ContractElementKind, DeprecationEntry, DeprecationManifest, MAX_EXTENSION_IDS,
    MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS, SymbolAuditVerdict, schema_registry,
};
use nlos_service_directory::DeprecationWarningReceipt;

use crate::json::hex_encode;
use crate::manifest_audit::{
    AuditedPackageManifest, ComponentRole, EffectClass, ExtensionCriticality, ReconcileDirective,
};
use crate::report::{AuditFinding, AuditOptions, FindingSeverity, UpgradePrescription};

/// Combined result of running SABI contract, extension, and driver-lifecycle
/// rules over an [`AuditedPackageManifest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContractAdjudicationOutcome {
    pub host_epoch: u32,
    pub negotiated_route: Option<String>,
    pub findings: Vec<AuditFinding>,
    pub prescriptions: Vec<UpgradePrescription>,
    pub warning_receipts: Vec<DeprecationWarningReceipt>,
}

/// Evaluates an [`AuditedPackageManifest`] against a validated
/// [`DeprecationManifest`] and [`AuditOptions`].
#[must_use]
pub fn adjudicate_manifest_contracts(
    manifest: &AuditedPackageManifest,
    deprecation: &DeprecationManifest,
    options: &AuditOptions,
) -> ContractAdjudicationOutcome {
    let host_epoch = options.host_epoch.unwrap_or(deprecation.current_epoch);
    let max_backward_epochs = options
        .max_backward_epochs
        .unwrap_or(deprecation.min_compatibility_window_epochs);

    let mut findings = Vec::new();
    let mut prescriptions = Vec::new();
    let mut warning_receipts = Vec::new();

    let negotiated_route =
        audit_epoch_windows(manifest, host_epoch, max_backward_epochs, &mut findings);

    audit_driver_effects(manifest, &mut findings);

    audit_sabi_extensions(
        manifest,
        deprecation,
        host_epoch,
        options,
        &mut findings,
        &mut prescriptions,
        &mut warning_receipts,
    );

    audit_sabi_symbol_uses(
        manifest,
        deprecation,
        host_epoch,
        &mut findings,
        &mut prescriptions,
        &mut warning_receipts,
    );

    ContractAdjudicationOutcome {
        host_epoch,
        negotiated_route,
        findings,
        prescriptions,
        warning_receipts,
    }
}

fn audit_epoch_windows(
    manifest: &AuditedPackageManifest,
    host_epoch: u32,
    max_backward_epochs: u32,
    findings: &mut Vec<AuditFinding>,
) -> Option<String> {
    if host_epoch == 0 {
        findings.push(AuditFinding {
            severity: FindingSeverity::Blocker,
            rule_id: "COMPAT-VER-001".to_owned(),
            location: "audit-options:host-epoch".to_owned(),
            message: "host SABI major epoch cannot be 0".to_owned(),
            remediation: Some("Specify --host-epoch >= 1.".to_owned()),
        });
        return None;
    }

    if max_backward_epochs < MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS {
        findings.push(AuditFinding {
            severity: FindingSeverity::Blocker,
            rule_id: "COMPAT-VER-001".to_owned(),
            location: "audit-options:max-backward-epochs".to_owned(),
            message: format!(
                "backward compatibility window {max_backward_epochs} is smaller than statutory minimum {MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS}"
            ),
            remediation: Some(format!(
                "Configure a rolling window of at least {MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS} epochs (N / N-1 / N-2)."
            )),
        });
        return None;
    }

    let target_major = manifest.target_sabi_epoch.major;
    let route = if target_major == 0 {
        findings.push(AuditFinding {
            severity: FindingSeverity::Blocker,
            rule_id: "COMPAT-VER-001".to_owned(),
            location: "package.manifest:target-sabi-epoch".to_owned(),
            message: "target-sabi-epoch major version 0 is invalid".to_owned(),
            remediation: Some(format!(
                "Target an active SABI major epoch within [{}..={host_epoch}].",
                host_epoch.saturating_sub(max_backward_epochs).max(1)
            )),
        });
        None
    } else if target_major > host_epoch {
        findings.push(AuditFinding {
            severity: FindingSeverity::Blocker,
            rule_id: "COMPAT-VER-001".to_owned(),
            location: "package.manifest:target-sabi-epoch".to_owned(),
            message: format!(
                "package target-sabi-epoch {} exceeds host SABI major epoch {host_epoch}",
                manifest.target_sabi_epoch
            ),
            remediation: Some(format!(
                "Target SABI epoch {host_epoch} (or upgrade the host runtime before installing this package)."
            )),
        });
        None
    } else if host_epoch - target_major > max_backward_epochs {
        let min_supported = host_epoch - max_backward_epochs;
        findings.push(AuditFinding {
            severity: FindingSeverity::Blocker,
            rule_id: "COMPAT-VER-001".to_owned(),
            location: "package.manifest:target-sabi-epoch".to_owned(),
            message: format!(
                "package target-sabi-epoch {} is outside host epoch {host_epoch} N/N-1/N-2 window (minimum supported major epoch is {min_supported})",
                manifest.target_sabi_epoch
            ),
            remediation: Some(format!(
                "Migrate package SABI bindings to major epoch >= {min_supported} (recommended: {host_epoch})."
            )),
        });
        None
    } else if host_epoch == target_major {
        Some("NATIVE_CURRENT_EPOCH".to_owned())
    } else {
        Some("OUT_OF_CORE_COMPAT_SHIM".to_owned())
    };

    if let Some(driver_epoch) = manifest.driver_abi_epoch {
        if driver_epoch == 0 {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "DRV-ABI-001".to_owned(),
                location: "package.manifest:driver-abi-epoch".to_owned(),
                message: "driver-abi-epoch 0 is invalid".to_owned(),
                remediation: Some(format!(
                    "Declare driver-abi-epoch within [{}..={host_epoch}].",
                    host_epoch.saturating_sub(max_backward_epochs).max(1)
                )),
            });
        } else if driver_epoch > host_epoch || host_epoch - driver_epoch > max_backward_epochs {
            let min_supported = host_epoch.saturating_sub(max_backward_epochs).max(1);
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "DRV-ABI-001".to_owned(),
                location: "package.manifest:driver-abi-epoch".to_owned(),
                message: format!(
                    "driver-abi-epoch {driver_epoch} is outside host epoch {host_epoch} compatibility window [{min_supported}..={host_epoch}]"
                ),
                remediation: Some(format!(
                    "Rebuild the user-space driver against Driver ABI epoch {host_epoch}."
                )),
            });
        }
    }

    route
}

#[allow(clippy::too_many_lines)]
fn audit_driver_effects(manifest: &AuditedPackageManifest, findings: &mut Vec<AuditFinding>) {
    for component in &manifest.components {
        if component.role == ComponentRole::Driver {
            let has_effect = manifest
                .effects
                .iter()
                .any(|effect| effect.matches_component(&component.name));
            if !has_effect {
                findings.push(AuditFinding {
                    severity: FindingSeverity::Blocker,
                    rule_id: "DRV-LIFE-001".to_owned(),
                    location: format!("package.manifest:{}", component.line),
                    message: format!(
                        "driver component {:?} is missing a required EffectDescriptor declaration",
                        component.name
                    ),
                    remediation: Some(format!(
                        "Add `effect = {} <READ_ONLY|REVERSIBLE|COMPENSATABLE|IRREVERSIBLE> reconcile=same-idempotency-key` to package.manifest.",
                        component.name
                    )),
                });
            }
        }
    }

    for effect in &manifest.effects {
        let location = format!("package.manifest:{}", effect.line);
        let matched_component = manifest
            .components
            .iter()
            .find(|component| effect.matches_component(&component.name));

        let Some(component) = matched_component else {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "DRV-LIFE-001".to_owned(),
                location,
                message: format!(
                    "effect target {:?} does not match any declared entry or component",
                    effect.target
                ),
                remediation: Some(
                    "Reference an existing component name or `<component>::<operation>`."
                        .to_owned(),
                ),
            });
            continue;
        };

        match &effect.reconcile {
            ReconcileDirective::ForbiddenPolicy(policy) => {
                findings.push(AuditFinding {
                    severity: FindingSeverity::Blocker,
                    rule_id: "DRV-LIFE-001".to_owned(),
                    location,
                    message: format!(
                        "effect target {:?} declares forbidden UNCERTAIN reconciliation policy {policy:?} (fresh-key blind retry is prohibited)",
                        effect.target
                    ),
                    remediation: Some(
                        "Declare `reconcile=same-idempotency-key` so UNCERTAIN outcomes reconcile under the original 128-bit IdempotencyKey."
                            .to_owned(),
                    ),
                });
            }
            ReconcileDirective::Omitted => {
                if component.role == ComponentRole::Driver
                    || effect.effect_class != EffectClass::ReadOnly
                {
                    findings.push(AuditFinding {
                        severity: FindingSeverity::Blocker,
                        rule_id: "DRV-LIFE-001".to_owned(),
                        location,
                        message: format!(
                            "effect target {:?} ({}) omits explicit UNCERTAIN reconciliation policy",
                            effect.target,
                            effect.effect_class.as_str()
                        ),
                        remediation: Some(
                            "Append `reconcile=same-idempotency-key` (or `reconcile=none` only for pure READ_ONLY operations)."
                                .to_owned(),
                        ),
                    });
                }
            }
            ReconcileDirective::None => {
                if effect.effect_class != EffectClass::ReadOnly {
                    findings.push(AuditFinding {
                        severity: FindingSeverity::Blocker,
                        rule_id: "DRV-LIFE-001".to_owned(),
                        location,
                        message: format!(
                            "mutating effect target {:?} ({}) cannot declare `reconcile=none`",
                            effect.target,
                            effect.effect_class.as_str()
                        ),
                        remediation: Some(
                            "Use `reconcile=same-idempotency-key` for all REVERSIBLE, COMPENSATABLE, and IRREVERSIBLE operations."
                                .to_owned(),
                        ),
                    });
                }
            }
            ReconcileDirective::SameIdempotencyKey => {}
        }
    }
}

#[allow(clippy::too_many_lines)]
fn audit_sabi_extensions(
    manifest: &AuditedPackageManifest,
    deprecation: &DeprecationManifest,
    host_epoch: u32,
    options: &AuditOptions,
    findings: &mut Vec<AuditFinding>,
    prescriptions: &mut Vec<UpgradePrescription>,
    warning_receipts: &mut Vec<DeprecationWarningReceipt>,
) {
    let mut counts: HashMap<(&str, ExtensionCriticality), usize> = HashMap::new();
    let package_service_tag = format!("pkg:{}", hex_encode(manifest.package_id.as_bytes()));

    for ext in &manifest.sabi_extensions {
        let location = format!("package.manifest:{}", ext.line);
        if deprecation.channel(&ext.channel).is_none() {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "COMPAT-VER-001".to_owned(),
                location,
                message: format!(
                    "sabi-ext references unknown or unregistered SABI channel {:?}",
                    ext.channel
                ),
                remediation: Some(
                    "Use one of the 9 canonical frozen `nlos.sabi.*` channel names.".to_owned(),
                ),
            });
            continue;
        }

        let counter = counts
            .entry((ext.channel.as_str(), ext.criticality))
            .or_insert(0);
        *counter += 1;
        if *counter > MAX_EXTENSION_IDS {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "COMPAT-VER-002".to_owned(),
                location: location.clone(),
                message: format!(
                    "channel {:?} declares more than {MAX_EXTENSION_IDS} {} extensions",
                    ext.channel,
                    ext.criticality.as_str()
                ),
                remediation: Some(format!(
                    "Reduce declared {} extension IDs to <= {MAX_EXTENSION_IDS}.",
                    ext.criticality.as_str()
                )),
            });
        }

        if ext.extension_id == 0 {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "COMPAT-VER-002".to_owned(),
                location,
                message: format!(
                    "sabi-ext in channel {:?} declares invalid extension ID 0",
                    ext.channel
                ),
                remediation: Some("Use a non-zero registered SABI extension ID.".to_owned()),
            });
            continue;
        }

        let ext_symbol_fallback = format!("extension:{}", ext.extension_id);
        match deprecation.audit_field_number(&ext.channel, None, ext.extension_id) {
            SymbolAuditVerdict::SunsetOrReserved { entry, reason } => {
                let symbol =
                    entry.map_or_else(|| ext_symbol_fallback.clone(), |item| item.symbol.clone());
                let prescription = build_sunset_prescription(
                    &ext.channel,
                    &symbol,
                    Some(ext.extension_id),
                    entry,
                    reason,
                    host_epoch,
                );
                findings.push(AuditFinding {
                    severity: FindingSeverity::Blocker,
                    rule_id: "COMPAT-DEPRECATE-001".to_owned(),
                    location,
                    message: format!(
                        "sabi-ext ID {} in channel {:?} is retired or reserved: {reason}",
                        ext.extension_id, ext.channel
                    ),
                    remediation: Some(prescription.patch_summary.clone()),
                });
                prescriptions.push(prescription);
            }
            SymbolAuditVerdict::Deprecated(entry) => {
                if ext.criticality == ExtensionCriticality::Critical
                    && !is_known_critical_extension(
                        &ext.channel,
                        ext.extension_id,
                        Some(entry),
                        options,
                    )
                {
                    findings.push(AuditFinding {
                        severity: FindingSeverity::Blocker,
                        rule_id: "COMPAT-VER-002".to_owned(),
                        location,
                        message: format!(
                            "unknown critical SABI extension ID {} in channel {:?} must fail-closed",
                            ext.extension_id, ext.channel
                        ),
                        remediation: Some(
                            "Downgrade optional capability negotiation to `noncritical` or target a host supporting this critical extension."
                                .to_owned(),
                        ),
                    });
                } else {
                    record_deprecated_entry(
                        manifest,
                        entry,
                        Some(ext.extension_id),
                        host_epoch,
                        &location,
                        &package_service_tag,
                        findings,
                        prescriptions,
                        warning_receipts,
                    );
                }
            }
            SymbolAuditVerdict::Active(opt_entry) => {
                if ext.criticality == ExtensionCriticality::Critical
                    && !is_known_critical_extension(
                        &ext.channel,
                        ext.extension_id,
                        opt_entry,
                        options,
                    )
                {
                    findings.push(AuditFinding {
                        severity: FindingSeverity::Blocker,
                        rule_id: "COMPAT-VER-002".to_owned(),
                        location,
                        message: format!(
                            "unknown critical SABI extension ID {} in channel {:?} must fail-closed",
                            ext.extension_id, ext.channel
                        ),
                        remediation: Some(
                            "Remove unsupported `critical` extension ID or mark optional telemetry/hint extensions as `noncritical`."
                                .to_owned(),
                        ),
                    });
                }
            }
        }
    }
}

fn is_known_critical_extension(
    channel: &str,
    extension_id: u32,
    manifest_entry: Option<&DeprecationEntry>,
    options: &AuditOptions,
) -> bool {
    if options.known_critical_extensions.contains(&extension_id) {
        return true;
    }
    if schema_registry().iter().any(|desc| {
        desc.name == channel && desc.supported_critical_extensions.contains(&extension_id)
    }) {
        return true;
    }
    manifest_entry.is_some_and(|entry| entry.kind == ContractElementKind::Extension)
}

#[allow(clippy::too_many_lines)]
fn audit_sabi_symbol_uses(
    manifest: &AuditedPackageManifest,
    deprecation: &DeprecationManifest,
    host_epoch: u32,
    findings: &mut Vec<AuditFinding>,
    prescriptions: &mut Vec<UpgradePrescription>,
    warning_receipts: &mut Vec<DeprecationWarningReceipt>,
) {
    let package_service_tag = format!("pkg:{}", hex_encode(manifest.package_id.as_bytes()));

    for use_decl in &manifest.sabi_uses {
        let location = format!("package.manifest:{}", use_decl.line);
        if deprecation.channel(&use_decl.channel).is_none() {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "COMPAT-VER-001".to_owned(),
                location,
                message: format!(
                    "sabi-use references unknown or unregistered SABI channel {:?}",
                    use_decl.channel
                ),
                remediation: Some(
                    "Use one of the 9 canonical frozen `nlos.sabi.*` channel names.".to_owned(),
                ),
            });
            continue;
        }

        if use_decl.field_number == Some(0) {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "COMPAT-DEPRECATE-001".to_owned(),
                location,
                message: format!(
                    "sabi-use {:?} in channel {:?} declares invalid protobuf field number 0",
                    use_decl.symbol, use_decl.channel
                ),
                remediation: Some(
                    "Specify a valid positive protobuf field number >= 1.".to_owned(),
                ),
            });
            continue;
        }

        let symbol_verdict = deprecation.audit_symbol(&use_decl.channel, &use_decl.symbol);
        let field_verdict = use_decl.field_number.map(|field_no| {
            resolve_field_verdict(deprecation, &use_decl.channel, &use_decl.symbol, field_no)
        });

        // First check if either symbol or field number hit SUNSET / RESERVED.
        if let SymbolAuditVerdict::SunsetOrReserved { entry, reason } = symbol_verdict {
            let effective_field = use_decl
                .field_number
                .or_else(|| entry.and_then(|item| item.field_number));
            let prescription = build_sunset_prescription(
                &use_decl.channel,
                &use_decl.symbol,
                effective_field,
                entry,
                reason,
                host_epoch,
            );
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "COMPAT-DEPRECATE-001".to_owned(),
                location,
                message: format!(
                    "SABI contract {:?} in channel {:?} cannot be used: {reason}",
                    use_decl.symbol, use_decl.channel
                ),
                remediation: Some(prescription.patch_summary.clone()),
            });
            prescriptions.push(prescription);
            continue;
        }

        if let Some(SymbolAuditVerdict::SunsetOrReserved { entry, reason }) = field_verdict {
            let symbol_label =
                entry.map_or_else(|| use_decl.symbol.clone(), |item| item.symbol.clone());
            let prescription = build_sunset_prescription(
                &use_decl.channel,
                &symbol_label,
                use_decl.field_number,
                entry,
                reason,
                host_epoch,
            );
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "COMPAT-DEPRECATE-001".to_owned(),
                location,
                message: format!(
                    "SABI field {:?} (field={}) in channel {:?} cannot be used: {reason}",
                    use_decl.symbol,
                    use_decl.field_number.unwrap_or_default(),
                    use_decl.channel
                ),
                remediation: Some(prescription.patch_summary.clone()),
            });
            prescriptions.push(prescription);
            continue;
        }

        // Check field_number consistency if the matched symbol entry declares an explicit field_number.
        if let (
            SymbolAuditVerdict::Active(Some(entry)) | SymbolAuditVerdict::Deprecated(entry),
            Some(declared_field),
        ) = (symbol_verdict, use_decl.field_number)
            && let Some(expected_field) = entry.field_number
            && expected_field != declared_field
        {
            findings.push(AuditFinding {
                severity: FindingSeverity::Blocker,
                rule_id: "COMPAT-DEPRECATE-001".to_owned(),
                location,
                message: format!(
                    "SABI symbol {:?} in channel {:?} has canonical field_number={expected_field}, but manifest declared field={declared_field}",
                    use_decl.symbol, use_decl.channel
                ),
                remediation: Some(format!(
                    "Update `sabi-use = {} {} field={expected_field}` to match the frozen schema slot.",
                    use_decl.channel, use_decl.symbol
                )),
            });
            continue;
        }

        // Next check DEPRECATED verdicts (non-blocking WARNING + UpgradePrescription).
        if let SymbolAuditVerdict::Deprecated(entry) = symbol_verdict {
            record_deprecated_entry(
                manifest,
                entry,
                use_decl.field_number.or(entry.field_number),
                host_epoch,
                &location,
                &package_service_tag,
                findings,
                prescriptions,
                warning_receipts,
            );
            continue;
        }

        if let Some(SymbolAuditVerdict::Deprecated(entry)) = field_verdict {
            record_deprecated_entry(
                manifest,
                entry,
                use_decl.field_number.or(entry.field_number),
                host_epoch,
                &location,
                &package_service_tag,
                findings,
                prescriptions,
                warning_receipts,
            );
        }
    }
}

fn resolve_field_verdict<'a>(
    deprecation: &'a DeprecationManifest,
    channel: &str,
    symbol: &str,
    field_no: u32,
) -> SymbolAuditVerdict<'a> {
    let direct = deprecation.audit_field_number(channel, Some(symbol), field_no);
    if !matches!(direct, SymbolAuditVerdict::Active(None)) {
        return direct;
    }
    if let Some((parent, _)) = symbol.rsplit_once('.') {
        let scoped = deprecation.audit_field_number(channel, Some(parent), field_no);
        if !matches!(scoped, SymbolAuditVerdict::Active(None)) {
            return scoped;
        }
    }
    deprecation.audit_field_number(channel, None, field_no)
}

#[allow(clippy::too_many_arguments)]
fn record_deprecated_entry(
    manifest: &AuditedPackageManifest,
    entry: &DeprecationEntry,
    field_number: Option<u32>,
    host_epoch: u32,
    location: &str,
    package_service_tag: &str,
    findings: &mut Vec<AuditFinding>,
    prescriptions: &mut Vec<UpgradePrescription>,
    warning_receipts: &mut Vec<DeprecationWarningReceipt>,
) {
    let earliest_sunset = entry.earliest_sunset_epoch.unwrap_or(host_epoch);
    let remaining_grace = earliest_sunset.saturating_sub(host_epoch);
    let replacement = entry
        .replacement
        .clone()
        .unwrap_or_else(|| "<see-migration-rule>".to_owned());
    let rule_id = entry
        .migration_rule_id
        .clone()
        .unwrap_or_else(|| "COMPAT-DEPRECATE-001".to_owned());

    let field_clause = field_number.map_or_else(String::new, |num| format!(" (field={num})"));
    let patch_summary = format!(
        "Replace `{}`{}{field_clause} in channel `{}` with `{replacement}` per rule `{rule_id}` before SABI epoch {earliest_sunset} ({remaining_grace} epoch(s) remaining in grace window).",
        entry.symbol,
        entry
            .reason
            .as_deref()
            .map_or_else(String::new, |why| format!(" [{why}]")),
        entry.channel,
    );

    let prescription = UpgradePrescription {
        channel: entry.channel.clone(),
        symbol: entry.symbol.clone(),
        field_number,
        lifecycle_state: "DEPRECATED".to_owned(),
        host_epoch,
        deprecated_in_epoch: entry.deprecated_in_epoch,
        earliest_sunset_epoch: entry.earliest_sunset_epoch,
        remaining_grace_epochs: Some(remaining_grace),
        replacement_symbol: entry.replacement.clone(),
        migration_rule_id: entry.migration_rule_id.clone(),
        patch_summary: patch_summary.clone(),
    };

    if let Some(receipt) = prescription.to_warning_receipt(
        manifest.target_sabi_epoch.major,
        manifest.target_sabi_epoch.minor,
        package_service_tag,
    ) {
        warning_receipts.push(receipt);
    }

    findings.push(AuditFinding {
        severity: FindingSeverity::Warning,
        rule_id: "COMPAT-DEPRECATE-001".to_owned(),
        location: location.to_owned(),
        message: format!(
            "SABI contract {:?}{field_clause} in channel {:?} is DEPRECATED (deprecated_in_epoch={}, earliest_sunset_epoch={earliest_sunset}, remaining_grace_epochs={remaining_grace})",
            entry.symbol,
            entry.channel,
            entry.deprecated_in_epoch.unwrap_or(0),
        ),
        remediation: Some(patch_summary),
    });

    prescriptions.push(prescription);
}

fn build_sunset_prescription(
    channel: &str,
    symbol: &str,
    field_number: Option<u32>,
    entry: Option<&DeprecationEntry>,
    reason: &str,
    host_epoch: u32,
) -> UpgradePrescription {
    let lifecycle_state = if entry.is_some() {
        "SUNSET".to_owned()
    } else {
        "RESERVED".to_owned()
    };
    let replacement_symbol = entry.and_then(|item| item.replacement.clone());
    let migration_rule_id = entry.and_then(|item| item.migration_rule_id.clone());
    let field_clause = field_number.map_or_else(String::new, |num| format!(" (field={num})"));

    let patch_summary = match (&replacement_symbol, &migration_rule_id) {
        (Some(replacement), Some(rule)) => format!(
            "Contract `{symbol}`{field_clause} in channel `{channel}` is {lifecycle_state} ({reason}); migrate immediately to `{replacement}` per rule `{rule}`."
        ),
        (Some(replacement), None) => format!(
            "Contract `{symbol}`{field_clause} in channel `{channel}` is {lifecycle_state} ({reason}); migrate immediately to `{replacement}`."
        ),
        (None, Some(rule)) => format!(
            "Contract `{symbol}`{field_clause} in channel `{channel}` is {lifecycle_state} ({reason}); remove usage per rule `{rule}`."
        ),
        (None, None) => format!(
            "Contract `{symbol}`{field_clause} in channel `{channel}` is {lifecycle_state} ({reason}); remove retired/reserved slot reference."
        ),
    };

    UpgradePrescription {
        channel: channel.to_owned(),
        symbol: symbol.to_owned(),
        field_number,
        lifecycle_state,
        host_epoch,
        deprecated_in_epoch: entry.and_then(|item| item.deprecated_in_epoch),
        earliest_sunset_epoch: entry.and_then(|item| item.earliest_sunset_epoch),
        remaining_grace_epochs: Some(0),
        replacement_symbol,
        migration_rule_id,
        patch_summary,
    }
}
