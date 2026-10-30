//! `nlos-migrate` CLI (`Stage E W78 / E3`).
//!
//! Deterministic static `SABI` contract scanner, `UpgradePrescription` compiler,
//! and `[PKG-UPDATE-001]` three-mode migration & fault-rollback sandbox runner.
//!
//! # Exit Codes
//!
//! - `0`: Check, prescription, or expected drill passed
//! - `1`: CLI argument / usage error
//! - `2`: Manifest or `.nlospkg` package format corruption
//! - `3`: `SABI` contract sunset, extension, or dependency incompatibility block
//! - `4`: Irreversible migration missing pre-backup / `Z4` consent, or disguised irreversible
//! - `5`: Health check (`DRV-HEALTH-001`) or rollback drill failure

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nlos_migrate::{
    DataMigrationMode, DrillFaultInjection, DrillOutcomeKind, DrillPackagePair, DrillRequest,
    EphemeralIrreversibleProvisioning, ScanOptions, UpgradePrescription, json::hex_decode_fixed,
    json::json_string, run_migration_drill, scan_path,
};

const USAGE: &str = "\
usage:
  nlos-migrate scan <PATH> [--host-epoch <N>] [--manifest-yaml <FILE>] [--known-critical <ID,...>] [--json]
  nlos-migrate prescribe <PATH> [--host-epoch <N>] [--manifest-yaml <FILE>] [--known-critical <ID,...>] [--json]
  nlos-migrate drill --mode <window|snapshot-rollback|irreversible> [--sql <STMT>]... \\
    [--inject-crash-after-step <K>] [--inject-health-failure] [--expect-rollback] \\
    [--with-verified-backup] [--with-z4-consent] [--z4-consent-token <HEX64>] \\
    [--v1-pkg <FILE> --v2-pkg <FILE>] [--state-root <DIR>] [--json]";

const EXIT_PASS: u8 = 0;
const EXIT_USAGE: u8 = 1;
const EXIT_CORRUPT_INPUT: u8 = 2;
const EXIT_CONTRACT_BLOCK: u8 = 3;
const EXIT_IRREVERSIBLE_BLOCK: u8 = 4;
const EXIT_DRILL_FAILURE: u8 = 5;

struct ScanCliArgs {
    target_path: PathBuf,
    host_epoch: Option<u32>,
    manifest_yaml_path: Option<PathBuf>,
    known_critical: Vec<u32>,
    json_output: bool,
}

#[derive(Default)]
struct DrillFlags {
    expect_rollback: bool,
    json_output: bool,
}

struct DrillCliArgs {
    mode: DataMigrationMode,
    sql_steps: Vec<String>,
    fault_injection: DrillFaultInjection,
    flags: DrillFlags,
    ephemeral: EphemeralIrreversibleProvisioning,
    v1_pkg_path: Option<PathBuf>,
    v2_pkg_path: Option<PathBuf>,
    state_root: Option<PathBuf>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(subcmd) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return ExitCode::from(EXIT_USAGE);
    };
    let code = match subcmd {
        "scan" => cmd_scan_or_prescribe(&args[1..], false),
        "prescribe" => cmd_scan_or_prescribe(&args[1..], true),
        "drill" => cmd_drill(&args[1..]),
        "--help" | "-h" | "help" => {
            println!("{USAGE}");
            EXIT_PASS
        }
        _ => {
            eprintln!("unknown subcommand {subcmd:?}\n{USAGE}");
            EXIT_USAGE
        }
    };
    ExitCode::from(code)
}

fn cmd_scan_or_prescribe(args: &[String], emit_prescription: bool) -> u8 {
    let parsed = match parse_scan_args(args) {
        Ok(val) => val,
        Err(msg) => {
            eprintln!("argument error: {msg}\n{USAGE}");
            return EXIT_USAGE;
        }
    };

    let options = match build_scan_options(&parsed) {
        Ok(opts) => opts,
        Err(msg) => {
            emit_error(parsed.json_output, "E_CORRUPT_MANIFEST_INPUT", &msg);
            return EXIT_CORRUPT_INPUT;
        }
    };

    let report = match scan_path(&parsed.target_path, &options) {
        Ok(rep) => rep,
        Err(err) => {
            emit_error(
                parsed.json_output,
                "E_CORRUPT_PACKAGE_OR_MANIFEST",
                &err.to_string(),
            );
            return EXIT_CORRUPT_INPUT;
        }
    };

    if emit_prescription {
        let prescription = UpgradePrescription::from_scan_report(&report);
        if parsed.json_output {
            println!("{}", prescription.render_json());
        } else {
            print!("{}", prescription.render_text());
        }
    } else if parsed.json_output {
        println!("{}", report.render_json());
    } else {
        print!("{}", report.render_text());
    }

    if report.passed() {
        EXIT_PASS
    } else if report.has_disguised_irreversible() {
        EXIT_IRREVERSIBLE_BLOCK
    } else {
        EXIT_CONTRACT_BLOCK
    }
}

fn build_scan_options(parsed: &ScanCliArgs) -> Result<ScanOptions, String> {
    let mut options = if let Some(yaml_path) = &parsed.manifest_yaml_path {
        let yaml_text = fs::read_to_string(yaml_path)
            .map_err(|err| format!("read {}: {err}", yaml_path.display()))?;
        ScanOptions::from_manifest_yaml(parsed.host_epoch, &yaml_text)
            .map_err(|err| err.to_string())?
    } else {
        ScanOptions::canonical_for_host_epoch(parsed.host_epoch.unwrap_or(1))
            .map_err(|err| err.to_string())?
    };
    options
        .known_critical_extensions
        .clone_from(&parsed.known_critical);
    Ok(options)
}

fn parse_scan_args(args: &[String]) -> Result<ScanCliArgs, String> {
    let mut target_path: Option<PathBuf> = None;
    let mut host_epoch: Option<u32> = None;
    let mut manifest_yaml_path: Option<PathBuf> = None;
    let mut known_critical: Vec<u32> = Vec::new();
    let mut json_output = false;

    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--json" => {
                json_output = true;
                idx += 1;
            }
            "--host-epoch" => {
                let raw = next_arg(args, idx, "--host-epoch")?;
                host_epoch = Some(
                    raw.parse::<u32>()
                        .map_err(|err| format!("invalid --host-epoch {raw:?}: {err}"))?,
                );
                idx += 2;
            }
            "--manifest-yaml" => {
                let raw = next_arg(args, idx, "--manifest-yaml")?;
                manifest_yaml_path = Some(PathBuf::from(raw));
                idx += 2;
            }
            "--known-critical" => {
                let raw = next_arg(args, idx, "--known-critical")?;
                known_critical = parse_u32_list(raw)?;
                idx += 2;
            }
            token if token.starts_with('-') => {
                return Err(format!("unknown flag {token:?}"));
            }
            positional => {
                if target_path.is_some() {
                    return Err(format!(
                        "unexpected extra positional argument {positional:?}"
                    ));
                }
                target_path = Some(PathBuf::from(positional));
                idx += 1;
            }
        }
    }

    let target_path = target_path.ok_or_else(|| "missing required <PATH> argument".to_owned())?;
    Ok(ScanCliArgs {
        target_path,
        host_epoch,
        manifest_yaml_path,
        known_critical,
        json_output,
    })
}

fn cmd_drill(args: &[String]) -> u8 {
    let parsed = match parse_drill_args(args) {
        Ok(val) => val,
        Err(msg) => {
            eprintln!("argument error: {msg}\n{USAGE}");
            return EXIT_USAGE;
        }
    };

    let packages =
        match load_drill_packages(parsed.v1_pkg_path.as_deref(), parsed.v2_pkg_path.as_deref()) {
            Ok(pair) => pair,
            Err(msg) => {
                emit_error(parsed.flags.json_output, "E_CORRUPT_DRILL_PACKAGE", &msg);
                return EXIT_CORRUPT_INPUT;
            }
        };

    let req = DrillRequest {
        mode: parsed.mode,
        packages,
        sql_steps: parsed.sql_steps,
        fault_injection: parsed.fault_injection,
        state_root: parsed.state_root,
        pre_backup_receipt: None,
        z4_consent_receipt: None,
        ephemeral_irreversible: parsed.ephemeral,
    };

    match run_migration_drill(&req) {
        Ok(report) => {
            if parsed.flags.json_output {
                println!("{}", report.render_json());
            } else {
                print!("{}", report.render_text());
            }
            match report.outcome {
                DrillOutcomeKind::RolledBackOnHealthFailure => {
                    if parsed.flags.expect_rollback
                        && report.userdata_matches_pre_upgrade
                        && report.final_generation == 1
                    {
                        EXIT_PASS
                    } else {
                        EXIT_DRILL_FAILURE
                    }
                }
                DrillOutcomeKind::ActivatedGeneration2
                | DrillOutcomeKind::RecoveredCrashAndActivatedGeneration2 { .. } => {
                    if parsed.flags.expect_rollback {
                        EXIT_DRILL_FAILURE
                    } else {
                        EXIT_PASS
                    }
                }
            }
        }
        Err(err) => {
            let code = if err.is_irreversible_governance_block() {
                "E_IRREVERSIBLE_GOVERNANCE_BLOCK"
            } else {
                "E_MIGRATION_DRILL_FAILURE"
            };
            emit_error(parsed.flags.json_output, code, &err.to_string());
            if err.is_irreversible_governance_block() {
                EXIT_IRREVERSIBLE_BLOCK
            } else {
                EXIT_DRILL_FAILURE
            }
        }
    }
}

fn load_drill_packages(
    v1_path: Option<&Path>,
    v2_path: Option<&Path>,
) -> Result<DrillPackagePair, String> {
    match (v1_path, v2_path) {
        (None, None) => Ok(DrillPackagePair::canonical_fixture()),
        (Some(p1), Some(p2)) => {
            let b1 = fs::read(p1).map_err(|err| format!("read {}: {err}", p1.display()))?;
            let b2 = fs::read(p2).map_err(|err| format!("read {}: {err}", p2.display()))?;
            DrillPackagePair::from_package_bytes(&b1, &b2)
        }
        _ => Err("must provide both --v1-pkg and --v2-pkg or neither".to_owned()),
    }
}

fn parse_drill_args(args: &[String]) -> Result<DrillCliArgs, String> {
    let mut mode: Option<DataMigrationMode> = None;
    let mut sql_steps: Vec<String> = Vec::new();
    let mut fault_injection = DrillFaultInjection::None;
    let mut flags = DrillFlags::default();
    let mut ephemeral = EphemeralIrreversibleProvisioning::default();
    let mut v1_pkg_path: Option<PathBuf> = None;
    let mut v2_pkg_path: Option<PathBuf> = None;
    let mut state_root: Option<PathBuf> = None;

    let mut idx = 0usize;
    while idx < args.len() {
        match args[idx].as_str() {
            "--mode" => {
                let raw = next_arg(args, idx, "--mode")?;
                mode = Some(DataMigrationMode::parse(raw)?);
                idx += 2;
            }
            "--sql" => {
                sql_steps.push(next_arg(args, idx, "--sql")?.to_owned());
                idx += 2;
            }
            "--inject-crash-after-step" => {
                let raw = next_arg(args, idx, "--inject-crash-after-step")?;
                let step = raw
                    .parse::<u64>()
                    .map_err(|err| format!("invalid --inject-crash-after-step: {err}"))?;
                fault_injection = DrillFaultInjection::CrashAfterStep(step);
                idx += 2;
            }
            "--inject-health-failure" => {
                fault_injection = DrillFaultInjection::PostMigrationHealthFailure;
                idx += 1;
            }
            "--expect-rollback" => {
                flags.expect_rollback = true;
                idx += 1;
            }
            "--with-verified-backup" => {
                ephemeral.create_verified_backup = true;
                idx += 1;
            }
            "--with-z4-consent" => {
                ephemeral.issue_z4_consent = true;
                idx += 1;
            }
            "--z4-consent-token" => {
                let raw = next_arg(args, idx, "--z4-consent-token")?;
                ephemeral.issue_z4_consent = true;
                ephemeral.override_z4_token =
                    Some(hex_decode_fixed::<32>(raw, "--z4-consent-token")?);
                idx += 2;
            }
            "--v1-pkg" => {
                v1_pkg_path = Some(PathBuf::from(next_arg(args, idx, "--v1-pkg")?));
                idx += 2;
            }
            "--v2-pkg" => {
                v2_pkg_path = Some(PathBuf::from(next_arg(args, idx, "--v2-pkg")?));
                idx += 2;
            }
            "--state-root" => {
                state_root = Some(PathBuf::from(next_arg(args, idx, "--state-root")?));
                idx += 2;
            }
            "--json" => {
                flags.json_output = true;
                idx += 1;
            }
            other => {
                return Err(format!("unknown drill argument {other:?}"));
            }
        }
    }

    if v1_pkg_path.is_some() != v2_pkg_path.is_some() {
        return Err("--v1-pkg and --v2-pkg must be supplied together".to_owned());
    }
    let mode = mode.ok_or_else(|| "missing required `--mode <MODE>`".to_owned())?;
    Ok(DrillCliArgs {
        mode,
        sql_steps,
        fault_injection,
        flags,
        ephemeral,
        v1_pkg_path,
        v2_pkg_path,
        state_root,
    })
}

fn next_arg<'a>(args: &'a [String], idx: usize, flag: &str) -> Result<&'a str, String> {
    args.get(idx + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("missing value for {flag}"))
}

fn parse_u32_list(raw: &str) -> Result<Vec<u32>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }
    trimmed
        .split(',')
        .map(|part| {
            part.trim()
                .parse::<u32>()
                .map_err(|err| format!("invalid extension id {part:?}: {err}"))
        })
        .collect()
}

fn emit_error(json_output: bool, code: &str, message: &str) {
    if json_output {
        println!(
            "{{\n  \"code\": {},\n  \"message\": {},\n  \"passed\": false\n}}",
            json_string(code),
            json_string(message),
        );
    } else {
        eprintln!("ERROR [{code}]: {message}");
    }
}
