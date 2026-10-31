//! `nlos-certify` CLI — Offline 4-gate store certification runner and
//! dependency license / lockfile supply-chain auditor (`ADR-0027` §3).

#![forbid(unsafe_code)]

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use ed25519_dalek::SigningKey;
use nlos_capability::{CustodyTier, ProvisionCustodyKeyRequest, SigningKeyCustodyVault};
use nlos_certify::{
    CertifyPipelineConfig, LoadedCertifyPackage, PublisherSubmissionProof,
    StaticPublisherTrustVerifier, StoreCertifierSigner, SupplyChainPolicy, SurfaceProfile,
    audit_dependency_supply_chain, derive_publisher_id_from_str, run_certification_pipeline,
    verify_store_certification_receipt,
};
use nlos_identity::KeyPurpose;
use nlos_publisher_trust::{PublisherTrustTier, RevocationEpoch};
use nlos_types::{ControlDomainId, KeyId, PrincipalId};

const CLI_CERTIFIER_ISSUED_AT_MS: u64 = 1_761_868_800_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
}

impl OutputFormat {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "text" | "TEXT" => Some(Self::Text),
            "json" | "JSON" => Some(Self::Json),
            _ => None,
        }
    }
}

fn print_help() {
    println!(
        "\
nlos-certify -- NLOS Automated 4-Gate Store Certification & Supply-Chain Auditor

USAGE:
  nlos-certify certify --package-dir <DIR> [--upgrade-dir <DIR>] [--profile <application-platform|desktop>] [--format <text|json>]
  nlos-certify supply-chain --root <DIR> [--format <text|json>]
  nlos-certify --help

SUBCOMMANDS:
  certify        Run Gate 1..Gate 4 on <DIR> and issue a signed StoreCertificationReceipt on 4/4 PASS
  supply-chain   Run pure-Rust SPDX license, checksum, and lockfile pinning audit (RISK-B-09) on <DIR>

EXIT CODES:
  0  All gates / supply-chain checks passed
  2  Policy or conformance gate blocked (FAIL / BLOCKER)
  1  Invalid CLI arguments or I/O error"
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h" | "help") {
        print_help();
        return ExitCode::SUCCESS;
    }

    match args[0].as_str() {
        "certify" => run_certify_cmd(&args[1..]),
        "supply-chain" => run_supply_chain_cmd(&args[1..]),
        unknown => {
            eprintln!("error: unknown subcommand '{unknown}' (try 'nlos-certify --help')");
            ExitCode::from(1)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn run_certify_cmd(args: &[String]) -> ExitCode {
    let mut package_dir: Option<PathBuf> = None;
    let mut upgrade_dir: Option<PathBuf> = None;
    let mut profile_override: Option<SurfaceProfile> = None;
    let mut format = OutputFormat::Text;

    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--help" | "-h" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            "--package-dir" => {
                let Some(val) = args.get(idx + 1) else {
                    eprintln!("error: --package-dir requires <DIR>");
                    return ExitCode::from(1);
                };
                package_dir = Some(PathBuf::from(val));
                idx += 2;
            }
            "--upgrade-dir" => {
                let Some(val) = args.get(idx + 1) else {
                    eprintln!("error: --upgrade-dir requires <DIR>");
                    return ExitCode::from(1);
                };
                upgrade_dir = Some(PathBuf::from(val));
                idx += 2;
            }
            "--profile" => {
                let Some(val) = args.get(idx + 1) else {
                    eprintln!("error: --profile requires <application-platform|desktop>");
                    return ExitCode::from(1);
                };
                let Some(parsed) = SurfaceProfile::parse(val) else {
                    eprintln!(
                        "error: invalid --profile '{val}' (expected 'application-platform' or 'desktop')"
                    );
                    return ExitCode::from(1);
                };
                profile_override = Some(parsed);
                idx += 2;
            }
            "--format" => {
                let Some(val) = args.get(idx + 1) else {
                    eprintln!("error: --format requires <text|json>");
                    return ExitCode::from(1);
                };
                let Some(parsed) = OutputFormat::parse(val) else {
                    eprintln!("error: invalid --format '{val}' (expected 'text' or 'json')");
                    return ExitCode::from(1);
                };
                format = parsed;
                idx += 2;
            }
            other => {
                eprintln!("error: unexpected argument '{other}' for 'certify'");
                return ExitCode::from(1);
            }
        }
    }

    let Some(pkg_dir) = package_dir else {
        eprintln!("error: missing required --package-dir <DIR>");
        return ExitCode::from(1);
    };

    let preloaded = match LoadedCertifyPackage::load(&pkg_dir) {
        Ok(pkg) => pkg,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::from(2);
        }
    };

    let effective_profile = profile_override
        .or_else(|| {
            preloaded
                .directives
                .declared_surface_profile
                .as_ref()
                .and_then(|(s, _)| SurfaceProfile::parse(s))
        })
        .unwrap_or(SurfaceProfile::ApplicationPlatform);

    let publisher_label = preloaded
        .directives
        .publisher_id_hint
        .as_deref()
        .unwrap_or("nlos.local.verified-publisher");
    let publisher_id = derive_publisher_id_from_str(publisher_label);
    let publisher_signing_key = SigningKey::from_bytes(&[0x53_u8; 32]);
    let submission_proof =
        PublisherSubmissionProof::for_package(publisher_id, &publisher_signing_key, &preloaded);

    let trust_verifier = StaticPublisherTrustVerifier::single_verified_owner(
        publisher_id,
        PublisherTrustTier::CertifiedPublisher,
        publisher_signing_key.verifying_key().to_bytes(),
        preloaded.application_id,
        RevocationEpoch::new(1),
        CLI_CERTIFIER_ISSUED_AT_MS,
    );

    let vault = match SigningKeyCustodyVault::new_in_memory([0xA5_u8; 32]) {
        Ok(v) => v,
        Err(err) => {
            eprintln!("error: initialize custody vault: {err}");
            return ExitCode::from(1);
        }
    };
    let certifier_key_id = KeyId::from_bytes([0xCE_u8; 16]);
    let slot = match vault.provision_key_slot(ProvisionCustodyKeyRequest {
        key_id: certifier_key_id,
        principal_id: PrincipalId::from_bytes([0xCF_u8; 16]),
        control_domain_id: ControlDomainId::from_bytes([0xCD_u8; 16]),
        authority_id: [0xCA_u8; 16],
        key_purpose: KeyPurpose::SemanticSigning,
        custody_tier: CustodyTier::OperatorFileSealed,
        rotation_epoch: 1,
        valid_from_ms: 1_000,
        valid_until_ms: u64::MAX / 2,
        seed: [0x7B_u8; 32],
        provisioned_at_ms: 1_000,
    }) {
        Ok(s) => s,
        Err(err) => {
            eprintln!("error: provision certifier custody key: {err}");
            return ExitCode::from(1);
        }
    };

    let signer = StoreCertifierSigner::CustodyVault {
        vault: &vault,
        key_id: certifier_key_id,
        expected_rotation_epoch: Some(1),
    };
    let config = CertifyPipelineConfig::new(
        submission_proof,
        effective_profile,
        CLI_CERTIFIER_ISSUED_AT_MS,
    );

    let report = match run_certification_pipeline(
        &pkg_dir,
        upgrade_dir.as_deref(),
        &config,
        &trust_verifier,
        &signer,
    ) {
        Ok(rep) => rep,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::from(2);
        }
    };

    if let Some(ref receipt) = report.receipt
        && let Err(err) = verify_store_certification_receipt(
            receipt,
            Some(&slot.public_key),
            CLI_CERTIFIER_ISSUED_AT_MS,
        )
    {
        eprintln!("error: self-verification of issued receipt failed: {err}");
        return ExitCode::from(1);
    }

    match format {
        OutputFormat::Text => print!("{}", report.render_text()),
        OutputFormat::Json => println!("{}", report.render_json()),
    }

    if report.certified() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

fn run_supply_chain_cmd(args: &[String]) -> ExitCode {
    let mut root_dir: Option<PathBuf> = None;
    let mut format = OutputFormat::Text;

    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "--help" | "-h" => {
                print_help();
                return ExitCode::SUCCESS;
            }
            "--root" => {
                let Some(val) = args.get(idx + 1) else {
                    eprintln!("error: --root requires <DIR>");
                    return ExitCode::from(1);
                };
                root_dir = Some(PathBuf::from(val));
                idx += 2;
            }
            "--format" => {
                let Some(val) = args.get(idx + 1) else {
                    eprintln!("error: --format requires <text|json>");
                    return ExitCode::from(1);
                };
                let Some(parsed) = OutputFormat::parse(val) else {
                    eprintln!("error: invalid --format '{val}' (expected 'text' or 'json')");
                    return ExitCode::from(1);
                };
                format = parsed;
                idx += 2;
            }
            other => {
                eprintln!("error: unexpected argument '{other}' for 'supply-chain'");
                return ExitCode::from(1);
            }
        }
    }

    let Some(root) = root_dir else {
        eprintln!("error: missing required --root <DIR>");
        return ExitCode::from(1);
    };

    let policy = SupplyChainPolicy::default();
    let report = match audit_dependency_supply_chain(&root, &policy) {
        Ok(rep) => rep,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::from(1);
        }
    };

    match format {
        OutputFormat::Text => print!("{}", report.render_text()),
        OutputFormat::Json => println!("{}", report.render_json()),
    }

    if report.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}
