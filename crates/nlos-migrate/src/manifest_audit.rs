//! Line-based `package.manifest` parser supporting legacy `nlos-package`
//! directives and Stage E declarative compatibility, multi-component
//! (`[PKG-MULTI-001]`), effect (`[DRV-LIFE-001]`), and migration
//! (`[PKG-UPDATE-001]`) extensions.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;

use nlos_application::pack_package_version;
use nlos_artifact::{MAX_ENTRY_NAME_BYTES, MAX_TASK_DEPENDENCIES_PER_TEMPLATE, PackageTaskKind};
use nlos_types::PackageId;

use crate::json::hex_decode_array;

/// Maximum manifest size accepted by the static auditor (`256 KiB`).
pub const MAX_PACKAGE_MANIFEST_BYTES: usize = 256 * 1024;

/// Target SABI epoch declared by `target-sabi-epoch = <u32>[.<u32>]`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SabiEpochTarget {
    pub major: u32,
    pub minor: u32,
}

impl Default for SabiEpochTarget {
    fn default() -> Self {
        Self { major: 1, minor: 0 }
    }
}

impl fmt::Display for SabiEpochTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}", self.major, self.minor)
    }
}

/// Multi-component package role (`[PKG-MULTI-001]`, `[PKG-SEED-001]`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ComponentRole {
    Executable,
    Driver,
    UiSurface,
    BackgroundService,
    SeedTemplate,
    Data,
}

impl ComponentRole {
    /// Parses a component role keyword from `entry` or `component`.
    #[must_use]
    pub fn parse_component(token: &str) -> Option<Self> {
        match token {
            "executable" => Some(Self::Executable),
            "driver" => Some(Self::Driver),
            "ui-surface" => Some(Self::UiSurface),
            "background-service" => Some(Self::BackgroundService),
            "seed-template" => Some(Self::SeedTemplate),
            "data" => Some(Self::Data),
            _ => None,
        }
    }

    /// Parses a legacy `entry = <name> <role> <path>` role keyword.
    #[must_use]
    pub fn parse_legacy_entry(token: &str) -> Option<Self> {
        match token {
            "executable" => Some(Self::Executable),
            "background-service" => Some(Self::BackgroundService),
            "data" => Some(Self::Data),
            _ => None,
        }
    }

    /// Canonical manifest token for this role.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Executable => "executable",
            Self::Driver => "driver",
            Self::UiSurface => "ui-surface",
            Self::BackgroundService => "background-service",
            Self::SeedTemplate => "seed-template",
            Self::Data => "data",
        }
    }

    /// Whether this component carries read-only data or seed templates subject
    /// to `[PKG-SEED-001]` mutable-database exclusion.
    #[must_use]
    pub const fn is_seed_or_data(self) -> bool {
        matches!(self, Self::SeedTemplate | Self::Data)
    }
}

/// One declared entry or multi-component payload in `package.manifest`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentDeclaration {
    pub line: usize,
    pub name: String,
    pub role: ComponentRole,
    pub relative_path: String,
    pub is_legacy_entry: bool,
}

/// Declared `EffectDescriptor` classification (`[DRV-NEGO-001]`, `[DRV-LIFE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectClass {
    ReadOnly,
    Reversible,
    Compensatable,
    Irreversible,
}

impl EffectClass {
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "READ_ONLY" => Some(Self::ReadOnly),
            "REVERSIBLE" => Some(Self::Reversible),
            "COMPENSATABLE" => Some(Self::Compensatable),
            "IRREVERSIBLE" => Some(Self::Irreversible),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "READ_ONLY",
            Self::Reversible => "REVERSIBLE",
            Self::Compensatable => "COMPENSATABLE",
            Self::Irreversible => "IRREVERSIBLE",
        }
    }
}

/// Declared `UNCERTAIN` crash-reconciliation directive (`[DRV-LIFE-001]`, `[KABI-UNCERTAIN-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReconcileDirective {
    /// Replays/queries exclusively under the original 128-bit `IdempotencyKey`.
    SameIdempotencyKey,
    /// Explicitly declares `reconcile=none`.
    None,
    /// Omitted `reconcile=...` clause on the `effect` line.
    Omitted,
    /// Invalid or forbidden directive (such as rotating to a fresh idempotency key).
    ForbiddenPolicy(String),
}

impl ReconcileDirective {
    #[must_use]
    pub fn parse_clause(clause: &str) -> Option<Self> {
        let value = clause.strip_prefix("reconcile=")?;
        if value.is_empty() {
            return None;
        }
        match value {
            "same-idempotency-key" => Some(Self::SameIdempotencyKey),
            "none" => Some(Self::None),
            other => Some(Self::ForbiddenPolicy(other.to_owned())),
        }
    }
}

/// One `effect = <component_or_op> <CLASS> [reconcile=...]` declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectDeclaration {
    pub line: usize,
    pub target: String,
    pub effect_class: EffectClass,
    pub reconcile: ReconcileDirective,
}

impl EffectDeclaration {
    /// Checks whether this `EffectDeclaration` covers the named component
    /// (either matching `<name>` directly or `<name>::<op>` / `<name>.<op>` / `<name>/<op>`).
    #[must_use]
    pub fn matches_component(&self, component_name: &str) -> bool {
        if self.target == component_name {
            return true;
        }
        for sep in ["::", ".", "/"] {
            if let Some(prefix) = self.target.strip_prefix(component_name)
                && prefix.starts_with(sep)
                && prefix.len() > sep.len()
            {
                return true;
            }
        }
        false
    }
}

/// One `sabi-use = <channel> <symbol> [field=<u32>]` contract reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SabiSymbolUse {
    pub line: usize,
    pub channel: String,
    pub symbol: String,
    pub field_number: Option<u32>,
}

/// Criticality class of a declared SABI extension (`[COMPAT-VER-002]`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ExtensionCriticality {
    Critical,
    NonCritical,
}

impl ExtensionCriticality {
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "critical" => Some(Self::Critical),
            "noncritical" | "non-critical" => Some(Self::NonCritical),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::NonCritical => "noncritical",
        }
    }
}

/// One `sabi-ext = <channel> <critical|noncritical> <id>` extension declaration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SabiExtensionUse {
    pub line: usize,
    pub channel: String,
    pub criticality: ExtensionCriticality,
    pub extension_id: u32,
}

/// Statutory `[PKG-UPDATE-001]` data container migration strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeclaredMigrationMode {
    BackwardCompatibleWindow,
    SnapshotAssistedRollback,
    Irreversible,
}

impl DeclaredMigrationMode {
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "BACKWARD_COMPATIBLE_WINDOW" => Some(Self::BackwardCompatibleWindow),
            "SNAPSHOT_ASSISTED_ROLLBACK" => Some(Self::SnapshotAssistedRollback),
            "IRREVERSIBLE" => Some(Self::Irreversible),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BackwardCompatibleWindow => "BACKWARD_COMPATIBLE_WINDOW",
            Self::SnapshotAssistedRollback => "SNAPSHOT_ASSISTED_ROLLBACK",
            Self::Irreversible => "IRREVERSIBLE",
        }
    }
}

/// One `task = ...` template declaration from `package.manifest`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskDeclaration {
    pub line: usize,
    pub node_key: [u8; 16],
    pub kind: PackageTaskKind,
    pub binding_path: String,
    pub inputs_path: String,
    pub outputs_path: String,
    pub policy_path: String,
    pub ceiling_path: String,
    pub dependency_keys: Vec<[u8; 16]>,
}

/// Parsed declarative `package.manifest` AST.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditedPackageManifest {
    pub package_id: PackageId,
    pub version_raw: String,
    pub packed_version: u64,
    pub target_sabi_epoch: SabiEpochTarget,
    pub target_sabi_epoch_explicit: bool,
    pub driver_abi_epoch: Option<u32>,
    pub migration_mode: Option<DeclaredMigrationMode>,
    pub components: Vec<ComponentDeclaration>,
    pub effects: Vec<EffectDeclaration>,
    pub sabi_uses: Vec<SabiSymbolUse>,
    pub sabi_extensions: Vec<SabiExtensionUse>,
    pub tasks: Vec<TaskDeclaration>,
}

/// Fail-closed manifest syntax or structural error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestParseError {
    IoError { path: String, detail: String },
    TooLarge { actual: usize, maximum: usize },
    SyntaxError { line: usize, detail: String },
    DuplicateScalar { line: usize, key: String },
    MissingRequiredField(&'static str),
    DuplicateComponentName { line: usize, name: String },
    EmptyComponents,
    InvalidTaskGraph { detail: String },
}

impl fmt::Display for ManifestParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::IoError { path, detail } => {
                write!(formatter, "failed to read {path}: {detail}")
            }
            Self::TooLarge { actual, maximum } => {
                write!(
                    formatter,
                    "package.manifest size {actual} exceeds maximum {maximum} bytes"
                )
            }
            Self::SyntaxError { line, detail } => {
                write!(formatter, "package.manifest line {line}: {detail}")
            }
            Self::DuplicateScalar { line, key } => {
                write!(
                    formatter,
                    "package.manifest line {line}: duplicate scalar directive {key:?}"
                )
            }
            Self::MissingRequiredField(field) => {
                write!(
                    formatter,
                    "package.manifest is missing required directive {field:?}"
                )
            }
            Self::DuplicateComponentName { line, name } => {
                write!(
                    formatter,
                    "package.manifest line {line}: duplicate entry/component name {name:?}"
                )
            }
            Self::EmptyComponents => {
                formatter.write_str("package.manifest must declare at least one entry or component")
            }
            Self::InvalidTaskGraph { detail } => {
                write!(formatter, "package.manifest task template error: {detail}")
            }
        }
    }
}

impl Error for ManifestParseError {}

/// Formats a packed `u64` package version back into `<major>.<minor>.<patch>`.
#[must_use]
pub fn format_packed_version(packed: u64) -> String {
    let major = packed >> 32;
    let minor = (packed >> 16) & 0xFFFF;
    let patch = packed & 0xFFFF;
    format!("{major}.{minor}.{patch}")
}

/// Reads and parses `<package_dir>/package.manifest`.
///
/// # Errors
///
/// Returns [`ManifestParseError`] if the file cannot be read or violates line
/// syntax, scalar uniqueness, component naming, or task-template locality.
pub fn load_package_manifest(
    package_dir: &Path,
) -> Result<AuditedPackageManifest, ManifestParseError> {
    let manifest_path = package_dir.join("package.manifest");
    let metadata = fs::metadata(&manifest_path).map_err(|err| ManifestParseError::IoError {
        path: manifest_path.display().to_string(),
        detail: err.to_string(),
    })?;
    if metadata.len() > u64::try_from(MAX_PACKAGE_MANIFEST_BYTES).unwrap_or(u64::MAX) {
        return Err(ManifestParseError::TooLarge {
            actual: usize::try_from(metadata.len()).unwrap_or(usize::MAX),
            maximum: MAX_PACKAGE_MANIFEST_BYTES,
        });
    }
    let text = fs::read_to_string(&manifest_path).map_err(|err| ManifestParseError::IoError {
        path: manifest_path.display().to_string(),
        detail: err.to_string(),
    })?;
    parse_package_manifest(&text)
}

/// Parses a `package.manifest` document in memory.
///
/// # Errors
///
/// Fails closed with [`ManifestParseError`] on malformed directives, duplicate
/// scalar keys, missing required fields (`package-id`, `version`, at least one
/// `entry` or `component`), duplicate component names, or invalid task edges.
#[allow(clippy::too_many_lines)]
pub fn parse_package_manifest(text: &str) -> Result<AuditedPackageManifest, ManifestParseError> {
    if text.len() > MAX_PACKAGE_MANIFEST_BYTES {
        return Err(ManifestParseError::TooLarge {
            actual: text.len(),
            maximum: MAX_PACKAGE_MANIFEST_BYTES,
        });
    }
    if text.contains('\0') {
        return Err(ManifestParseError::SyntaxError {
            line: 1,
            detail: "manifest contains NUL byte".to_owned(),
        });
    }

    let mut package_id: Option<PackageId> = None;
    let mut version_info: Option<(String, u64)> = None;
    let mut target_sabi_epoch: Option<SabiEpochTarget> = None;
    let mut driver_abi_epoch: Option<u32> = None;
    let mut migration_mode: Option<DeclaredMigrationMode> = None;

    let mut components = Vec::new();
    let mut effects = Vec::new();
    let mut sabi_uses = Vec::new();
    let mut sabi_extensions = Vec::new();
    let mut tasks = Vec::new();
    let mut seen_scalars = HashSet::new();

    for (index, raw_line) in text.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_val)) = trimmed.split_once('=') else {
            return Err(ManifestParseError::SyntaxError {
                line: line_no,
                detail: "expected `key = value`".to_owned(),
            });
        };
        let key = raw_key.trim();
        let value = raw_val.trim();
        if key.is_empty() {
            return Err(ManifestParseError::SyntaxError {
                line: line_no,
                detail: "empty directive key".to_owned(),
            });
        }

        let is_repeatable = matches!(
            key,
            "entry" | "component" | "effect" | "sabi-use" | "sabi-ext" | "task"
        );
        if !is_repeatable && !seen_scalars.insert(key.to_owned()) {
            return Err(ManifestParseError::DuplicateScalar {
                line: line_no,
                key: key.to_owned(),
            });
        }

        match key {
            "package-id" => {
                let bytes = hex_decode_array::<16>(value).ok_or_else(|| {
                    ManifestParseError::SyntaxError {
                        line: line_no,
                        detail: "package-id must be exactly 32 lowercase/uppercase hex chars"
                            .to_owned(),
                    }
                })?;
                package_id = Some(PackageId::from_bytes(bytes));
            }
            "version" => {
                let packed = parse_manifest_version(value).ok_or_else(|| {
                    ManifestParseError::SyntaxError {
                        line: line_no,
                        detail: "version must be `<u64>` or `<major>.<minor>.<patch>`".to_owned(),
                    }
                })?;
                version_info = Some((value.to_owned(), packed));
            }
            "target-sabi-epoch" => {
                target_sabi_epoch = Some(parse_sabi_epoch_target(value, line_no)?);
            }
            "driver-abi-epoch" => {
                let parsed = value
                    .parse::<u32>()
                    .map_err(|_| ManifestParseError::SyntaxError {
                        line: line_no,
                        detail: format!("invalid driver-abi-epoch {value:?}; expected u32"),
                    })?;
                driver_abi_epoch = Some(parsed);
            }
            "migration-mode" => {
                let mode = DeclaredMigrationMode::parse(value).ok_or_else(|| {
                    ManifestParseError::SyntaxError {
                        line: line_no,
                        detail: format!(
                            "invalid migration-mode {value:?}; expected BACKWARD_COMPATIBLE_WINDOW, SNAPSHOT_ASSISTED_ROLLBACK, or IRREVERSIBLE"
                        ),
                    }
                })?;
                migration_mode = Some(mode);
            }
            "entry" => {
                components.push(parse_component_line(value, line_no, true)?);
            }
            "component" => {
                components.push(parse_component_line(value, line_no, false)?);
            }
            "effect" => {
                effects.push(parse_effect_line(value, line_no)?);
            }
            "sabi-use" => {
                sabi_uses.push(parse_sabi_use_line(value, line_no)?);
            }
            "sabi-ext" => {
                sabi_extensions.push(parse_sabi_ext_line(value, line_no)?);
            }
            "task" => {
                tasks.push(parse_task_line(value, line_no)?);
            }
            unknown => {
                return Err(ManifestParseError::SyntaxError {
                    line: line_no,
                    detail: format!("unknown manifest directive {unknown:?}"),
                });
            }
        }
    }

    let package_id = package_id.ok_or(ManifestParseError::MissingRequiredField("package-id"))?;
    let (version_raw, packed_version) =
        version_info.ok_or(ManifestParseError::MissingRequiredField("version"))?;
    if components.is_empty() {
        return Err(ManifestParseError::EmptyComponents);
    }

    let mut seen_names = HashSet::with_capacity(components.len());
    for comp in &components {
        if comp.name.is_empty() || comp.name.len() > MAX_ENTRY_NAME_BYTES {
            return Err(ManifestParseError::SyntaxError {
                line: comp.line,
                detail: format!(
                    "component name {:?} must be 1..={MAX_ENTRY_NAME_BYTES} bytes",
                    comp.name
                ),
            });
        }
        if !seen_names.insert(comp.name.as_str()) {
            return Err(ManifestParseError::DuplicateComponentName {
                line: comp.line,
                name: comp.name.clone(),
            });
        }
    }

    validate_task_declarations(&tasks)?;

    Ok(AuditedPackageManifest {
        package_id,
        version_raw,
        packed_version,
        target_sabi_epoch: target_sabi_epoch.unwrap_or_default(),
        target_sabi_epoch_explicit: target_sabi_epoch.is_some(),
        driver_abi_epoch,
        migration_mode,
        components,
        effects,
        sabi_uses,
        sabi_extensions,
        tasks,
    })
}

fn parse_manifest_version(value: &str) -> Option<u64> {
    if value.contains('.') {
        let mut parts = value.split('.');
        let major: u32 = parts.next()?.parse().ok()?;
        let minor: u32 = parts.next()?.parse().ok()?;
        let patch: u32 = parts.next()?.parse().ok()?;
        if parts.next().is_some() || minor > u32::from(u16::MAX) || patch > u32::from(u16::MAX) {
            return None;
        }
        Some(pack_package_version(major, minor, patch))
    } else {
        value.parse::<u64>().ok()
    }
}

fn parse_sabi_epoch_target(
    value: &str,
    line: usize,
) -> Result<SabiEpochTarget, ManifestParseError> {
    if let Some((maj_str, min_str)) = value.split_once('.') {
        if min_str.contains('.') {
            return Err(ManifestParseError::SyntaxError {
                line,
                detail: format!(
                    "invalid target-sabi-epoch {value:?}; expected `<u32>` or `<u32>.<u32>`"
                ),
            });
        }
        let major = maj_str
            .parse::<u32>()
            .map_err(|_| ManifestParseError::SyntaxError {
                line,
                detail: format!("invalid target-sabi-epoch major in {value:?}"),
            })?;
        let minor = min_str
            .parse::<u32>()
            .map_err(|_| ManifestParseError::SyntaxError {
                line,
                detail: format!("invalid target-sabi-epoch minor in {value:?}"),
            })?;
        Ok(SabiEpochTarget { major, minor })
    } else {
        let major = value
            .parse::<u32>()
            .map_err(|_| ManifestParseError::SyntaxError {
                line,
                detail: format!(
                    "invalid target-sabi-epoch {value:?}; expected `<u32>` or `<u32>.<u32>`"
                ),
            })?;
        Ok(SabiEpochTarget { major, minor: 0 })
    }
}

fn parse_component_line(
    value: &str,
    line: usize,
    is_legacy_entry: bool,
) -> Result<ComponentDeclaration, ManifestParseError> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 3 {
        let directive = if is_legacy_entry {
            "entry"
        } else {
            "component"
        };
        return Err(ManifestParseError::SyntaxError {
            line,
            detail: format!("expected `{directive} = <name> <kind> <path>`"),
        });
    }
    let role_opt = if is_legacy_entry {
        ComponentRole::parse_legacy_entry(tokens[1])
    } else {
        ComponentRole::parse_component(tokens[1])
    };
    let role = role_opt.ok_or_else(|| ManifestParseError::SyntaxError {
        line,
        detail: format!("unsupported component role {:?}", tokens[1]),
    })?;
    Ok(ComponentDeclaration {
        line,
        name: tokens[0].to_owned(),
        role,
        relative_path: tokens[2].to_owned(),
        is_legacy_entry,
    })
}

fn parse_effect_line(value: &str, line: usize) -> Result<EffectDeclaration, ManifestParseError> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 2 && tokens.len() != 3 {
        return Err(ManifestParseError::SyntaxError {
            line,
            detail: "expected `effect = <component_or_op> <READ_ONLY|REVERSIBLE|COMPENSATABLE|IRREVERSIBLE> [reconcile=<same-idempotency-key|none>]`".to_owned(),
        });
    }
    let effect_class =
        EffectClass::parse(tokens[1]).ok_or_else(|| ManifestParseError::SyntaxError {
            line,
            detail: format!("unsupported effect class {:?}", tokens[1]),
        })?;
    let reconcile = if tokens.len() == 3 {
        ReconcileDirective::parse_clause(tokens[2]).ok_or_else(|| {
            ManifestParseError::SyntaxError {
                line,
                detail: format!(
                    "expected `reconcile=<same-idempotency-key|none>`, got {:?}",
                    tokens[2]
                ),
            }
        })?
    } else {
        ReconcileDirective::Omitted
    };
    Ok(EffectDeclaration {
        line,
        target: tokens[0].to_owned(),
        effect_class,
        reconcile,
    })
}

fn parse_sabi_use_line(value: &str, line: usize) -> Result<SabiSymbolUse, ManifestParseError> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 2 && tokens.len() != 3 {
        return Err(ManifestParseError::SyntaxError {
            line,
            detail: "expected `sabi-use = <channel> <symbol> [field=<u32>]`".to_owned(),
        });
    }
    let field_number = if tokens.len() == 3 {
        let raw_num =
            tokens[2]
                .strip_prefix("field=")
                .ok_or_else(|| ManifestParseError::SyntaxError {
                    line,
                    detail: format!("expected `field=<u32>`, got {:?}", tokens[2]),
                })?;
        let parsed = raw_num
            .parse::<u32>()
            .map_err(|_| ManifestParseError::SyntaxError {
                line,
                detail: format!("invalid field number {raw_num:?}; expected u32"),
            })?;
        Some(parsed)
    } else {
        None
    };
    Ok(SabiSymbolUse {
        line,
        channel: tokens[0].to_owned(),
        symbol: tokens[1].to_owned(),
        field_number,
    })
}

fn parse_sabi_ext_line(value: &str, line: usize) -> Result<SabiExtensionUse, ManifestParseError> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 3 {
        return Err(ManifestParseError::SyntaxError {
            line,
            detail: "expected `sabi-ext = <channel> <critical|noncritical> <id>`".to_owned(),
        });
    }
    let criticality =
        ExtensionCriticality::parse(tokens[1]).ok_or_else(|| ManifestParseError::SyntaxError {
            line,
            detail: format!("expected `critical` or `noncritical`, got {:?}", tokens[1]),
        })?;
    let extension_id = tokens[2]
        .parse::<u32>()
        .map_err(|_| ManifestParseError::SyntaxError {
            line,
            detail: format!("invalid extension id {:?}; expected u32", tokens[2]),
        })?;
    Ok(SabiExtensionUse {
        line,
        channel: tokens[0].to_owned(),
        criticality,
        extension_id,
    })
}

fn parse_task_line(value: &str, line: usize) -> Result<TaskDeclaration, ManifestParseError> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 7 && tokens.len() != 8 {
        return Err(ManifestParseError::SyntaxError {
            line,
            detail: "expected `task = <node-key-HEX32> <agent-role|executable> <binding> <inputs> <outputs> <policy> <ceiling> [deps=<HEX32,...>]`".to_owned(),
        });
    }
    let node_key =
        hex_decode_array::<16>(tokens[0]).ok_or_else(|| ManifestParseError::SyntaxError {
            line,
            detail: "task node key must be 32 hex chars".to_owned(),
        })?;
    let kind = match tokens[1] {
        "agent-role" => PackageTaskKind::AgentRole,
        "executable" => PackageTaskKind::Executable,
        other => {
            return Err(ManifestParseError::SyntaxError {
                line,
                detail: format!("unknown task kind {other:?}"),
            });
        }
    };
    let mut dependency_keys = Vec::new();
    if tokens.len() == 8 {
        let dep_list =
            tokens[7]
                .strip_prefix("deps=")
                .ok_or_else(|| ManifestParseError::SyntaxError {
                    line,
                    detail: "eighth task token must be `deps=<HEX32,...>`".to_owned(),
                })?;
        for dep in dep_list.split(',') {
            let dep_key =
                hex_decode_array::<16>(dep).ok_or_else(|| ManifestParseError::SyntaxError {
                    line,
                    detail: format!("task dependency {dep:?} must be 32 hex chars"),
                })?;
            dependency_keys.push(dep_key);
        }
    }
    Ok(TaskDeclaration {
        line,
        node_key,
        kind,
        binding_path: tokens[2].to_owned(),
        inputs_path: tokens[3].to_owned(),
        outputs_path: tokens[4].to_owned(),
        policy_path: tokens[5].to_owned(),
        ceiling_path: tokens[6].to_owned(),
        dependency_keys,
    })
}

fn validate_task_declarations(tasks: &[TaskDeclaration]) -> Result<(), ManifestParseError> {
    if tasks.is_empty() {
        return Ok(());
    }
    let mut seen = HashSet::with_capacity(tasks.len());
    for task in tasks {
        if !seen.insert(task.node_key) {
            return Err(ManifestParseError::InvalidTaskGraph {
                detail: format!("duplicate task template node key on line {}", task.line),
            });
        }
        if task.dependency_keys.len() > MAX_TASK_DEPENDENCIES_PER_TEMPLATE {
            return Err(ManifestParseError::InvalidTaskGraph {
                detail: format!(
                    "task on line {} exceeds MAX_TASK_DEPENDENCIES_PER_TEMPLATE",
                    task.line
                ),
            });
        }
        if task.dependency_keys.contains(&task.node_key) {
            return Err(ManifestParseError::InvalidTaskGraph {
                detail: format!("task on line {} depends on itself", task.line),
            });
        }
    }
    for task in tasks {
        for dep in &task.dependency_keys {
            if !seen.contains(dep) {
                return Err(ManifestParseError::InvalidTaskGraph {
                    detail: format!("task on line {} depends on undeclared node key", task.line),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DeclaredMigrationMode, ManifestParseError, SabiEpochTarget, format_packed_version,
        parse_package_manifest,
    };

    #[test]
    fn parses_scalar_directives_and_packed_versions() {
        let parsed = parse_package_manifest(
            "package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
             version = 2.3.4\n\
             target-sabi-epoch = 2\n\
             driver-abi-epoch = 2\n\
             migration-mode = BACKWARD_COMPATIBLE_WINDOW\n\
             component = core executable core.bin\n",
        )
        .expect("parse valid manifest");

        assert_eq!(format_packed_version(parsed.packed_version), "2.3.4");
        assert_eq!(
            parsed.target_sabi_epoch,
            SabiEpochTarget { major: 2, minor: 0 }
        );
        assert!(parsed.target_sabi_epoch_explicit);
        assert_eq!(parsed.driver_abi_epoch, Some(2));
        assert_eq!(
            parsed.migration_mode,
            Some(DeclaredMigrationMode::BackwardCompatibleWindow)
        );
    }

    #[test]
    fn rejects_duplicate_scalars_duplicate_components_and_missing_fields() {
        let dup_scalar = parse_package_manifest(
            "package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
             version = 1.0.0\n\
             version = 1.1.0\n\
             entry = app executable app.bin\n",
        );
        assert!(matches!(
            dup_scalar,
            Err(ManifestParseError::DuplicateScalar { line: 3, .. })
        ));

        let dup_comp = parse_package_manifest(
            "package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
             version = 1.0.0\n\
             entry = app executable app.bin\n\
             component = app ui-surface ui.bundle\n",
        );
        assert!(matches!(
            dup_comp,
            Err(ManifestParseError::DuplicateComponentName { line: 4, .. })
        ));

        let no_comp = parse_package_manifest(
            "package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
             version = 1.0.0\n",
        );
        assert_eq!(no_comp, Err(ManifestParseError::EmptyComponents));
    }

    #[test]
    fn rejects_self_referential_or_dangling_task_dependencies() {
        let dangling = parse_package_manifest(
            "package-id = 4e1077c1a5b24d8e9f0123456789abcd\n\
             version = 1.0.0\n\
             entry = app executable app.bin\n\
             task = 10a1c2d3e4f5061728394a5b6c7d8e01 executable b.txt i.txt o.txt p.txt c.txt deps=ffffffffffffffffffffffffffffffff\n",
        );
        assert!(matches!(
            dangling,
            Err(ManifestParseError::InvalidTaskGraph { .. })
        ));
    }
}
