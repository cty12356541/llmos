//! Machine-readable SABI deprecation manifest parser and `ADR-0027` lifecycle
//! state-machine validator (`ACTIVE -> DEPRECATED -> SUNSET` + permanent `RESERVED`).
//!
//! Implemented in 100% safe Rust (`#![forbid(unsafe_code)]`) with zero external
//! YAML crate dependencies.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use crate::version_matrix::MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS;
use crate::{SchemaDescriptor, schema_registry};

/// Maximum size in bytes accepted for a deprecation manifest YAML document.
pub const MAX_DEPRECATION_MANIFEST_BYTES: usize = 512 * 1024;

/// Embedded canonical SABI v1 deprecation manifest (`schema/nlos/sabi/v1/deprecation-manifest.yaml`).
pub const CANONICAL_DEPRECATION_MANIFEST_YAML: &str =
    include_str!("../../../schema/nlos/sabi/v1/deprecation-manifest.yaml");

/// Lifecycle state of a managed SABI contract element (`[COMPAT-DEPRECATE-001]`, `ADR-0027`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeprecationState {
    /// Actively supported contract element; zero deprecation warnings.
    Active,
    /// Within the deprecation grace window (`earliest_sunset_epoch >= deprecated_in_epoch + 2`);
    /// remains wire-compatible while emitting structured `DeprecationWarningReceipt` telemetry.
    Deprecated,
    /// Retired after completing its grace window; permanently `RESERVED` against slot/symbol reuse
    /// and rejected fail-closed (`E_ABI_VERSION`).
    Sunset,
}

impl DeprecationState {
    fn parse(raw: &str, symbol: &str) -> Result<Self, DeprecationManifestError> {
        match raw {
            "ACTIVE" => Ok(Self::Active),
            "DEPRECATED" => Ok(Self::Deprecated),
            "SUNSET" => Ok(Self::Sunset),
            other => Err(DeprecationManifestError::InvalidState {
                symbol: symbol.to_owned(),
                state: other.to_owned(),
            }),
        }
    }
}

/// Classification of a managed SABI schema element.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContractElementKind {
    Message,
    Field,
    Method,
    EnumValue,
    Extension,
}

impl ContractElementKind {
    fn parse(raw: &str, line: usize) -> Result<Self, DeprecationManifestError> {
        match raw {
            "MESSAGE" => Ok(Self::Message),
            "FIELD" => Ok(Self::Field),
            "METHOD" => Ok(Self::Method),
            "ENUM_VALUE" => Ok(Self::EnumValue),
            "EXTENSION" => Ok(Self::Extension),
            other => Err(DeprecationManifestError::SyntaxError {
                line,
                message: format!("unsupported entry kind {other:?}"),
            }),
        }
    }
}

/// Per-channel freeze and permanent tombstone metadata in `deprecation-manifest.yaml`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelDeprecationRecord {
    pub name: String,
    pub major: u32,
    pub minor: u32,
    pub frozen: bool,
    pub reserved_field_numbers: Vec<u32>,
    pub reserved_symbols: Vec<String>,
}

/// Managed SABI contract lifecycle entry in `deprecation-manifest.yaml`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeprecationEntry {
    pub channel: String,
    pub symbol: String,
    pub kind: ContractElementKind,
    pub parent_type: Option<String>,
    pub field_number: Option<u32>,
    pub introduced_in_epoch: u32,
    pub state: DeprecationState,
    pub deprecated_in_epoch: Option<u32>,
    pub earliest_sunset_epoch: Option<u32>,
    pub sunset_in_epoch: Option<u32>,
    pub replacement: Option<String>,
    pub migration_rule_id: Option<String>,
    pub reason: Option<String>,
}

/// Validated machine-readable SABI deprecation manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeprecationManifest {
    pub manifest_version: u32,
    pub current_epoch: u32,
    pub min_compatibility_window_epochs: u32,
    pub frozen_channels_total: usize,
    pub channels: Vec<ChannelDeprecationRecord>,
    pub entries: Vec<DeprecationEntry>,
}

/// Audit verdict produced when checking a negotiated symbol or field/extension slot
/// against [`DeprecationManifest`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymbolAuditVerdict<'a> {
    /// Active contract element (or untombstoned additive element); allow without warning.
    Active(Option<&'a DeprecationEntry>),
    /// Deprecated contract element inside its grace window; allow traffic and emit
    /// a `DeprecationWarningReceipt`.
    Deprecated(&'a DeprecationEntry),
    /// Sunset or permanently reserved tombstone; reject fail-closed (`E_ABI_VERSION`).
    SunsetOrReserved {
        entry: Option<&'a DeprecationEntry>,
        reason: &'static str,
    },
}

/// Fail-closed validation and parsing errors for `deprecation-manifest.yaml`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeprecationManifestError {
    ManifestTooLarge {
        actual: usize,
        maximum: usize,
    },
    SyntaxError {
        line: usize,
        message: String,
    },
    MissingTopLevelField(&'static str),
    UnsupportedManifestVersion(u32),
    InvalidEpoch {
        context: String,
        epoch: u32,
    },
    InvalidCompatibilityWindow {
        got: u32,
        minimum: u32,
    },
    ChannelCountMismatch {
        declared: usize,
        actual: usize,
    },
    UnfrozenChannel(String),
    DuplicateChannel(String),
    UnknownChannel {
        channel: String,
        symbol: String,
    },
    InvalidState {
        symbol: String,
        state: String,
    },
    InvalidFieldNumber {
        channel: String,
        symbol: String,
        field_number: u32,
    },
    InvalidStateMetadata {
        channel: String,
        symbol: String,
        detail: &'static str,
    },
    MissingDeprecationMetadata {
        channel: String,
        symbol: String,
        field: &'static str,
    },
    InsufficientDeprecationWindow {
        channel: String,
        symbol: String,
        deprecated_in_epoch: u32,
        earliest_sunset_epoch: u32,
        required_window_epochs: u32,
    },
    PrematureSunset {
        channel: String,
        symbol: String,
        current_epoch: u32,
        earliest_sunset_epoch: u32,
        sunset_in_epoch: u32,
    },
    ReservedSlotReuse {
        channel: String,
        symbol: String,
        conflict: String,
    },
    DuplicateEntry {
        channel: String,
        symbol: String,
    },
    RegistryMismatch {
        channel: String,
        detail: String,
    },
}

#[allow(clippy::too_many_lines)]
impl fmt::Display for DeprecationManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManifestTooLarge { actual, maximum } => write!(
                formatter,
                "deprecation manifest has {actual} bytes; maximum is {maximum}"
            ),
            Self::SyntaxError { line, message } => {
                write!(formatter, "manifest syntax error at line {line}: {message}")
            }
            Self::MissingTopLevelField(field) => {
                write!(formatter, "manifest is missing top-level field {field:?}")
            }
            Self::UnsupportedManifestVersion(version) => {
                write!(
                    formatter,
                    "unsupported manifest_version {version}; expected 1"
                )
            }
            Self::InvalidEpoch { context, epoch } => {
                write!(formatter, "invalid epoch {epoch} for {context}")
            }
            Self::InvalidCompatibilityWindow { got, minimum } => write!(
                formatter,
                "min_compatibility_window_epochs {got} is below minimum {minimum}"
            ),
            Self::ChannelCountMismatch { declared, actual } => write!(
                formatter,
                "frozen_channels_total is {declared}, but channels list has {actual} entries"
            ),
            Self::UnfrozenChannel(channel) => {
                write!(formatter, "channel {channel:?} must have frozen: true")
            }
            Self::DuplicateChannel(channel) => {
                write!(formatter, "duplicate channel {channel:?} in manifest")
            }
            Self::UnknownChannel { channel, symbol } => write!(
                formatter,
                "entry {symbol:?} references unregistered channel {channel:?}"
            ),
            Self::InvalidState { symbol, state } => write!(
                formatter,
                "entry {symbol:?} has invalid state {state:?}; expected ACTIVE, DEPRECATED, or SUNSET"
            ),
            Self::InvalidFieldNumber {
                channel,
                symbol,
                field_number,
            } => write!(
                formatter,
                "entry {symbol:?} in {channel:?} has invalid field_number {field_number}"
            ),
            Self::InvalidStateMetadata {
                channel,
                symbol,
                detail,
            } => write!(
                formatter,
                "entry {symbol:?} in {channel:?} has inconsistent state metadata: {detail}"
            ),
            Self::MissingDeprecationMetadata {
                channel,
                symbol,
                field,
            } => write!(
                formatter,
                "entry {symbol:?} in {channel:?} is missing required field {field:?}"
            ),
            Self::InsufficientDeprecationWindow {
                channel,
                symbol,
                deprecated_in_epoch,
                earliest_sunset_epoch,
                required_window_epochs,
            } => write!(
                formatter,
                "entry {symbol:?} in {channel:?} has earliest_sunset_epoch={earliest_sunset_epoch} < deprecated_in_epoch={deprecated_in_epoch} + {required_window_epochs}"
            ),
            Self::PrematureSunset {
                channel,
                symbol,
                current_epoch,
                earliest_sunset_epoch,
                sunset_in_epoch,
            } => write!(
                formatter,
                "entry {symbol:?} in {channel:?} cannot sunset at epoch {sunset_in_epoch} (current_epoch={current_epoch}, earliest_sunset_epoch={earliest_sunset_epoch})"
            ),
            Self::ReservedSlotReuse {
                channel,
                symbol,
                conflict,
            } => write!(
                formatter,
                "entry {symbol:?} in {channel:?} reuses permanently reserved slot/symbol: {conflict}"
            ),
            Self::DuplicateEntry { channel, symbol } => {
                write!(
                    formatter,
                    "duplicate entry {symbol:?} in channel {channel:?}"
                )
            }
            Self::RegistryMismatch { channel, detail } => write!(
                formatter,
                "manifest/registry mismatch for channel {channel:?}: {detail}"
            ),
        }
    }
}

impl Error for DeprecationManifestError {}

impl DeprecationManifest {
    /// Looks up a channel record by canonical SABI channel name.
    #[must_use]
    pub fn channel(&self, name: &str) -> Option<&ChannelDeprecationRecord> {
        self.channels.iter().find(|channel| channel.name == name)
    }

    /// Looks up a managed entry by `(channel, symbol)`.
    #[must_use]
    pub fn entry(&self, channel: &str, symbol: &str) -> Option<&DeprecationEntry> {
        self.entries
            .iter()
            .find(|entry| entry.channel == channel && entry.symbol == symbol)
    }

    /// Cross-checks this manifest against a slice of [`SchemaDescriptor`]s (such as
    /// [`schema_registry`]), requiring 100% agreement on channel count, names,
    /// `major`, `minor`, and `frozen == true`.
    ///
    /// # Errors
    ///
    /// Returns [`DeprecationManifestError::RegistryMismatch`] on any discrepancy.
    pub fn validate_against_registry(
        &self,
        registry: &[SchemaDescriptor],
    ) -> Result<(), DeprecationManifestError> {
        if self.channels.len() != registry.len() {
            return Err(DeprecationManifestError::RegistryMismatch {
                channel: "<all>".to_owned(),
                detail: format!(
                    "manifest has {} channels, registry has {}",
                    self.channels.len(),
                    registry.len()
                ),
            });
        }
        for descriptor in registry {
            if !descriptor.frozen {
                return Err(DeprecationManifestError::RegistryMismatch {
                    channel: descriptor.name.to_owned(),
                    detail: "registry descriptor is not frozen".to_owned(),
                });
            }
            let Some(channel) = self.channel(descriptor.name) else {
                return Err(DeprecationManifestError::RegistryMismatch {
                    channel: descriptor.name.to_owned(),
                    detail: "channel missing from deprecation manifest".to_owned(),
                });
            };
            if !channel.frozen {
                return Err(DeprecationManifestError::RegistryMismatch {
                    channel: descriptor.name.to_owned(),
                    detail: "manifest channel is not marked frozen".to_owned(),
                });
            }
            if channel.major != descriptor.major || channel.minor != descriptor.minor {
                return Err(DeprecationManifestError::RegistryMismatch {
                    channel: descriptor.name.to_owned(),
                    detail: format!(
                        "manifest version {}.{} != registry version {}.{}",
                        channel.major, channel.minor, descriptor.major, descriptor.minor
                    ),
                });
            }
        }
        Ok(())
    }

    /// Audits a target symbol within `channel` against channel-level reserved
    /// tombstones and entry lifecycle states.
    #[must_use]
    pub fn audit_symbol(&self, channel: &str, symbol: &str) -> SymbolAuditVerdict<'_> {
        if let Some(ch) = self.channel(channel)
            && ch
                .reserved_symbols
                .iter()
                .any(|reserved| reserved == symbol)
        {
            return SymbolAuditVerdict::SunsetOrReserved {
                entry: None,
                reason: "symbol is permanently reserved in channel tombstone list",
            };
        }
        let Some(entry) = self.entry(channel, symbol) else {
            return SymbolAuditVerdict::Active(None);
        };
        match entry.state {
            DeprecationState::Active => SymbolAuditVerdict::Active(Some(entry)),
            DeprecationState::Deprecated => SymbolAuditVerdict::Deprecated(entry),
            DeprecationState::Sunset => SymbolAuditVerdict::SunsetOrReserved {
                entry: Some(entry),
                reason: "symbol has reached SUNSET and is permanently retired",
            },
        }
    }

    /// Audits a field or feature/extension number within `channel` (optionally scoped
    /// to `parent_type`) against reserved field tombstones and entry lifecycle states.
    #[must_use]
    pub fn audit_field_number(
        &self,
        channel: &str,
        parent_type: Option<&str>,
        field_number: u32,
    ) -> SymbolAuditVerdict<'_> {
        if let Some(ch) = self.channel(channel)
            && ch.reserved_field_numbers.contains(&field_number)
        {
            return SymbolAuditVerdict::SunsetOrReserved {
                entry: None,
                reason: "field number is permanently reserved in channel tombstone list",
            };
        }
        let matched = self.entries.iter().find(|entry| {
            entry.channel == channel
                && entry.field_number == Some(field_number)
                && match parent_type {
                    Some(parent) => entry.parent_type.as_deref() == Some(parent),
                    None => true,
                }
        });
        let Some(entry) = matched else {
            return SymbolAuditVerdict::Active(None);
        };
        match entry.state {
            DeprecationState::Active => SymbolAuditVerdict::Active(Some(entry)),
            DeprecationState::Deprecated => SymbolAuditVerdict::Deprecated(entry),
            DeprecationState::Sunset => SymbolAuditVerdict::SunsetOrReserved {
                entry: Some(entry),
                reason: "field number has reached SUNSET and is permanently reserved",
            },
        }
    }
}

/// Parses and validates a machine-readable SABI deprecation manifest YAML document.
///
/// # Errors
///
/// Fails closed with [`DeprecationManifestError`] if the YAML syntax is malformed,
/// exceeds [`MAX_DEPRECATION_MANIFEST_BYTES`], or violates any `ADR-0027` §3.2
/// lifecycle invariant (`min_compatibility_window_epochs >= 2`, two-epoch minimum
/// deprecation grace window, non-premature sunset, and permanent `RESERVED`
/// slot/symbol non-reuse).
pub fn parse_deprecation_manifest(
    yaml: &str,
) -> Result<DeprecationManifest, DeprecationManifestError> {
    if yaml.len() > MAX_DEPRECATION_MANIFEST_BYTES {
        return Err(DeprecationManifestError::ManifestTooLarge {
            actual: yaml.len(),
            maximum: MAX_DEPRECATION_MANIFEST_BYTES,
        });
    }
    if yaml.contains('\0') {
        return Err(DeprecationManifestError::SyntaxError {
            line: 1,
            message: "manifest contains NUL byte".to_owned(),
        });
    }

    let raw = parse_raw_manifest(yaml)?;
    validate_manifest(raw)
}

/// Loads the embedded `schema/nlos/sabi/v1/deprecation-manifest.yaml` and cross-checks
/// all 9 channels (`name`, `major`, `minor`, `frozen == true`) against [`schema_registry`].
///
/// # Errors
///
/// Fails closed if the embedded manifest fails [`parse_deprecation_manifest`] or
/// diverges from [`schema_registry`].
pub fn canonical_deprecation_manifest() -> Result<DeprecationManifest, DeprecationManifestError> {
    let manifest = parse_deprecation_manifest(CANONICAL_DEPRECATION_MANIFEST_YAML)?;
    manifest.validate_against_registry(schema_registry())?;
    Ok(manifest)
}

#[derive(Default)]
struct RawChannel {
    line: usize,
    name: Option<String>,
    major: Option<u32>,
    minor: Option<u32>,
    frozen: Option<bool>,
    reserved_field_numbers: Vec<u32>,
    reserved_symbols: Vec<String>,
}

#[derive(Default)]
struct RawEntry {
    line: usize,
    channel: Option<String>,
    symbol: Option<String>,
    kind: Option<ContractElementKind>,
    parent_type: Option<String>,
    field_number: Option<u32>,
    introduced_in_epoch: Option<u32>,
    state: Option<String>,
    deprecated_in_epoch: Option<u32>,
    earliest_sunset_epoch: Option<u32>,
    sunset_in_epoch: Option<u32>,
    replacement: Option<String>,
    migration_rule_id: Option<String>,
    reason: Option<String>,
}

#[derive(Default)]
struct RawManifest {
    manifest_version: Option<u32>,
    current_epoch: Option<u32>,
    min_compatibility_window_epochs: Option<u32>,
    frozen_channels_total: Option<usize>,
    channels: Vec<RawChannel>,
    entries: Vec<RawEntry>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Section {
    Root,
    Channels,
    Entries,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PendingList {
    None,
    ReservedFieldNumbers { indent: usize },
    ReservedSymbols { indent: usize },
}

#[allow(clippy::too_many_lines)]
fn parse_raw_manifest(yaml: &str) -> Result<RawManifest, DeprecationManifestError> {
    let mut raw = RawManifest::default();
    let mut section = Section::Root;
    let mut pending_list = PendingList::None;

    for (idx, raw_line) in yaml.lines().enumerate() {
        let line_no = idx + 1;
        let stripped = strip_yaml_comment(raw_line, line_no)?;
        if stripped.trim().is_empty() {
            continue;
        }
        if stripped.bytes().any(|b| b == b'\t') {
            return Err(DeprecationManifestError::SyntaxError {
                line: line_no,
                message: "YAML tabs are not permitted; use spaces".to_owned(),
            });
        }

        let indent = stripped.len() - stripped.trim_start().len();
        let content = stripped.trim();

        match pending_list {
            PendingList::ReservedFieldNumbers { indent: min_indent } if indent > min_indent => {
                let Some(item) = content.strip_prefix('-') else {
                    return Err(DeprecationManifestError::SyntaxError {
                        line: line_no,
                        message: "expected '-' list item in reserved_field_numbers".to_owned(),
                    });
                };
                let value = parse_u32(item.trim(), line_no, "reserved_field_numbers")?;
                let current = raw.channels.last_mut().ok_or_else(|| {
                    DeprecationManifestError::SyntaxError {
                        line: line_no,
                        message: "unexpected reserved_field_numbers item without channel"
                            .to_owned(),
                    }
                })?;
                current.reserved_field_numbers.push(value);
                continue;
            }
            PendingList::ReservedSymbols { indent: min_indent } if indent > min_indent => {
                let Some(item) = content.strip_prefix('-') else {
                    return Err(DeprecationManifestError::SyntaxError {
                        line: line_no,
                        message: "expected '-' list item in reserved_symbols".to_owned(),
                    });
                };
                let value = parse_string(item.trim(), line_no)?;
                let current = raw.channels.last_mut().ok_or_else(|| {
                    DeprecationManifestError::SyntaxError {
                        line: line_no,
                        message: "unexpected reserved_symbols item without channel".to_owned(),
                    }
                })?;
                current.reserved_symbols.push(value);
                continue;
            }
            _ => {
                pending_list = PendingList::None;
            }
        }

        if indent == 0 {
            let (key, val) = split_key_value(content, line_no)?;
            match key {
                "manifest_version" => {
                    raw.manifest_version = Some(parse_u32(val, line_no, "manifest_version")?);
                    section = Section::Root;
                }
                "current_epoch" => {
                    raw.current_epoch = Some(parse_u32(val, line_no, "current_epoch")?);
                    section = Section::Root;
                }
                "min_compatibility_window_epochs" => {
                    raw.min_compatibility_window_epochs =
                        Some(parse_u32(val, line_no, "min_compatibility_window_epochs")?);
                    section = Section::Root;
                }
                "frozen_channels_total" => {
                    let count = parse_u32(val, line_no, "frozen_channels_total")?;
                    raw.frozen_channels_total = Some(count as usize);
                    section = Section::Root;
                }
                "channels" => {
                    if val == "[]" {
                        section = Section::Root;
                    } else if val.is_empty() {
                        section = Section::Channels;
                    } else {
                        return Err(DeprecationManifestError::SyntaxError {
                            line: line_no,
                            message: "channels must be followed by empty value or []".to_owned(),
                        });
                    }
                }
                "entries" => {
                    if val == "[]" {
                        section = Section::Root;
                    } else if val.is_empty() {
                        section = Section::Entries;
                    } else {
                        return Err(DeprecationManifestError::SyntaxError {
                            line: line_no,
                            message: "entries must be followed by empty value or []".to_owned(),
                        });
                    }
                }
                other => {
                    return Err(DeprecationManifestError::SyntaxError {
                        line: line_no,
                        message: format!("unknown top-level key {other:?}"),
                    });
                }
            }
            continue;
        }

        match section {
            Section::Root => {
                return Err(DeprecationManifestError::SyntaxError {
                    line: line_no,
                    message: "unexpected indented line outside channels/entries section".to_owned(),
                });
            }
            Section::Channels => {
                if let Some(after_dash) = content.strip_prefix('-') {
                    raw.channels.push(RawChannel {
                        line: line_no,
                        ..RawChannel::default()
                    });
                    let trimmed = after_dash.trim();
                    if !trimmed.is_empty() {
                        let prop_indent = stripped.len() - after_dash.trim_start().len();
                        let channel = raw.channels.last_mut().expect("just pushed RawChannel");
                        pending_list = apply_channel_field(channel, trimmed, prop_indent, line_no)?;
                    }
                } else {
                    let channel = raw.channels.last_mut().ok_or_else(|| {
                        DeprecationManifestError::SyntaxError {
                            line: line_no,
                            message: "channel mapping field precedes '-' list marker".to_owned(),
                        }
                    })?;
                    pending_list = apply_channel_field(channel, content, indent, line_no)?;
                }
            }
            Section::Entries => {
                if let Some(after_dash) = content.strip_prefix('-') {
                    raw.entries.push(RawEntry {
                        line: line_no,
                        ..RawEntry::default()
                    });
                    let trimmed = after_dash.trim();
                    if !trimmed.is_empty() {
                        let entry = raw.entries.last_mut().expect("just pushed RawEntry");
                        apply_entry_field(entry, trimmed, line_no)?;
                    }
                } else {
                    let entry = raw.entries.last_mut().ok_or_else(|| {
                        DeprecationManifestError::SyntaxError {
                            line: line_no,
                            message: "entry mapping field precedes '-' list marker".to_owned(),
                        }
                    })?;
                    apply_entry_field(entry, content, line_no)?;
                }
            }
        }
    }

    Ok(raw)
}

fn apply_channel_field(
    channel: &mut RawChannel,
    content: &str,
    field_indent: usize,
    line: usize,
) -> Result<PendingList, DeprecationManifestError> {
    let (key, val) = split_key_value(content, line)?;
    match key {
        "name" => channel.name = Some(parse_string(val, line)?),
        "major" => channel.major = Some(parse_u32(val, line, "channel.major")?),
        "minor" => channel.minor = Some(parse_u32(val, line, "channel.minor")?),
        "frozen" => channel.frozen = Some(parse_bool(val, line, "channel.frozen")?),
        "reserved_field_numbers" => {
            if val.is_empty() {
                return Ok(PendingList::ReservedFieldNumbers {
                    indent: field_indent,
                });
            }
            channel.reserved_field_numbers = parse_inline_u32_list(val, line)?;
        }
        "reserved_symbols" => {
            if val.is_empty() {
                return Ok(PendingList::ReservedSymbols {
                    indent: field_indent,
                });
            }
            channel.reserved_symbols = parse_inline_string_list(val, line)?;
        }
        other => {
            return Err(DeprecationManifestError::SyntaxError {
                line,
                message: format!("unknown channel key {other:?}"),
            });
        }
    }
    Ok(PendingList::None)
}

fn apply_entry_field(
    entry: &mut RawEntry,
    content: &str,
    line: usize,
) -> Result<(), DeprecationManifestError> {
    let (key, val) = split_key_value(content, line)?;
    match key {
        "channel" => entry.channel = Some(parse_string(val, line)?),
        "symbol" => entry.symbol = Some(parse_string(val, line)?),
        "kind" => {
            let kind_str = parse_string(val, line)?;
            entry.kind = Some(ContractElementKind::parse(&kind_str, line)?);
        }
        "parent_type" => entry.parent_type = parse_optional_string(val, line)?,
        "field_number" => entry.field_number = parse_optional_u32(val, line, "field_number")?,
        "introduced_in_epoch" => {
            entry.introduced_in_epoch = Some(parse_u32(val, line, "introduced_in_epoch")?);
        }
        "state" => entry.state = Some(parse_string(val, line)?),
        "deprecated_in_epoch" => {
            entry.deprecated_in_epoch = parse_optional_u32(val, line, "deprecated_in_epoch")?;
        }
        "earliest_sunset_epoch" => {
            entry.earliest_sunset_epoch = parse_optional_u32(val, line, "earliest_sunset_epoch")?;
        }
        "sunset_in_epoch" => {
            entry.sunset_in_epoch = parse_optional_u32(val, line, "sunset_in_epoch")?;
        }
        "replacement" => entry.replacement = parse_optional_string(val, line)?,
        "migration_rule_id" => entry.migration_rule_id = parse_optional_string(val, line)?,
        "reason" => entry.reason = parse_optional_string(val, line)?,
        other => {
            return Err(DeprecationManifestError::SyntaxError {
                line,
                message: format!("unknown entry key {other:?}"),
            });
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate_manifest(raw: RawManifest) -> Result<DeprecationManifest, DeprecationManifestError> {
    let manifest_version =
        raw.manifest_version
            .ok_or(DeprecationManifestError::MissingTopLevelField(
                "manifest_version",
            ))?;
    if manifest_version != 1 {
        return Err(DeprecationManifestError::UnsupportedManifestVersion(
            manifest_version,
        ));
    }

    let current_epoch = raw
        .current_epoch
        .ok_or(DeprecationManifestError::MissingTopLevelField(
            "current_epoch",
        ))?;
    if current_epoch == 0 {
        return Err(DeprecationManifestError::InvalidEpoch {
            context: "current_epoch".to_owned(),
            epoch: 0,
        });
    }

    let min_compatibility_window_epochs = raw.min_compatibility_window_epochs.ok_or(
        DeprecationManifestError::MissingTopLevelField("min_compatibility_window_epochs"),
    )?;
    if min_compatibility_window_epochs < MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS {
        return Err(DeprecationManifestError::InvalidCompatibilityWindow {
            got: min_compatibility_window_epochs,
            minimum: MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
        });
    }

    let frozen_channels_total =
        raw.frozen_channels_total
            .ok_or(DeprecationManifestError::MissingTopLevelField(
                "frozen_channels_total",
            ))?;

    let mut channels = Vec::with_capacity(raw.channels.len());
    let mut seen_channels = HashSet::new();
    let mut channel_reserved_fields: HashSet<(String, u32)> = HashSet::new();
    let mut reserved_symbols: HashSet<(String, String)> = HashSet::new();

    for raw_ch in raw.channels {
        let name = raw_ch
            .name
            .ok_or_else(|| DeprecationManifestError::SyntaxError {
                line: raw_ch.line,
                message: "channel is missing 'name'".to_owned(),
            })?;
        if name.is_empty() {
            return Err(DeprecationManifestError::SyntaxError {
                line: raw_ch.line,
                message: "channel name must not be empty".to_owned(),
            });
        }
        if !seen_channels.insert(name.clone()) {
            return Err(DeprecationManifestError::DuplicateChannel(name));
        }
        let major = raw_ch
            .major
            .ok_or_else(|| DeprecationManifestError::SyntaxError {
                line: raw_ch.line,
                message: format!("channel {name:?} is missing 'major'"),
            })?;
        if major == 0 {
            return Err(DeprecationManifestError::InvalidEpoch {
                context: format!("channel {name:?} major"),
                epoch: 0,
            });
        }
        let minor = raw_ch
            .minor
            .ok_or_else(|| DeprecationManifestError::SyntaxError {
                line: raw_ch.line,
                message: format!("channel {name:?} is missing 'minor'"),
            })?;
        let frozen = raw_ch
            .frozen
            .ok_or_else(|| DeprecationManifestError::SyntaxError {
                line: raw_ch.line,
                message: format!("channel {name:?} is missing 'frozen'"),
            })?;
        if !frozen {
            return Err(DeprecationManifestError::UnfrozenChannel(name));
        }

        for &field_num in &raw_ch.reserved_field_numbers {
            if field_num == 0 {
                return Err(DeprecationManifestError::InvalidFieldNumber {
                    channel: name,
                    symbol: "<reserved_field_numbers>".to_owned(),
                    field_number: 0,
                });
            }
            if !channel_reserved_fields.insert((name.clone(), field_num)) {
                return Err(DeprecationManifestError::SyntaxError {
                    line: raw_ch.line,
                    message: format!(
                        "duplicate reserved field number {field_num} in channel {name:?}"
                    ),
                });
            }
        }

        for sym in &raw_ch.reserved_symbols {
            if sym.trim().is_empty() {
                return Err(DeprecationManifestError::SyntaxError {
                    line: raw_ch.line,
                    message: format!("empty symbol in {name:?} reserved_symbols"),
                });
            }
            if !reserved_symbols.insert((name.clone(), sym.clone())) {
                return Err(DeprecationManifestError::SyntaxError {
                    line: raw_ch.line,
                    message: format!("duplicate reserved symbol {sym:?} in channel {name:?}"),
                });
            }
        }

        channels.push(ChannelDeprecationRecord {
            name,
            major,
            minor,
            frozen,
            reserved_field_numbers: raw_ch.reserved_field_numbers,
            reserved_symbols: raw_ch.reserved_symbols,
        });
    }

    if frozen_channels_total != channels.len() {
        return Err(DeprecationManifestError::ChannelCountMismatch {
            declared: frozen_channels_total,
            actual: channels.len(),
        });
    }

    let mut entries = Vec::with_capacity(raw.entries.len());
    let mut sunset_symbols: HashSet<(String, String)> = HashSet::new();
    let mut sunset_fields: HashSet<(String, String, u32)> = HashSet::new();
    let mut active_or_deprecated_symbols: HashSet<(String, String)> = HashSet::new();
    let mut active_or_deprecated_fields: HashSet<(String, String, u32)> = HashSet::new();

    for raw_entry in raw.entries {
        let channel = raw_entry
            .channel
            .ok_or_else(|| DeprecationManifestError::SyntaxError {
                line: raw_entry.line,
                message: "entry is missing 'channel'".to_owned(),
            })?;
        let symbol = raw_entry
            .symbol
            .ok_or_else(|| DeprecationManifestError::SyntaxError {
                line: raw_entry.line,
                message: "entry is missing 'symbol'".to_owned(),
            })?;
        if symbol.trim().is_empty() {
            return Err(DeprecationManifestError::SyntaxError {
                line: raw_entry.line,
                message: format!("entry in channel {channel:?} has empty symbol"),
            });
        }
        if !seen_channels.contains(&channel) {
            return Err(DeprecationManifestError::UnknownChannel { channel, symbol });
        }

        if let Some(field_num) = raw_entry.field_number
            && field_num == 0
        {
            return Err(DeprecationManifestError::InvalidFieldNumber {
                channel,
                symbol,
                field_number: 0,
            });
        }

        let raw_state = raw_entry
            .state
            .ok_or_else(|| DeprecationManifestError::SyntaxError {
                line: raw_entry.line,
                message: format!("entry {symbol:?} is missing 'state'"),
            })?;
        let state = DeprecationState::parse(&raw_state, &symbol)?;

        let introduced_in_epoch = raw_entry.introduced_in_epoch.unwrap_or(1);
        if introduced_in_epoch == 0 || introduced_in_epoch > current_epoch {
            return Err(DeprecationManifestError::InvalidEpoch {
                context: format!("entry {symbol:?} introduced_in_epoch"),
                epoch: introduced_in_epoch,
            });
        }

        let kind = raw_entry
            .kind
            .unwrap_or(if raw_entry.field_number.is_some() {
                ContractElementKind::Field
            } else {
                ContractElementKind::Message
            });

        validate_entry_lifecycle(
            &channel,
            &symbol,
            state,
            current_epoch,
            min_compatibility_window_epochs,
            raw_entry.deprecated_in_epoch,
            raw_entry.earliest_sunset_epoch,
            raw_entry.sunset_in_epoch,
            raw_entry.replacement.as_deref(),
            raw_entry.reason.as_deref(),
        )?;

        let parent_scope = raw_entry.parent_type.clone().unwrap_or_default();
        match state {
            DeprecationState::Sunset => {
                if !sunset_symbols.insert((channel.clone(), symbol.clone())) {
                    return Err(DeprecationManifestError::DuplicateEntry { channel, symbol });
                }
                if let Some(field_num) = raw_entry.field_number
                    && !sunset_fields.insert((channel.clone(), parent_scope, field_num))
                {
                    return Err(DeprecationManifestError::DuplicateEntry { channel, symbol });
                }
            }
            DeprecationState::Active | DeprecationState::Deprecated => {
                if !active_or_deprecated_symbols.insert((channel.clone(), symbol.clone())) {
                    return Err(DeprecationManifestError::DuplicateEntry { channel, symbol });
                }
                if let Some(field_num) = raw_entry.field_number
                    && !active_or_deprecated_fields.insert((
                        channel.clone(),
                        parent_scope,
                        field_num,
                    ))
                {
                    return Err(DeprecationManifestError::DuplicateEntry { channel, symbol });
                }
            }
        }

        entries.push(DeprecationEntry {
            channel,
            symbol,
            kind,
            parent_type: raw_entry.parent_type,
            field_number: raw_entry.field_number,
            introduced_in_epoch,
            state,
            deprecated_in_epoch: raw_entry.deprecated_in_epoch,
            earliest_sunset_epoch: raw_entry.earliest_sunset_epoch,
            sunset_in_epoch: raw_entry.sunset_in_epoch,
            replacement: raw_entry.replacement,
            migration_rule_id: raw_entry.migration_rule_id,
            reason: raw_entry.reason,
        });
    }

    // Second pass (order-independent): enforce Permanent RESERVED rule across all
    // Active and Deprecated entries against both channel-level tombstones and
    // Sunset entries anywhere in the manifest.
    for entry in &entries {
        if entry.state == DeprecationState::Sunset {
            continue;
        }
        if reserved_symbols.contains(&(entry.channel.clone(), entry.symbol.clone())) {
            return Err(DeprecationManifestError::ReservedSlotReuse {
                channel: entry.channel.clone(),
                symbol: entry.symbol.clone(),
                conflict: format!("symbol {:?} is in channel reserved_symbols", entry.symbol),
            });
        }
        if sunset_symbols.contains(&(entry.channel.clone(), entry.symbol.clone())) {
            return Err(DeprecationManifestError::ReservedSlotReuse {
                channel: entry.channel.clone(),
                symbol: entry.symbol.clone(),
                conflict: format!("symbol {:?} was previously SUNSET", entry.symbol),
            });
        }
        if let Some(field_num) = entry.field_number {
            if channel_reserved_fields.contains(&(entry.channel.clone(), field_num)) {
                return Err(DeprecationManifestError::ReservedSlotReuse {
                    channel: entry.channel.clone(),
                    symbol: entry.symbol.clone(),
                    conflict: format!(
                        "field_number {field_num} is in channel reserved_field_numbers"
                    ),
                });
            }
            let parent_scope = entry.parent_type.clone().unwrap_or_default();
            if sunset_fields.contains(&(entry.channel.clone(), parent_scope.clone(), field_num)) {
                return Err(DeprecationManifestError::ReservedSlotReuse {
                    channel: entry.channel.clone(),
                    symbol: entry.symbol.clone(),
                    conflict: format!(
                        "field_number {field_num} on parent {parent_scope:?} was previously SUNSET"
                    ),
                });
            }
        }
    }

    Ok(DeprecationManifest {
        manifest_version,
        current_epoch,
        min_compatibility_window_epochs,
        frozen_channels_total,
        channels,
        entries,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate_entry_lifecycle(
    channel: &str,
    symbol: &str,
    state: DeprecationState,
    current_epoch: u32,
    min_window: u32,
    deprecated_in_epoch: Option<u32>,
    earliest_sunset_epoch: Option<u32>,
    sunset_in_epoch: Option<u32>,
    replacement: Option<&str>,
    reason: Option<&str>,
) -> Result<(), DeprecationManifestError> {
    match state {
        DeprecationState::Active => {
            if deprecated_in_epoch.is_some()
                || earliest_sunset_epoch.is_some()
                || sunset_in_epoch.is_some()
            {
                return Err(DeprecationManifestError::InvalidStateMetadata {
                    channel: channel.to_owned(),
                    symbol: symbol.to_owned(),
                    detail: "ACTIVE entry must not set deprecation or sunset epochs",
                });
            }
            Ok(())
        }
        DeprecationState::Deprecated => {
            if sunset_in_epoch.is_some() {
                return Err(DeprecationManifestError::InvalidStateMetadata {
                    channel: channel.to_owned(),
                    symbol: symbol.to_owned(),
                    detail: "DEPRECATED entry must not set sunset_in_epoch",
                });
            }
            let (dep_epoch, earliest_epoch) = require_deprecation_metadata(
                channel,
                symbol,
                current_epoch,
                min_window,
                deprecated_in_epoch,
                earliest_sunset_epoch,
                replacement,
                reason,
            )?;
            let _ = (dep_epoch, earliest_epoch);
            Ok(())
        }
        DeprecationState::Sunset => {
            let (_dep_epoch, earliest_epoch) = require_deprecation_metadata(
                channel,
                symbol,
                current_epoch,
                min_window,
                deprecated_in_epoch,
                earliest_sunset_epoch,
                replacement,
                reason,
            )?;
            let sunset_epoch = sunset_in_epoch.ok_or_else(|| {
                DeprecationManifestError::MissingDeprecationMetadata {
                    channel: channel.to_owned(),
                    symbol: symbol.to_owned(),
                    field: "sunset_in_epoch",
                }
            })?;
            if sunset_epoch < earliest_epoch || current_epoch < sunset_epoch {
                return Err(DeprecationManifestError::PrematureSunset {
                    channel: channel.to_owned(),
                    symbol: symbol.to_owned(),
                    current_epoch,
                    earliest_sunset_epoch: earliest_epoch,
                    sunset_in_epoch: sunset_epoch,
                });
            }
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn require_deprecation_metadata(
    channel: &str,
    symbol: &str,
    current_epoch: u32,
    min_window: u32,
    deprecated_in_epoch: Option<u32>,
    earliest_sunset_epoch: Option<u32>,
    replacement: Option<&str>,
    reason: Option<&str>,
) -> Result<(u32, u32), DeprecationManifestError> {
    let dep_epoch = deprecated_in_epoch.ok_or_else(|| {
        DeprecationManifestError::MissingDeprecationMetadata {
            channel: channel.to_owned(),
            symbol: symbol.to_owned(),
            field: "deprecated_in_epoch",
        }
    })?;
    if dep_epoch == 0 || dep_epoch > current_epoch {
        return Err(DeprecationManifestError::InvalidEpoch {
            context: format!("entry {symbol:?} deprecated_in_epoch"),
            epoch: dep_epoch,
        });
    }
    let earliest_epoch = earliest_sunset_epoch.ok_or_else(|| {
        DeprecationManifestError::MissingDeprecationMetadata {
            channel: channel.to_owned(),
            symbol: symbol.to_owned(),
            field: "earliest_sunset_epoch",
        }
    })?;
    if replacement.is_none_or(|value| value.trim().is_empty()) {
        return Err(DeprecationManifestError::MissingDeprecationMetadata {
            channel: channel.to_owned(),
            symbol: symbol.to_owned(),
            field: "replacement",
        });
    }
    if reason.is_none_or(|value| value.trim().is_empty()) {
        return Err(DeprecationManifestError::MissingDeprecationMetadata {
            channel: channel.to_owned(),
            symbol: symbol.to_owned(),
            field: "reason",
        });
    }
    let min_allowed_sunset = dep_epoch.checked_add(min_window).ok_or_else(|| {
        DeprecationManifestError::InvalidEpoch {
            context: format!("entry {symbol:?} earliest_sunset_epoch overflow"),
            epoch: earliest_epoch,
        }
    })?;
    if earliest_epoch < min_allowed_sunset {
        return Err(DeprecationManifestError::InsufficientDeprecationWindow {
            channel: channel.to_owned(),
            symbol: symbol.to_owned(),
            deprecated_in_epoch: dep_epoch,
            earliest_sunset_epoch: earliest_epoch,
            required_window_epochs: min_window,
        });
    }
    Ok((dep_epoch, earliest_epoch))
}

fn strip_yaml_comment(line: &str, line_no: usize) -> Result<&str, DeprecationManifestError> {
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;

    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => {
                escaped = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '#' if !in_double
                && !in_single
                && (idx == 0 || line[..idx].ends_with(char::is_whitespace)) =>
            {
                return Ok(&line[..idx]);
            }
            _ => {}
        }
    }
    if in_double || in_single {
        return Err(DeprecationManifestError::SyntaxError {
            line: line_no,
            message: "unterminated quoted string".to_owned(),
        });
    }
    Ok(line)
}

fn split_key_value(content: &str, line: usize) -> Result<(&str, &str), DeprecationManifestError> {
    let Some((raw_key, raw_val)) = content.split_once(':') else {
        return Err(DeprecationManifestError::SyntaxError {
            line,
            message: format!("expected 'key: value', got {content:?}"),
        });
    };
    let key = raw_key.trim();
    if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(DeprecationManifestError::SyntaxError {
            line,
            message: format!("invalid mapping key {key:?}"),
        });
    }
    Ok((key, raw_val.trim()))
}

fn parse_u32(val: &str, line: usize, field: &str) -> Result<u32, DeprecationManifestError> {
    val.parse::<u32>()
        .map_err(|_| DeprecationManifestError::SyntaxError {
            line,
            message: format!("invalid unsigned integer {val:?} for {field}"),
        })
}

fn parse_optional_u32(
    val: &str,
    line: usize,
    field: &str,
) -> Result<Option<u32>, DeprecationManifestError> {
    if val.is_empty() || val == "null" || val == "~" {
        return Ok(None);
    }
    parse_u32(val, line, field).map(Some)
}

fn parse_bool(val: &str, line: usize, field: &str) -> Result<bool, DeprecationManifestError> {
    match val {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(DeprecationManifestError::SyntaxError {
            line,
            message: format!("invalid boolean {val:?} for {field}"),
        }),
    }
}

fn parse_optional_string(
    val: &str,
    line: usize,
) -> Result<Option<String>, DeprecationManifestError> {
    if val.is_empty() || val == "null" || val == "~" {
        return Ok(None);
    }
    parse_string(val, line).map(Some)
}

fn parse_string(val: &str, line: usize) -> Result<String, DeprecationManifestError> {
    if val.is_empty() {
        return Err(DeprecationManifestError::SyntaxError {
            line,
            message: "expected non-empty string scalar".to_owned(),
        });
    }
    if val.len() >= 2 && val.starts_with('"') && val.ends_with('"') {
        let inner = &val[1..val.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(other) => {
                        return Err(DeprecationManifestError::SyntaxError {
                            line,
                            message: format!("unsupported escape sequence \\{other}"),
                        });
                    }
                    None => {
                        return Err(DeprecationManifestError::SyntaxError {
                            line,
                            message: "trailing backslash in string".to_owned(),
                        });
                    }
                }
            } else {
                out.push(ch);
            }
        }
        return Ok(out);
    }
    if val.len() >= 2 && val.starts_with('\'') && val.ends_with('\'') {
        let inner = &val[1..val.len() - 1];
        return Ok(inner.replace("''", "'"));
    }
    if val.starts_with('"') || val.starts_with('\'') || val.starts_with('[') || val.starts_with('{')
    {
        return Err(DeprecationManifestError::SyntaxError {
            line,
            message: format!("malformed scalar {val:?}"),
        });
    }
    Ok(val.to_owned())
}

fn split_inline_array(val: &str, line: usize) -> Result<Vec<&str>, DeprecationManifestError> {
    let trimmed = val.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Err(DeprecationManifestError::SyntaxError {
            line,
            message: format!("expected inline list '[...]', got {val:?}"),
        });
    }
    let inner = trimmed[1..trimmed.len() - 1].trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }

    let mut items = Vec::new();
    let mut start = 0;
    let mut in_double = false;
    let mut in_single = false;
    let mut escaped = false;

    for (idx, ch) in inner.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            ',' if !in_double && !in_single => {
                let piece = inner[start..idx].trim();
                if piece.is_empty() {
                    return Err(DeprecationManifestError::SyntaxError {
                        line,
                        message: "empty element in inline list".to_owned(),
                    });
                }
                items.push(piece);
                start = idx + 1;
            }
            _ => {}
        }
    }
    let tail = inner[start..].trim();
    if tail.is_empty() {
        return Err(DeprecationManifestError::SyntaxError {
            line,
            message: "trailing comma in inline list".to_owned(),
        });
    }
    items.push(tail);
    Ok(items)
}

fn parse_inline_u32_list(val: &str, line: usize) -> Result<Vec<u32>, DeprecationManifestError> {
    split_inline_array(val, line)?
        .into_iter()
        .map(|item| parse_u32(item, line, "inline u32 list"))
        .collect()
}

fn parse_inline_string_list(
    val: &str,
    line: usize,
) -> Result<Vec<String>, DeprecationManifestError> {
    split_inline_array(val, line)?
        .into_iter()
        .map(|item| parse_string(item, line))
        .collect()
}
