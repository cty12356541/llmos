//! `nlos-migrate` CLI binary:
//! - `nlos-migrate check <PACKAGE_DIR> [--manifest-yaml <YAML_PATH>] [--host-epoch <U32>] [--json]`
//! - `nlos-migrate drill --mode <window|snapshot|irreversible> [--inject-health-fail] [--inject-step-crash] [--omit-backup] [--omit-z4-consent] [--disguise-destructive] [--json]`
//!
//! Exit codes:
//! - `0`: compliant package / migration drill passed
//! - `1`: CLI usage or input error
//! - `2`: static check detected at least one `BLOCKER` violation
//! - `3`: migration sandbox drill blocked by safety gate or failed

#![forbid(unsafe_code)]

use std::env;
use std::fs;
use std::path::Path;
use std::process::ExitCode;

use nlos_migrate::{
    AuditOptions, CliDrillScenario, MigrationMode, audit_package_directory,
    audit_package_directory_with_yaml, execute_ephemeral_cli_drill, render_drill_error,
};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let code = run_cli(&args);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn run_cli(args: &[String]) -> i32 {
    let Some((subcommand, tail)) = args.split_first() else {
        eprintln!("{}", usage_text());
        return 1;
    };

    match subcommand.as_str() {
        "-h" | "--help" | "help" => {
            println!("{}", usage_text());
            0
        }
        "check" => run_check_command(tail),
        "drill" => run_drill_command(tail),
        unknown => {
            eprintln!("error: unknown subcommand {unknown:?}\n\n{}", usage_text());
            1
        }
    }
}

fn run_check_command(args: &[String]) -> i32 {
    let mut package_dir: Option<String> = None;
    let mut manifest_yaml_path: Option<String> = None;
    let mut host_epoch: Option<u32> = None;
    let mut as_json = false;

    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "-h" | "--help" => {
                println!("{}", usage_text());
                return 0;
            }
            "--json" => {
                as_json = true;
                idx += 1;
            }
            "--manifest-yaml" => {
                let Some(val) = args.get(idx + 1) else {
                    eprintln!("error: --manifest-yaml requires a <YAML_PATH> argument");
                    return 1;
                };
                manifest_yaml_path = Some(val.clone());
                idx += 2;
            }
            "--host-epoch" => {
                let Some(val) = args.get(idx + 1) else {
                    eprintln!("error: --host-epoch requires a <U32> argument");
                    return 1;
                };
                let Ok(parsed) = val.parse::<u32>() else {
                    eprintln!(
                        "error: invalid --host-epoch {val:?}; expected unsigned 32-bit integer"
                    );
                    return 1;
                };
                host_epoch = Some(parsed);
                idx += 2;
            }
            flag if flag.starts_with('-') => {
                eprintln!("error: unknown flag {flag:?} for `nlos-migrate check`");
                return 1;
            }
            positional => {
                if package_dir.is_some() {
                    eprintln!("error: unexpected extra positional argument {positional:?}");
                    return 1;
                }
                package_dir = Some(positional.to_owned());
                idx += 1;
            }
        }
    }

    let Some(package_dir) = package_dir else {
        eprintln!(
            "error: `nlos-migrate check` requires <PACKAGE_DIR>\n\n{}",
            usage_text()
        );
        return 1;
    };

    let dir_path = Path::new(&package_dir);
    let Ok(meta) = fs::metadata(dir_path) else {
        eprintln!("error: package directory {package_dir:?} does not exist");
        return 1;
    };
    if !meta.is_dir() {
        eprintln!("error: {package_dir:?} is not a directory");
        return 1;
    }

    let options = AuditOptions {
        host_epoch,
        max_backward_epochs: None,
        known_critical_extensions: Vec::new(),
    };

    let report = if let Some(yaml_path) = manifest_yaml_path {
        let yaml_text = match fs::read_to_string(&yaml_path) {
            Ok(content) => content,
            Err(err) => {
                eprintln!("error: failed to read --manifest-yaml {yaml_path:?}: {err}");
                return 1;
            }
        };
        match audit_package_directory_with_yaml(dir_path, &yaml_text, &options) {
            Ok(rep) => rep,
            Err(err) => {
                eprintln!("error: invalid deprecation manifest {yaml_path:?}: {err}");
                return 1;
            }
        }
    } else {
        audit_package_directory(dir_path, &options)
    };

    if as_json {
        print!("{}", report.render_json());
    } else {
        print!("{}", report.render_text());
    }
    report.verdict.exit_code()
}

fn run_drill_command(args: &[String]) -> i32 {
    let mut mode: Option<MigrationMode> = None;
    let mut inject_health_fail = false;
    let mut inject_step_crash = false;
    let mut omit_backup = false;
    let mut omit_z4_consent = false;
    let mut disguise_destructive = false;
    let mut as_json = false;

    let mut idx = 0;
    while idx < args.len() {
        match args[idx].as_str() {
            "-h" | "--help" => {
                println!("{}", usage_text());
                return 0;
            }
            "--mode" => {
                let Some(val) = args.get(idx + 1) else {
                    eprintln!("error: --mode requires <window|snapshot|irreversible>");
                    return 1;
                };
                let Some(parsed) = MigrationMode::parse_cli(val) else {
                    eprintln!(
                        "error: invalid --mode {val:?}; expected `window`, `snapshot`, or `irreversible`"
                    );
                    return 1;
                };
                mode = Some(parsed);
                idx += 2;
            }
            "--inject-health-fail" => {
                inject_health_fail = true;
                idx += 1;
            }
            "--inject-step-crash" => {
                inject_step_crash = true;
                idx += 1;
            }
            "--omit-backup" => {
                omit_backup = true;
                idx += 1;
            }
            "--omit-z4-consent" => {
                omit_z4_consent = true;
                idx += 1;
            }
            "--disguise-destructive" => {
                disguise_destructive = true;
                idx += 1;
            }
            "--json" => {
                as_json = true;
                idx += 1;
            }
            other => {
                eprintln!("error: unknown argument {other:?} for `nlos-migrate drill`");
                return 1;
            }
        }
    }

    let Some(mode) = mode else {
        eprintln!(
            "error: `nlos-migrate drill` requires `--mode <window|snapshot|irreversible>`\n\n{}",
            usage_text()
        );
        return 1;
    };

    let scenario = CliDrillScenario {
        mode,
        inject_health_fail,
        inject_step_crash,
        omit_backup,
        omit_z4_consent,
        disguise_destructive,
    };

    match execute_ephemeral_cli_drill(scenario) {
        Ok(receipt) => {
            if as_json {
                print!("{}", receipt.render_json());
            } else {
                print!("{}", receipt.render_text());
            }
            0
        }
        Err(err) => {
            let rendered = render_drill_error(mode, &err, as_json);
            if as_json {
                print!("{rendered}");
            } else {
                eprint!("{rendered}");
            }
            3
        }
    }
}

fn usage_text() -> &'static str {
    "nlos-migrate: Stage E SABI/Driver/Package migration auditor & rollback sandbox

USAGE:
  nlos-migrate check <PACKAGE_DIR> [--manifest-yaml <YAML_PATH>] [--host-epoch <U32>] [--json]
  nlos-migrate drill --mode <window|snapshot|irreversible> [--inject-health-fail] [--inject-step-crash] [--omit-backup] [--omit-z4-consent] [--disguise-destructive] [--json]

EXIT CODES:
  0  Compliant package or migration sandbox drill passed
  1  CLI usage error
  2  Static audit detected at least one BLOCKER violation
  3  Migration sandbox drill blocked by safety gate or failed"
}
