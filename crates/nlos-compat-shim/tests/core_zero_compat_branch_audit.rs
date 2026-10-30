//! Standing static audit guarding Stage E Decision Point `E1` (`ADR-0027`
//! section 2.2): core NLOS authority crates MUST carry zero dependencies on
//! `nlos-compat-shim` and zero historical package compatibility branch tokens
//! (`target_sabi_epoch`, `driver_abi_epoch`, `SabiEpochAdapterCell`,
//! `DriverProtocolCompatShim`, `CompatShimMountDecision`).

use std::fs;
use std::path::{Path, PathBuf};

const CORE_AUTHORITY_CRATES: [&str; 6] = [
    "nlos-cell",
    "nlos-task",
    "nlos-capability",
    "nlos-resource",
    "nlos-consensus",
    "nlos-system-control",
];

const FORBIDDEN_CARGO_TOML_TOKENS: [&str; 2] = ["nlos-compat-shim", "nlos_compat_shim"];

const FORBIDDEN_RUST_SOURCE_TOKENS: [&str; 7] = [
    "nlos_compat_shim",
    "nlos-compat-shim",
    "target_sabi_epoch",
    "driver_abi_epoch",
    "SabiEpochAdapterCell",
    "DriverProtocolCompatShim",
    "CompatShimMountDecision",
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = fs::read_dir(dir).unwrap_or_else(|err| {
        panic!("failed to read directory {}: {err}", dir.display());
    });
    let mut paths: Vec<PathBuf> = entries
        .map(|entry| entry.expect("dir entry").path())
        .collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_rust_files(&path, out);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn core_authority_crates_have_zero_compat_shim_dependency_and_zero_historical_epoch_branches() {
    let crates_dir = workspace_root().join("crates");
    let mut total_scanned_rs_files = 0_usize;

    for crate_name in CORE_AUTHORITY_CRATES {
        let crate_root = crates_dir.join(crate_name);
        let cargo_toml_path = crate_root.join("Cargo.toml");
        let cargo_toml = fs::read_to_string(&cargo_toml_path).unwrap_or_else(|err| {
            panic!("failed to read {}: {err}", cargo_toml_path.display());
        });
        for forbidden in FORBIDDEN_CARGO_TOML_TOKENS {
            assert!(
                !cargo_toml.contains(forbidden),
                "core crate {crate_name} Cargo.toml must not depend on {forbidden}"
            );
        }

        let mut rs_files = Vec::new();
        collect_rust_files(&crate_root.join("src"), &mut rs_files);
        assert!(
            !rs_files.is_empty(),
            "core crate {crate_name} must contain at least one src/*.rs file"
        );
        total_scanned_rs_files += rs_files.len();

        for rs_path in rs_files {
            let source = fs::read_to_string(&rs_path).unwrap_or_else(|err| {
                panic!("failed to read {}: {err}", rs_path.display());
            });
            for forbidden in FORBIDDEN_RUST_SOURCE_TOKENS {
                assert!(
                    !source.contains(forbidden),
                    "core authority source {} polluted with historical compat token {forbidden:?}",
                    rs_path.display()
                );
            }
        }
    }

    assert!(
        total_scanned_rs_files >= 6,
        "expected to audit all six core crates; scanned {total_scanned_rs_files} .rs files"
    );
}
