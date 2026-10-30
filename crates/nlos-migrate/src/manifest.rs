//! Package source directory (`package.manifest` + `migration.manifest`) and
//! built `nlos/package-file/v1` (`.nlospkg` / `.nlos-pkg`) loader.

use std::error::Error;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use nlos_artifact::{PACKAGE_FILE_MAGIC, check_package_file, decode_package_file};
use nlos_types::PackageId;

use crate::json::hex_decode_fixed;

/// Statutory three-mode data container migration declaration (`[PKG-UPDATE-001]`, `ADR-0027`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataMigrationMode {
    /// Additive schema window readable and writable by both `v1` and `v2`.
    BackwardCompatibleWindow,
    /// Reversible upgrade backed by a pre-upgrade `SHA-256` `UserData` snapshot.
    SnapshotAssistedRollback,
    /// Controlled destructive upgrade gated by pre-backup + `Z4` `Trusted UI` consent.
    Irreversible,
}

impl DataMigrationMode {
    /// Canonical uppercase wire/manifest identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BackwardCompatibleWindow => "BACKWARD_COMPATIBLE_WINDOW",
            Self::SnapshotAssistedRollback => "SNAPSHOT_ASSISTED_ROLLBACK",
            Self::Irreversible => "IRREVERSIBLE",
        }
    }

    /// Parses a manifest or CLI token into [`DataMigrationMode`].
    ///
    /// # Errors
    ///
    /// Returns an error string when `raw` does not name one of the three modes.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "BACKWARD_COMPATIBLE_WINDOW" | "window" | "backward-compatible-window" => {
                Ok(Self::BackwardCompatibleWindow)
            }
            "SNAPSHOT_ASSISTED_ROLLBACK" | "snapshot-rollback" | "snapshot-assisted-rollback" => {
                Ok(Self::SnapshotAssistedRollback)
            }
            "IRREVERSIBLE" | "irreversible" => Ok(Self::Irreversible),
            other => Err(format!(
                "unsupported data migration mode {other:?}; expected BACKWARD_COMPATIBLE_WINDOW, \
                 SNAPSHOT_ASSISTED_ROLLBACK, or IRREVERSIBLE"
            )),
        }
    }
}

/// Statutory four-class `EffectDescriptor` classification (`06-架构设计总纲-v0.5.md`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectClassDeclaration {
    /// Side-effect-free read operation (`READ_ONLY`).
    ReadOnly,
    /// Locally undoable mutation (`REVERSIBLE`).
    Reversible,
    /// Mutation requiring an explicit compensation handler (`COMPENSATABLE`).
    Compensatable,
    /// Externally visible or destructive mutation requiring `Z4` confirmation (`IRREVERSIBLE`).
    Irreversible,
}

impl EffectClassDeclaration {
    /// Canonical uppercase identifier.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "READ_ONLY",
            Self::Reversible => "REVERSIBLE",
            Self::Compensatable => "COMPENSATABLE",
            Self::Irreversible => "IRREVERSIBLE",
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim() {
            "READ_ONLY" => Ok(Self::ReadOnly),
            "REVERSIBLE" => Ok(Self::Reversible),
            "COMPENSATABLE" => Ok(Self::Compensatable),
            "IRREVERSIBLE" => Ok(Self::Irreversible),
            other => Err(format!(
                "invalid EffectDescriptor class {other:?}; expected READ_ONLY, REVERSIBLE, \
                 COMPENSATABLE, or IRREVERSIBLE"
            )),
        }
    }
}

/// One declared `EffectDescriptor` binding inside a package migration manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclaredEffect {
    /// Stable package-local operation or capability identifier.
    pub effect_id: String,
    /// Declared four-class effect category.
    pub effect_class: EffectClassDeclaration,
    /// Optional compensation handler identifier (mandatory when `Compensatable`).
    pub compensation_handler: Option<String>,
}

/// One `SABI` channel negotiated by a third-party package.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclaredChannelUsage {
    /// Canonical channel name (e.g. `nlos.sabi.Envelope`).
    pub channel: String,
    /// Target `SABI` major epoch for this channel.
    pub major: u32,
    /// Target `SABI` minor revision for this channel.
    pub minor: u32,
    /// Channel-specific critical extension IDs (`[COMPAT-VER-002]`).
    pub critical_extensions: Vec<u32>,
    /// Channel-specific non-critical extension IDs (`[COMPAT-VER-002]`).
    pub non_critical_extensions: Vec<u32>,
}

/// One referenced `SABI` message, method, field, enum value, or extension symbol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclaredSymbolRef {
    /// Owning `SABI` channel name.
    pub channel: String,
    /// Referenced symbol identifier (e.g. `Envelope.trace_id` or `ExecuteCapability`).
    pub symbol: String,
    /// Enclosing `.proto` message or enum name when referencing a field/enum value.
    pub parent_type: Option<String>,
    /// Optional `.proto` field number when referencing a numbered field.
    pub field_number: Option<u32>,
}

/// One ecosystem dependency selector declared by a package (`[PLAN-DEPENDENCY-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclaredDependencySelector {
    /// Ecosystem entity kind (`package`, `skill`, `tool`, `model`, `artifact`, `topic`, `service`).
    pub entity_kind: String,
    /// Raw selector expression (must be generation/digest/version pinned, never `latest`).
    pub raw_selector: String,
}

/// Origin kind of an inspected package target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetSourceKind {
    /// Developer package directory containing `package.manifest` (+ optional `migration.manifest`).
    SourceDirectory,
    /// Standalone manifest file (`package.manifest` or `migration.manifest`).
    ManifestFile,
    /// Signed binary `nlos/package-file/v1` archive (`.nlospkg` / `.nlos-pkg`).
    BuiltPackageFile,
}

impl TargetSourceKind {
    /// Canonical lowercase identifier for CLI/JSON reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SourceDirectory => "source_directory",
            Self::ManifestFile => "manifest_file",
            Self::BuiltPackageFile => "built_package_file",
        }
    }
}

/// Normalized package contract and migration metadata extracted for static scanning.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InspectedPackageTarget {
    /// How the target was loaded.
    pub source_kind: TargetSourceKind,
    /// Declared 16-byte package identity.
    pub package_id: PackageId,
    /// Unpacked `(major, minor, patch)` package version.
    pub version: (u32, u32, u32),
    /// Declared payload entry names.
    pub entry_names: Vec<String>,
    /// Package-wide target `SABI` major epoch (`N`).
    pub target_sabi_epoch: u32,
    /// Package-wide target `SABI` minor version.
    pub target_sabi_minor: u32,
    /// Optional target driver ABI epoch (`DRV-*` boundary).
    pub driver_abi_epoch: Option<u32>,
    /// Package-wide critical `SABI` extension identifiers.
    pub critical_extensions: Vec<u32>,
    /// Package-wide non-critical `SABI` extension identifiers.
    pub non_critical_extensions: Vec<u32>,
    /// Explicit per-channel `SABI` requirements.
    pub channels: Vec<DeclaredChannelUsage>,
    /// Referenced `SABI` symbols and numbered fields.
    pub symbols: Vec<DeclaredSymbolRef>,
    /// Declared four-class `EffectDescriptor` entries.
    pub effects: Vec<DeclaredEffect>,
    /// Declared ecosystem dependency selectors.
    pub dependencies: Vec<DeclaredDependencySelector>,
    /// Optional `[PKG-UPDATE-001]` data container migration mode.
    pub data_migration_mode: Option<DataMigrationMode>,
    /// Ordered `SQL` / data migration steps declared for package upgrade.
    pub migration_steps: Vec<String>,
}

/// Fail-closed manifest or binary package loading error (maps to CLI exit code `2`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestLoadError {
    /// Filesystem read error.
    Io { path: PathBuf, detail: String },
    /// Directory lacked `package.manifest`.
    MissingManifest { path: PathBuf },
    /// Line-level syntax or structural error in a manifest file.
    SyntaxError {
        path: PathBuf,
        line: usize,
        message: String,
    },
    /// Binary `.nlospkg` framing or UTF-8 embedded manifest decode error.
    CorruptPackageFile { path: PathBuf, detail: String },
    /// Binary `.nlospkg` failed `nlos_artifact::check_package_file` conformance rules.
    PackageConformanceFailed {
        path: PathBuf,
        findings: Vec<String>,
    },
}

impl fmt::Display for ManifestLoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, detail } => {
                write!(formatter, "failed to read {}: {detail}", path.display())
            }
            Self::MissingManifest { path } => write!(
                formatter,
                "directory {} is missing package.manifest",
                path.display()
            ),
            Self::SyntaxError {
                path,
                line,
                message,
            } => write!(
                formatter,
                "manifest syntax error in {}:{line}: {message}",
                path.display()
            ),
            Self::CorruptPackageFile { path, detail } => write!(
                formatter,
                "corrupt package file {}: {detail}",
                path.display()
            ),
            Self::PackageConformanceFailed { path, findings } => write!(
                formatter,
                "package conformance failed for {}: {}",
                path.display(),
                findings.join("; ")
            ),
        }
    }
}

impl Error for ManifestLoadError {}

#[derive(Default)]
struct PartialManifestOverlay {
    package_id: Option<PackageId>,
    version: Option<(u32, u32, u32)>,
    entry_names: Vec<String>,
    target_sabi_epoch: Option<u32>,
    target_sabi_minor: Option<u32>,
    driver_abi_epoch: Option<u32>,
    critical_extensions: Vec<u32>,
    non_critical_extensions: Vec<u32>,
    channels: Vec<DeclaredChannelUsage>,
    symbols: Vec<DeclaredSymbolRef>,
    effects: Vec<DeclaredEffect>,
    dependencies: Vec<DeclaredDependencySelector>,
    data_migration_mode: Option<DataMigrationMode>,
    migration_steps: Vec<String>,
}

/// Loads and validates either a package source directory, a manifest file, or a
/// built `nlos/package-file/v1` archive from `path`.
///
/// # Errors
///
/// Fails closed with [`ManifestLoadError`] on unreadable paths, malformed manifest
/// directives, corrupt `.nlospkg` framing, or `PKG-CONF-*` conformance failures.
pub fn inspect_package_target(path: &Path) -> Result<InspectedPackageTarget, ManifestLoadError> {
    let metadata = fs::metadata(path).map_err(|err| ManifestLoadError::Io {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })?;
    if metadata.is_dir() {
        return load_from_source_directory(path);
    }
    let raw_bytes = fs::read(path).map_err(|err| ManifestLoadError::Io {
        path: path.to_path_buf(),
        detail: err.to_string(),
    })?;
    let ext = path.extension().and_then(|os| os.to_str()).unwrap_or("");
    if raw_bytes.starts_with(PACKAGE_FILE_MAGIC) || matches!(ext, "nlospkg" | "nlos-pkg") {
        return load_from_package_bytes(path, &raw_bytes);
    }
    let text =
        std::str::from_utf8(&raw_bytes).map_err(|err| ManifestLoadError::CorruptPackageFile {
            path: path.to_path_buf(),
            detail: format!("file is neither a valid .nlospkg nor UTF-8 manifest: {err}"),
        })?;
    let mut overlay = PartialManifestOverlay::default();
    parse_manifest_lines(text, path, &mut overlay)?;
    finalize_overlay(TargetSourceKind::ManifestFile, path, overlay)
}

fn load_from_source_directory(dir: &Path) -> Result<InspectedPackageTarget, ManifestLoadError> {
    let pkg_manifest_path = dir.join("package.manifest");
    if !pkg_manifest_path.is_file() {
        return Err(ManifestLoadError::MissingManifest {
            path: dir.to_path_buf(),
        });
    }
    let pkg_text = fs::read_to_string(&pkg_manifest_path).map_err(|err| ManifestLoadError::Io {
        path: pkg_manifest_path.clone(),
        detail: err.to_string(),
    })?;
    let mut overlay = PartialManifestOverlay::default();
    parse_manifest_lines(&pkg_text, &pkg_manifest_path, &mut overlay)?;

    let mig_manifest_path = dir.join("migration.manifest");
    if mig_manifest_path.is_file() {
        let mig_text =
            fs::read_to_string(&mig_manifest_path).map_err(|err| ManifestLoadError::Io {
                path: mig_manifest_path.clone(),
                detail: err.to_string(),
            })?;
        parse_manifest_lines(&mig_text, &mig_manifest_path, &mut overlay)?;
    }
    finalize_overlay(
        TargetSourceKind::SourceDirectory,
        &pkg_manifest_path,
        overlay,
    )
}

fn load_from_package_bytes(
    path: &Path,
    bytes: &[u8],
) -> Result<InspectedPackageTarget, ManifestLoadError> {
    let decoded =
        decode_package_file(bytes).map_err(|err| ManifestLoadError::CorruptPackageFile {
            path: path.to_path_buf(),
            detail: err.to_string(),
        })?;
    let conformance = check_package_file(bytes);
    if !conformance.is_conformant() {
        let findings = conformance
            .findings
            .iter()
            .map(|item| format!("{}: {}", item.rule.id(), item.detail))
            .collect();
        return Err(ManifestLoadError::PackageConformanceFailed {
            path: path.to_path_buf(),
            findings,
        });
    }

    let mut overlay = PartialManifestOverlay {
        package_id: Some(decoded.package_id),
        version: Some(unpack_version(decoded.version)),
        entry_names: decoded.entries.iter().map(|e| e.name.clone()).collect(),
        ..PartialManifestOverlay::default()
    };

    if let Some(mig_entry) = decoded
        .entries
        .iter()
        .find(|entry| entry.name == "migration.manifest")
    {
        let mig_text = std::str::from_utf8(&mig_entry.payload).map_err(|err| {
            ManifestLoadError::CorruptPackageFile {
                path: path.to_path_buf(),
                detail: format!("embedded migration.manifest is not UTF-8: {err}"),
            }
        })?;
        parse_manifest_lines(mig_text, path, &mut overlay)?;
    }

    finalize_overlay(TargetSourceKind::BuiltPackageFile, path, overlay)
}

fn finalize_overlay(
    source_kind: TargetSourceKind,
    path: &Path,
    overlay: PartialManifestOverlay,
) -> Result<InspectedPackageTarget, ManifestLoadError> {
    let package_id = overlay
        .package_id
        .ok_or_else(|| ManifestLoadError::SyntaxError {
            path: path.to_path_buf(),
            line: 1,
            message: "missing required `package-id = <32-hex>`".to_owned(),
        })?;
    let version = overlay
        .version
        .ok_or_else(|| ManifestLoadError::SyntaxError {
            path: path.to_path_buf(),
            line: 1,
            message: "missing required `version = <major.minor.patch>`".to_owned(),
        })?;

    let target_sabi_epoch = overlay.target_sabi_epoch.unwrap_or(1);
    let target_sabi_minor = overlay.target_sabi_minor.unwrap_or(0);
    let mut channels = overlay.channels;
    if channels.is_empty() {
        channels.push(DeclaredChannelUsage {
            channel: "nlos.sabi.Envelope".to_owned(),
            major: target_sabi_epoch,
            minor: target_sabi_minor,
            critical_extensions: overlay.critical_extensions.clone(),
            non_critical_extensions: overlay.non_critical_extensions.clone(),
        });
    }

    Ok(InspectedPackageTarget {
        source_kind,
        package_id,
        version,
        entry_names: overlay.entry_names,
        target_sabi_epoch,
        target_sabi_minor,
        driver_abi_epoch: overlay.driver_abi_epoch,
        critical_extensions: overlay.critical_extensions,
        non_critical_extensions: overlay.non_critical_extensions,
        channels,
        symbols: overlay.symbols,
        effects: overlay.effects,
        dependencies: overlay.dependencies,
        data_migration_mode: overlay.data_migration_mode,
        migration_steps: overlay.migration_steps,
    })
}

fn parse_manifest_lines(
    source: &str,
    path: &Path,
    overlay: &mut PartialManifestOverlay,
) -> Result<(), ManifestLoadError> {
    if source.contains('\0') {
        return Err(ManifestLoadError::SyntaxError {
            path: path.to_path_buf(),
            line: 1,
            message: "NUL byte is forbidden in manifest".to_owned(),
        });
    }
    for (idx, raw_line) in source.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_val)) = trimmed.split_once('=') else {
            return Err(ManifestLoadError::SyntaxError {
                path: path.to_path_buf(),
                line: line_no,
                message: "expected `key = value` directive".to_owned(),
            });
        };
        apply_manifest_directive(path, line_no, raw_key.trim(), raw_val.trim(), overlay)?;
    }
    Ok(())
}

fn apply_manifest_directive(
    path: &Path,
    line_no: usize,
    key: &str,
    val: &str,
    overlay: &mut PartialManifestOverlay,
) -> Result<(), ManifestLoadError> {
    let syntax_err = |message: String| ManifestLoadError::SyntaxError {
        path: path.to_path_buf(),
        line: line_no,
        message,
    };

    match key {
        "package-id" => {
            let bytes = hex_decode_fixed::<16>(val, "package-id").map_err(syntax_err)?;
            overlay.package_id = Some(PackageId::from_bytes(bytes));
        }
        "version" => {
            overlay.version = Some(parse_semver_triple(val).map_err(syntax_err)?);
        }
        "entry" => {
            let entry_name = parse_entry_name(val).map_err(syntax_err)?;
            overlay.entry_names.push(entry_name);
        }
        "task" | "surface" => {}
        "target-sabi-epoch" => {
            overlay.target_sabi_epoch = Some(parse_u32_field(val, key).map_err(syntax_err)?);
        }
        "target-sabi-minor" => {
            overlay.target_sabi_minor = Some(parse_u32_field(val, key).map_err(syntax_err)?);
        }
        "driver-abi-epoch" => {
            overlay.driver_abi_epoch = Some(parse_u32_field(val, key).map_err(syntax_err)?);
        }
        "critical-extensions" => {
            overlay.critical_extensions = parse_u32_csv(val, key).map_err(syntax_err)?;
        }
        "non-critical-extensions" => {
            overlay.non_critical_extensions = parse_u32_csv(val, key).map_err(syntax_err)?;
        }
        "channel" => {
            overlay
                .channels
                .push(parse_channel_directive(val, overlay).map_err(syntax_err)?);
        }
        "symbol" => {
            overlay
                .symbols
                .push(parse_symbol_directive(val).map_err(syntax_err)?);
        }
        "effect" => {
            overlay
                .effects
                .push(parse_effect_directive(val).map_err(syntax_err)?);
        }
        "dependency" => {
            overlay
                .dependencies
                .push(parse_dependency_directive(val).map_err(syntax_err)?);
        }
        "data-migration-mode" => {
            overlay.data_migration_mode = Some(DataMigrationMode::parse(val).map_err(syntax_err)?);
        }
        "migration-step" => {
            if val.is_empty() {
                return Err(syntax_err("migration-step must not be empty".to_owned()));
            }
            overlay.migration_steps.push(val.to_owned());
        }
        other => {
            return Err(syntax_err(format!("unknown manifest directive {other:?}")));
        }
    }
    Ok(())
}

fn parse_semver_triple(raw: &str) -> Result<(u32, u32, u32), String> {
    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() != 3 {
        return Err(format!(
            "version {raw:?} must be formatted as <major.minor.patch>"
        ));
    }
    let major = parse_u32_field(parts[0], "version.major")?;
    let minor = parse_u32_field(parts[1], "version.minor")?;
    let patch = parse_u32_field(parts[2], "version.patch")?;
    if major == 0 && minor == 0 && patch == 0 {
        return Err("version 0.0.0 is forbidden".to_owned());
    }
    if minor > u32::from(u16::MAX) || patch > u32::from(u16::MAX) {
        return Err("version minor and patch must fit in u16".to_owned());
    }
    Ok((major, minor, patch))
}

const fn unpack_version(packed: u64) -> (u32, u32, u32) {
    #[allow(clippy::cast_possible_truncation)]
    let major = (packed >> 32) as u32;
    #[allow(clippy::cast_possible_truncation)]
    let minor = ((packed >> 16) & 0xFFFF) as u32;
    #[allow(clippy::cast_possible_truncation)]
    let patch = (packed & 0xFFFF) as u32;
    (major, minor, patch)
}

fn parse_entry_name(raw: &str) -> Result<String, String> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.len() != 3 {
        return Err("entry directive requires `<name> <role> <path>`".to_owned());
    }
    if !matches!(tokens[1], "executable" | "background-service" | "data") {
        return Err(format!("unknown entry role {:?}", tokens[1]));
    }
    Ok(tokens[0].to_owned())
}

fn parse_u32_field(raw: &str, name: &str) -> Result<u32, String> {
    raw.trim()
        .parse::<u32>()
        .map_err(|err| format!("invalid u32 for {name} ({raw:?}): {err}"))
}

fn parse_u32_csv(raw: &str, name: &str) -> Result<Vec<u32>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    trimmed
        .split(',')
        .map(|part| parse_u32_field(part, name))
        .collect()
}

fn parse_channel_directive(
    raw: &str,
    overlay: &PartialManifestOverlay,
) -> Result<DeclaredChannelUsage, String> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.is_empty() {
        return Err("channel directive requires at least `<channel_name>`".to_owned());
    }
    let channel = tokens[0].to_owned();
    let mut major = overlay.target_sabi_epoch.unwrap_or(1);
    let mut minor = overlay.target_sabi_minor.unwrap_or(0);
    let mut critical = overlay.critical_extensions.clone();
    let mut non_critical = overlay.non_critical_extensions.clone();

    for token in &tokens[1..] {
        if let Some(rest) = token.strip_prefix("critical=") {
            critical = parse_u32_csv(rest, "channel.critical")?;
        } else if let Some(rest) = token.strip_prefix("non-critical=") {
            non_critical = parse_u32_csv(rest, "channel.non-critical")?;
        } else if let Some((maj_s, min_s)) = token.split_once('.') {
            major = parse_u32_field(maj_s, "channel.major")?;
            minor = parse_u32_field(min_s, "channel.minor")?;
        } else {
            return Err(format!("unrecognized channel attribute {token:?}"));
        }
    }

    Ok(DeclaredChannelUsage {
        channel,
        major,
        minor,
        critical_extensions: critical,
        non_critical_extensions: non_critical,
    })
}

fn parse_symbol_directive(raw: &str) -> Result<DeclaredSymbolRef, String> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.len() < 2 {
        return Err("symbol directive requires `<channel> <symbol> [attrs...]`".to_owned());
    }
    let channel = tokens[0].to_owned();
    let symbol = tokens[1].to_owned();
    let mut parent_type = symbol.split_once('.').map(|(parent, _)| parent.to_owned());
    let mut field_number = None;

    for token in &tokens[2..] {
        if let Some(rest) = token.strip_prefix("parent=") {
            if rest.is_empty() {
                return Err("symbol parent= attribute must not be empty".to_owned());
            }
            parent_type = Some(rest.to_owned());
        } else if let Some(rest) = token.strip_prefix("field=") {
            field_number = Some(parse_u32_field(rest, "symbol.field")?);
        } else {
            return Err(format!("unrecognized symbol attribute {token:?}"));
        }
    }

    Ok(DeclaredSymbolRef {
        channel,
        symbol,
        parent_type,
        field_number,
    })
}

fn parse_effect_directive(raw: &str) -> Result<DeclaredEffect, String> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.len() < 2 {
        return Err(
            "effect directive requires `<effect_id> <CLASS> [compensation=...]`".to_owned(),
        );
    }
    let effect_id = tokens[0].to_owned();
    let effect_class = EffectClassDeclaration::parse(tokens[1])?;
    let mut compensation_handler = None;
    for token in &tokens[2..] {
        if let Some(rest) = token.strip_prefix("compensation=") {
            if !rest.is_empty() && !rest.eq_ignore_ascii_case("none") {
                compensation_handler = Some(rest.to_owned());
            }
        } else {
            return Err(format!("unrecognized effect attribute {token:?}"));
        }
    }
    Ok(DeclaredEffect {
        effect_id,
        effect_class,
        compensation_handler,
    })
}

fn parse_dependency_directive(raw: &str) -> Result<DeclaredDependencySelector, String> {
    let tokens: Vec<&str> = raw.split_whitespace().collect();
    if tokens.len() != 2 {
        return Err("dependency directive requires `<entity_kind> <selector>`".to_owned());
    }
    Ok(DeclaredDependencySelector {
        entity_kind: tokens[0].to_owned(),
        raw_selector: tokens[1].to_owned(),
    })
}
