//! Gate 1: Protocol, SABI v1 surface conformance, `N / N-1 / N-2` epoch window,
//! and `EffectDescriptor` / `UNCERTAIN` reconciliation audit (`ADR-0027` §3.1).
//!
//! Enforces:
//! - `[SABI-CONFORM-001]`: Validates `SurfaceProfile` (`APPLICATION_PLATFORM` vs
//!   `DESKTOP`), imported/exported SABI v1 services and feature sets against the
//!   frozen SABI v1 service catalog, and cross-service negotiation invariants
//!   (such as `SEMANTIC_SETTLEMENT` requiring `RESOURCE_ESCROW`).
//! - `[COMPAT-VER-001]`, `[COMPAT-VER-002]`, `[COMPAT-DEPRECATE-001]`, `[DRV-NEGO-001]`:
//!   Reuses `nlos_migrate::audit_package_directory_with_manifest` to enforce the
//!   `N / N-1 / N-2` SABI epoch window, reject unknown `critical` extensions, and
//!   validate driver ABI negotiation.
//! - `[DRV-LIFE-001]` & `[KABI-UNCERTAIN-001]`: Audits `EffectDescriptor`
//!   declarations across `READ_ONLY` / `REVERSIBLE` / `COMPENSATABLE` /
//!   `IRREVERSIBLE`, 128-bit `IdempotencyKey` passthrough, and `UNCERTAIN`
//!   same-idempotency-key reconciliation.

use std::collections::BTreeSet;
use std::fs;

use nlos_migrate::{
    AuditOptions, ComponentRole, EffectClass, FindingSeverity, ReconcileDirective,
    adjudicate_manifest_contracts, audit_package_directory_with_manifest,
};
use nlos_schema::canonical_deprecation_manifest;

use crate::model::{
    CertifyError, CertifyFinding, CertifySeverity, GateExecutionSummary, GateId,
    LoadedCertifyPackage, SabiServiceDirection, SurfaceProfile,
};

/// Frozen SABI v1 service specification entry.
#[derive(Clone, Copy, Debug)]
pub struct FrozenSabiServiceSpec {
    /// Canonical uppercase SABI v1 service identifier.
    pub canonical_name: &'static str,
    /// CamelCase alias accepted in manifests.
    pub camel_alias: &'static str,
    /// Minimum required surface profile (`Desktop` means desktop-exclusive).
    pub desktop_exclusive: bool,
    /// Frozen v1 feature flags permitted on this service.
    pub allowed_features: &'static [&'static str],
}

/// Canonical frozen SABI v1 service catalog (v0.5 §7.1–§7.3, ADR-0027 §1–§3).
pub const FROZEN_SABI_V1_CATALOG: &[FrozenSabiServiceSpec] = &[
    FrozenSabiServiceSpec {
        canonical_name: "IDENTITY",
        camel_alias: "Identity",
        desktop_exclusive: false,
        allowed_features: &["principal-verify", "custody-attest", "session-bind"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "CAPABILITY",
        camel_alias: "Capability",
        desktop_exclusive: false,
        allowed_features: &["scoped-grant", "delegation", "revocation-watch"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "SEMANTIC_DISPATCH",
        camel_alias: "SemanticDispatch",
        desktop_exclusive: false,
        allowed_features: &["intent-submit", "structured-reply", "streaming"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "TASK",
        camel_alias: "Task",
        desktop_exclusive: false,
        allowed_features: &["background-job", "cancel-fence", "lease-heartbeat"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "ARTIFACT",
        camel_alias: "Artifact",
        desktop_exclusive: false,
        allowed_features: &[
            "content-addressed",
            "scoped-chooser",
            "provenance",
            "cache-domain",
        ],
    },
    FrozenSabiServiceSpec {
        canonical_name: "AUDIT",
        camel_alias: "Audit",
        desktop_exclusive: false,
        allowed_features: &["append-receipt", "query-lineage"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "POLICY_DECISION",
        camel_alias: "PolicyDecision",
        desktop_exclusive: false,
        allowed_features: &["zone-adjudicate", "z4-consent"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "SCHEMA_REGISTRY",
        camel_alias: "SchemaRegistry",
        desktop_exclusive: false,
        allowed_features: &["v1-wire", "extension-negotiate"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "SERVICE_DIRECTORY",
        camel_alias: "ServiceDirectory",
        desktop_exclusive: false,
        allowed_features: &["lookup", "provider-register", "health-watch"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "CONTEXT_GOVERNOR",
        camel_alias: "ContextGovernor",
        desktop_exclusive: false,
        allowed_features: &["budget-quote", "ephemeral-window"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "RESOURCE_ESCROW",
        camel_alias: "ResourceEscrow",
        desktop_exclusive: false,
        allowed_features: &["quota-hold", "escrow-release", "escrow-settle"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "SEMANTIC_SETTLEMENT",
        camel_alias: "SemanticSettlement",
        desktop_exclusive: false,
        allowed_features: &["two-phase-settle", "compensation-hook"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "APPLICATION",
        camel_alias: "Application",
        desktop_exclusive: false,
        allowed_features: &["lifecycle-query", "task-template"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "SCHEDULER",
        camel_alias: "Scheduler",
        desktop_exclusive: false,
        allowed_features: &["timer-trigger", "cron-window"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "OBSERVABILITY",
        camel_alias: "Observability",
        desktop_exclusive: false,
        allowed_features: &["structured-trace", "health-probe"],
    },
    // Desktop-exclusive SABI v1 services:
    FrozenSabiServiceSpec {
        canonical_name: "UI_SURFACE",
        camel_alias: "UiSurface",
        desktop_exclusive: true,
        allowed_features: &["declarative-view", "chooser-sheet", "z4-banner"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "WORKSPACE",
        camel_alias: "Workspace",
        desktop_exclusive: true,
        allowed_features: &["pane-attach", "focus-handoff"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "SESSION",
        camel_alias: "Session",
        desktop_exclusive: true,
        allowed_features: &["interactive-unlock", "presence-signal"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "WINDOW_COMPOSITOR",
        camel_alias: "WindowCompositor",
        desktop_exclusive: true,
        allowed_features: &["surface-frame", "dpi-scale"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "CLIPBOARD_SHELL",
        camel_alias: "ClipboardShell",
        desktop_exclusive: true,
        allowed_features: &["explicit-paste-intent"],
    },
    FrozenSabiServiceSpec {
        canonical_name: "SYSTEM_TRAY",
        camel_alias: "SystemTray",
        desktop_exclusive: true,
        allowed_features: &["status-indicator"],
    },
];

/// Detailed execution output of Gate 1.
#[derive(Clone, Debug)]
pub struct Gate1Outcome {
    /// Effective surface profile adjudicated by Gate 1.
    pub effective_profile: SurfaceProfile,
    /// Evaluated host SABI epoch.
    pub host_sabi_epoch: u32,
    /// Structured findings emitted by Gate 1.
    pub findings: Vec<CertifyFinding>,
    /// Cryptographically committed summary for receipt inclusion.
    pub summary: GateExecutionSummary,
}

/// Runs Gate 1 (protocol, SABI v1 surface conformance, epoch window, and
/// `EffectDescriptor` / `UNCERTAIN` reconciliation audit) on `pkg`.
///
/// # Errors
///
/// Returns [`CertifyError`] only on unexpected filesystem read errors while
/// inspecting referenced task policy files. All protocol/manifest violations
/// are captured as `BLOCKER` [`CertifyFinding`]s inside [`Gate1Outcome`].
pub fn run_gate1_protocol_audit(
    pkg: &LoadedCertifyPackage,
    requested_profile: SurfaceProfile,
    host_sabi_epoch: u32,
) -> Result<Gate1Outcome, CertifyError> {
    let mut findings = Vec::new();

    // 1. Reuse `nlos_migrate::audit_package_directory_with_manifest` (for standard
    //    `package.manifest` packages) or `adjudicate_manifest_contracts` (when
    //    `package.manifest` carries inline certification-only directives) for
    //    `COMPAT-VER-001/002`, `COMPAT-DEPRECATE-001`, `DRV-NEGO-001`,
    //    `DRV-LIFE-001`, and `KABI-UNCERTAIN-001`.
    let deprecation = canonical_deprecation_manifest().map_err(|err| {
        CertifyError::Authority(format!("canonical deprecation manifest failed: {err}"))
    })?;
    let audit_options = AuditOptions {
        host_epoch: Some(host_sabi_epoch),
        ..AuditOptions::default()
    };
    let migrate_findings = if pkg.directives.has_inline_manifest_extensions {
        adjudicate_manifest_contracts(&pkg.base_manifest, &deprecation, &audit_options).findings
    } else {
        audit_package_directory_with_manifest(&pkg.package_dir, &deprecation, &audit_options)
            .findings
    };

    let mut seen_rule_subjects = BTreeSet::new();
    for mf in &migrate_findings {
        // `PKG-IMM-001` and `PKG-SEED-001` belong to Gate 2 (immutable package purity).
        if matches!(mf.rule_id.as_str(), "PKG-IMM-001" | "PKG-SEED-001") {
            continue;
        }
        let severity = match mf.severity {
            FindingSeverity::Blocker => CertifySeverity::Blocker,
            FindingSeverity::Warning => CertifySeverity::Warning,
        };
        let subject = mf.location.clone();
        seen_rule_subjects.insert((mf.rule_id.clone(), subject.clone(), mf.message.clone()));
        findings.push(CertifyFinding {
            gate: GateId::Gate1ProtocolAndEffects,
            rule_id: mf.rule_id.clone(),
            severity,
            subject,
            message: mf.message.clone(),
            remediation: mf.remediation.clone().unwrap_or_else(|| {
                "Align SABI epoch, extension criticality, and driver contract declarations."
                    .to_owned()
            }),
        });
    }

    // 2. SurfaceProfile & frozen SABI v1 service catalog audit (`[SABI-CONFORM-001]`).
    audit_sabi_surface_conformance(pkg, requested_profile, &mut findings);

    // 3. `EffectDescriptor` completeness, 128-bit `IdempotencyKey` passthrough,
    //    and `UNCERTAIN` reconciliation audit (`[DRV-LIFE-001]`, `[KABI-UNCERTAIN-001]`).
    audit_effects_and_idempotency(pkg, &seen_rule_subjects, &mut findings)?;

    let target_epoch_major = pkg.base_manifest.target_sabi_epoch.major;
    let mut extra = Vec::new();
    extra.extend_from_slice(requested_profile.as_str().as_bytes());
    extra.extend_from_slice(&host_sabi_epoch.to_be_bytes());
    extra.extend_from_slice(&target_epoch_major.to_be_bytes());

    let blocker_count = findings
        .iter()
        .filter(|f| f.severity == CertifySeverity::Blocker)
        .count();
    let summary_line = format!(
        "Gate 1 ({}, target_sabi_epoch={}, host_sabi_epoch={}): {} blocker(s)",
        requested_profile.as_str(),
        pkg.base_manifest.target_sabi_epoch,
        host_sabi_epoch,
        blocker_count,
    );
    let summary = GateExecutionSummary::from_findings(
        GateId::Gate1ProtocolAndEffects,
        &findings,
        &extra,
        summary_line,
    );

    Ok(Gate1Outcome {
        effective_profile: requested_profile,
        host_sabi_epoch,
        findings,
        summary,
    })
}

fn lookup_sabi_service(raw_name: &str) -> Option<&'static FrozenSabiServiceSpec> {
    let trimmed = raw_name.trim();
    FROZEN_SABI_V1_CATALOG.iter().find(|spec| {
        spec.canonical_name.eq_ignore_ascii_case(trimmed)
            || spec.camel_alias.eq_ignore_ascii_case(trimmed)
    })
}

#[allow(clippy::too_many_lines)]
fn audit_sabi_surface_conformance(
    pkg: &LoadedCertifyPackage,
    requested_profile: SurfaceProfile,
    findings: &mut Vec<CertifyFinding>,
) {
    if let Some((ref raw_prof, line_no)) = pkg.directives.declared_surface_profile {
        match SurfaceProfile::parse(raw_prof) {
            Some(declared) => {
                if declared != requested_profile {
                    findings.push(CertifyFinding {
                        gate: GateId::Gate1ProtocolAndEffects,
                        rule_id: "SABI-CONFORM-001".to_owned(),
                        severity: CertifySeverity::Blocker,
                        subject: format!("package.manifest:{line_no}"),
                        message: format!(
                            "manifest declares surface-profile={} which conflicts with certification target profile {}",
                            declared.as_str(),
                            requested_profile.as_str()
                        ),
                        remediation: format!(
                            "Certify under --profile {} or align surface-profile in package.manifest.",
                            declared.cli_name()
                        ),
                    });
                }
            }
            None => {
                findings.push(CertifyFinding {
                    gate: GateId::Gate1ProtocolAndEffects,
                    rule_id: "SABI-CONFORM-001".to_owned(),
                    severity: CertifySeverity::Blocker,
                    subject: format!("package.manifest:{line_no}"),
                    message: format!(
                        "unknown surface-profile '{raw_prof}' (expected APPLICATION_PLATFORM or DESKTOP)"
                    ),
                    remediation:
                        "Set surface-profile to APPLICATION_PLATFORM or DESKTOP.".to_owned(),
                });
            }
        }
    }

    if requested_profile == SurfaceProfile::ApplicationPlatform {
        for comp in &pkg.base_manifest.components {
            if comp.role == ComponentRole::UiSurface {
                findings.push(CertifyFinding {
                    gate: GateId::Gate1ProtocolAndEffects,
                    rule_id: "SABI-CONFORM-001".to_owned(),
                    severity: CertifySeverity::Blocker,
                    subject: format!("package.manifest:{}", comp.line),
                    message: format!(
                        "component '{}' declares windowed role 'ui-surface', which is forbidden under APPLICATION_PLATFORM",
                        comp.name
                    ),
                    remediation:
                        "Target --profile desktop for windowed UI components or remove ui-surface from APPLICATION_PLATFORM packages."
                            .to_owned(),
                });
            }
        }

        for usage in &pkg.base_manifest.sabi_uses {
            let chan = usage.channel.to_ascii_lowercase();
            let sym = usage.symbol.to_ascii_lowercase();
            if chan.contains("ui_surface")
                || chan.contains("ui-surface")
                || chan.contains("workspace")
                || chan.contains("window_compositor")
                || chan.contains("clipboard_shell")
                || chan.contains("system_tray")
                || chan.contains("desktop")
                || sym.starts_with("uisurface")
                || sym.starts_with("windowcompositor")
                || sym.starts_with("desktop")
            {
                findings.push(CertifyFinding {
                    gate: GateId::Gate1ProtocolAndEffects,
                    rule_id: "SABI-CONFORM-001".to_owned(),
                    severity: CertifySeverity::Blocker,
                    subject: format!("package.manifest:{}", usage.line),
                    message: format!(
                        "sabi-use '{} {}' references desktop-exclusive SABI surface under APPLICATION_PLATFORM",
                        usage.channel, usage.symbol
                    ),
                    remediation:
                        "Remove desktop-exclusive SABI field usage or certify under DESKTOP profile."
                            .to_owned(),
                });
            }
        }
    }

    let mut uses_semantic_settlement = false;
    let mut settlement_line = 0_usize;
    let mut negotiates_resource_escrow = false;

    for binding in &pkg.directives.sabi_services {
        let dir_str = match binding.direction {
            SabiServiceDirection::Import => "import",
            SabiServiceDirection::Export => "export",
        };
        let Some(spec) = lookup_sabi_service(&binding.service_name) else {
            findings.push(CertifyFinding {
                gate: GateId::Gate1ProtocolAndEffects,
                rule_id: "SABI-CONFORM-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: format!("package.manifest:{}", binding.line_no),
                message: format!(
                    "sabi-service {dir_str} '{}' is not in the frozen SABI v1 service catalog",
                    binding.service_name
                ),
                remediation:
                    "Bind only frozen SABI v1 services (e.g. IDENTITY, CAPABILITY, TASK, ARTIFACT, SEMANTIC_DISPATCH, RESOURCE_ESCROW)."
                        .to_owned(),
            });
            continue;
        };

        if spec.desktop_exclusive && requested_profile == SurfaceProfile::ApplicationPlatform {
            findings.push(CertifyFinding {
                gate: GateId::Gate1ProtocolAndEffects,
                rule_id: "SABI-CONFORM-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: format!("package.manifest:{}", binding.line_no),
                message: format!(
                    "desktop-exclusive SABI v1 service '{}' ({dir_str}) cannot be bound under APPLICATION_PLATFORM",
                    spec.canonical_name
                ),
                remediation:
                    "Switch target profile to DESKTOP or remove desktop-only service bindings."
                        .to_owned(),
            });
        }

        for feat in &binding.features {
            if !spec
                .allowed_features
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(feat))
            {
                findings.push(CertifyFinding {
                    gate: GateId::Gate1ProtocolAndEffects,
                    rule_id: "SABI-CONFORM-001".to_owned(),
                    severity: CertifySeverity::Blocker,
                    subject: format!("package.manifest:{}", binding.line_no),
                    message: format!(
                        "feature '{feat}' is not a frozen SABI v1 feature of service '{}'",
                        spec.canonical_name
                    ),
                    remediation: format!(
                        "Allowed v1 features for {}: {}",
                        spec.canonical_name,
                        spec.allowed_features.join(", ")
                    ),
                });
            }
        }

        if spec.canonical_name == "SEMANTIC_SETTLEMENT" {
            uses_semantic_settlement = true;
            if settlement_line == 0 {
                settlement_line = binding.line_no;
            }
        }
        if spec.canonical_name == "RESOURCE_ESCROW"
            && binding.direction == SabiServiceDirection::Import
        {
            negotiates_resource_escrow = true;
        }
    }

    for usage in &pkg.base_manifest.sabi_uses {
        let chan = usage.channel.to_ascii_lowercase();
        let sym = usage.symbol.to_ascii_lowercase();
        if chan.contains("semantic_settlement")
            || chan.contains("settlement")
            || sym.contains("settlement")
        {
            uses_semantic_settlement = true;
            if settlement_line == 0 {
                settlement_line = usage.line;
            }
        }
        if chan.contains("resource_escrow") || chan.contains("escrow") || sym.contains("escrow") {
            negotiates_resource_escrow = true;
        }
    }

    if uses_semantic_settlement && !negotiates_resource_escrow {
        findings.push(CertifyFinding {
            gate: GateId::Gate1ProtocolAndEffects,
            rule_id: "SABI-CONFORM-001".to_owned(),
            severity: CertifySeverity::Blocker,
            subject: format!("package.manifest:{settlement_line}"),
            message:
                "package binds SEMANTIC_SETTLEMENT without importing/negotiating mandatory RESOURCE_ESCROW dependency"
                    .to_owned(),
            remediation:
                "Declare 'sabi-service = import RESOURCE_ESCROW' alongside SEMANTIC_SETTLEMENT so two-phase settlement holds verifiable resource escrow."
                    .to_owned(),
        });
    }
}

#[allow(clippy::too_many_lines)]
fn audit_effects_and_idempotency(
    pkg: &LoadedCertifyPackage,
    seen_rule_subjects: &BTreeSet<(String, String, String)>,
    findings: &mut Vec<CertifyFinding>,
) -> Result<(), CertifyError> {
    // Audit task template policy payloads for idempotency passthrough compliance.
    let mut task_templates_clean = true;
    for task in &pkg.base_manifest.tasks {
        let task_label = crate::json::hex_encode(&task.node_key);
        let policy_path = pkg.package_dir.join(&task.policy_path);
        if !policy_path.is_file() {
            task_templates_clean = false;
            findings.push(CertifyFinding {
                gate: GateId::Gate1ProtocolAndEffects,
                rule_id: "DRV-LIFE-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: format!("package.manifest:{}", task.line),
                message: format!(
                    "task '{task_label}' references missing policy artifact '{}'",
                    task.policy_path
                ),
                remediation:
                    "Ensure every task template policy artifact exists inside the package."
                        .to_owned(),
            });
            continue;
        }
        let policy_text = fs::read_to_string(&policy_path).map_err(|source| CertifyError::Io {
            context: format!("read {}", policy_path.display()),
            source,
        })?;
        for (idx, line) in policy_text.lines().enumerate() {
            let trimmed = line.trim();
            for token in trimmed.split_whitespace() {
                if matches!(
                    token,
                    "retry-idempotent=false"
                        | "idempotency-passthrough=false"
                        | "uncertain-reconcile=fresh-key"
                        | "reconcile=fresh-key-blind-retry"
                ) {
                    task_templates_clean = false;
                    findings.push(CertifyFinding {
                        gate: GateId::Gate1ProtocolAndEffects,
                        rule_id: "KABI-UNCERTAIN-001".to_owned(),
                        severity: CertifySeverity::Blocker,
                        subject: format!("{}:{}", task.policy_path, idx + 1),
                        message: format!(
                            "task '{task_label}' policy violates 128-bit IdempotencyKey passthrough / UNCERTAIN reconciliation ('{token}')"
                        ),
                        remediation:
                            "Set retry-idempotent=true and reconcile UNCERTAIN outcomes with the same 128-bit IdempotencyKey."
                                .to_owned(),
                    });
                }
            }
        }
    }

    // Check explicit `idempotency-passthrough` directives.
    for policy in &pkg.directives.idempotency_policies {
        let valid = matches!(
            policy.mode.as_str(),
            "128-bit" | "passthrough-128bit" | "true" | "required"
        );
        if !valid {
            findings.push(CertifyFinding {
                gate: GateId::Gate1ProtocolAndEffects,
                rule_id: "KABI-UNCERTAIN-001".to_owned(),
                severity: CertifySeverity::Blocker,
                subject: format!("package.manifest:{}", policy.line_no),
                message: format!(
                    "component '{}' declares invalid idempotency-passthrough mode '{}' (128-bit IdempotencyKey passthrough is mandatory)",
                    policy.component, policy.mode
                ),
                remediation:
                    "Declare 'idempotency-passthrough = <component> 128-bit' and preserve caller IdempotencyKey across retries."
                        .to_owned(),
            });
        }
    }

    // Check every Executable / Driver / BackgroundService component for EffectDescriptor completeness.
    for comp in &pkg.base_manifest.components {
        if !matches!(
            comp.role,
            ComponentRole::Executable | ComponentRole::Driver | ComponentRole::BackgroundService
        ) {
            continue;
        }
        let has_covering_effect = pkg
            .base_manifest
            .effects
            .iter()
            .any(|e| e.matches_component(&comp.name));

        if !has_covering_effect {
            // Legacy `entry = <name> <executable|background-service> <path>` packages
            // (such as `examples/starter-assistant-app/package`) predate Stage E
            // multi-component `effect = ...` lines and satisfy Gate 1 when their
            // declared task policy templates pass idempotency/CAS checks cleanly.
            if comp.is_legacy_entry && comp.role != ComponentRole::Driver && task_templates_clean {
                continue;
            }
            let subject = format!("package.manifest:{}", comp.line);
            let msg = format!(
                "component '{}' (role={}) lacks an explicit EffectDescriptor ('effect = {} <READ_ONLY|REVERSIBLE|COMPENSATABLE|IRREVERSIBLE> reconcile=same-idempotency-key')",
                comp.name,
                comp.role.as_str(),
                comp.name
            );
            if !seen_rule_subjects.contains(&(
                "DRV-LIFE-001".to_owned(),
                subject.clone(),
                msg.clone(),
            )) && !findings
                .iter()
                .any(|f| f.rule_id == "DRV-LIFE-001" && f.message.contains(&comp.name))
            {
                findings.push(CertifyFinding {
                    gate: GateId::Gate1ProtocolAndEffects,
                    rule_id: "DRV-LIFE-001".to_owned(),
                    severity: CertifySeverity::Blocker,
                    subject,
                    message: msg,
                    remediation: format!(
                        "Declare 'effect = {} <READ_ONLY|REVERSIBLE|COMPENSATABLE|IRREVERSIBLE> reconcile=same-idempotency-key'.",
                        comp.name
                    ),
                });
            }
        }
    }

    // Validate all EffectDescriptor classes & UNCERTAIN reconciliation strategies
    // (including Executable/BackgroundService mutating effects not covered by driver-only checks).
    for eff in &pkg.base_manifest.effects {
        let subject = format!("package.manifest:{}", eff.line);
        let is_mutating = matches!(
            eff.effect_class,
            EffectClass::Reversible | EffectClass::Compensatable | EffectClass::Irreversible
        );

        match &eff.reconcile {
            ReconcileDirective::None | ReconcileDirective::Omitted if is_mutating => {
                push_dedup_finding(
                    findings,
                    CertifyFinding {
                        gate: GateId::Gate1ProtocolAndEffects,
                        rule_id: "KABI-UNCERTAIN-001".to_owned(),
                        severity: CertifySeverity::Blocker,
                        subject,
                        message: format!(
                            "mutating target '{}' ({}) omits or disables same-idempotency-key UNCERTAIN reconciliation",
                            eff.target,
                            eff.effect_class.as_str()
                        ),
                        remediation:
                            "Declare reconcile=same-idempotency-key for all REVERSIBLE, COMPENSATABLE, and IRREVERSIBLE effects."
                                .to_owned(),
                    },
                );
            }
            ReconcileDirective::SameIdempotencyKey
            | ReconcileDirective::None
            | ReconcileDirective::Omitted => {}
            ReconcileDirective::ForbiddenPolicy(raw) => {
                push_dedup_finding(
                    findings,
                    CertifyFinding {
                        gate: GateId::Gate1ProtocolAndEffects,
                        rule_id: "KABI-UNCERTAIN-001".to_owned(),
                        severity: CertifySeverity::Blocker,
                        subject,
                        message: format!(
                            "target '{}' declares forbidden UNCERTAIN reconcile directive '{raw}'",
                            eff.target
                        ),
                        remediation:
                            "Switch to reconcile=same-idempotency-key so UNCERTAIN outcomes never duplicate side effects."
                                .to_owned(),
                    },
                );
            }
        }
    }

    Ok(())
}

fn push_dedup_finding(findings: &mut Vec<CertifyFinding>, candidate: CertifyFinding) {
    let already_present = findings.iter().any(|f| {
        f.rule_id == candidate.rule_id
            && (f.subject == candidate.subject || f.message == candidate.message)
    });
    if !already_present {
        findings.push(candidate);
    }
}
