//! Package compatibility declaration (`PackageCompatibilitySpec`) and fail-closed
//! `llmos.package` / `package.manifest` compatibility parser.
//!
//! Authority:
//! - `06-架构设计总纲-v0.5.md` section 9 (`COMPAT-VER-001`, `COMPAT-VER-002`)
//! - `ADR-0027` section 2.2 (per-package out-of-core compatibility shim mounting)

use std::error::Error;
use std::fmt;

use nlos_schema::MAX_EXTENSION_IDS;

/// Maximum UTF-8 byte length accepted when parsing a `package.manifest` document.
pub const MAX_COMPAT_MANIFEST_BYTES: usize = 64 * 1024;
/// Maximum number of `required_features` entries declared by one package.
pub const MAX_REQUIRED_FEATURES: usize = 64;
/// Maximum UTF-8 byte length of a single feature identifier.
pub const MAX_FEATURE_NAME_BYTES: usize = 128;

/// Compatibility contract declared in a `llmos.package` / `package.manifest` artifact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageCompatibilitySpec {
    /// Declared SABI major epoch (`>= 1` for valid packages; `0` is rejected at mount).
    pub target_sabi_epoch: u32,
    /// Declared minimum SABI minor version within `target_sabi_epoch`.
    pub target_sabi_minor: u32,
    /// Optional driver ABI epoch when the package bundles a `DriverProvider` adapter.
    pub driver_abi_epoch: Option<u32>,
    /// Required capability/protocol feature tokens negotiated at mount time.
    pub required_features: Vec<String>,
    /// Critical `SchemaIdentity` extension IDs required by the package (`[COMPAT-VER-002]`).
    pub critical_extensions: Vec<u32>,
    /// Non-critical `SchemaIdentity` extension IDs carried by the package (`[COMPAT-VER-002]`).
    pub non_critical_extensions: Vec<u32>,
}

impl Default for PackageCompatibilitySpec {
    fn default() -> Self {
        Self {
            target_sabi_epoch: 1,
            target_sabi_minor: 0,
            driver_abi_epoch: None,
            required_features: Vec::new(),
            critical_extensions: Vec::new(),
            non_critical_extensions: Vec::new(),
        }
    }
}

impl PackageCompatibilitySpec {
    /// Constructs a baseline compatibility spec for `(target_sabi_epoch, target_sabi_minor)`.
    #[must_use]
    pub const fn new(target_sabi_epoch: u32, target_sabi_minor: u32) -> Self {
        Self {
            target_sabi_epoch,
            target_sabi_minor,
            driver_abi_epoch: None,
            required_features: Vec::new(),
            critical_extensions: Vec::new(),
            non_critical_extensions: Vec::new(),
        }
    }

    /// Attaches an explicit `driver_abi_epoch` declaration.
    #[must_use]
    pub const fn with_driver_abi_epoch(mut self, driver_abi_epoch: u32) -> Self {
        self.driver_abi_epoch = Some(driver_abi_epoch);
        self
    }

    /// Attaches `required_features` to this compatibility declaration.
    #[must_use]
    pub fn with_required_features(mut self, features: Vec<String>) -> Self {
        self.required_features = features;
        self
    }

    /// Attaches `critical_extensions` to this compatibility declaration.
    #[must_use]
    pub fn with_critical_extensions(mut self, extensions: Vec<u32>) -> Self {
        self.critical_extensions = extensions;
        self
    }

    /// Attaches `non_critical_extensions` to this compatibility declaration.
    #[must_use]
    pub fn with_non_critical_extensions(mut self, extensions: Vec<u32>) -> Self {
        self.non_critical_extensions = extensions;
        self
    }

    /// Validates bounded structural invariants on the compatibility declaration.
    ///
    /// Note that `target_sabi_epoch == 0` or unsupported critical extension IDs are
    /// adjudicated by [`crate::mount::evaluate_compat_shim_mount`] so they map to
    /// canonical `SabiErrorCode::AbiVersion` mount rejections with upgrade guidance.
    ///
    /// # Errors
    ///
    /// Returns [`ManifestCompatibilityError`] if extension lists or feature lists
    /// exceed bounded limits or contain malformed feature tokens.
    pub fn validate_bounds(&self) -> Result<(), ManifestCompatibilityError> {
        if self.critical_extensions.len() > MAX_EXTENSION_IDS {
            return Err(ManifestCompatibilityError::TooManyExtensions {
                kind: "critical_extensions",
                actual: self.critical_extensions.len(),
                maximum: MAX_EXTENSION_IDS,
            });
        }
        if self.non_critical_extensions.len() > MAX_EXTENSION_IDS {
            return Err(ManifestCompatibilityError::TooManyExtensions {
                kind: "non_critical_extensions",
                actual: self.non_critical_extensions.len(),
                maximum: MAX_EXTENSION_IDS,
            });
        }
        if self.required_features.len() > MAX_REQUIRED_FEATURES {
            return Err(ManifestCompatibilityError::TooManyRequiredFeatures {
                actual: self.required_features.len(),
                maximum: MAX_REQUIRED_FEATURES,
            });
        }
        for feature in &self.required_features {
            validate_feature_token(feature)?;
        }
        Ok(())
    }

    /// Formats this declaration into canonical `package.manifest` compatibility lines.
    #[must_use]
    pub fn to_manifest_lines(&self) -> String {
        let mut lines = vec![
            format!(
                "compatibility.target-sabi-epoch = {}",
                self.target_sabi_epoch
            ),
            format!(
                "compatibility.target-sabi-minor = {}",
                self.target_sabi_minor
            ),
        ];
        if let Some(driver_epoch) = self.driver_abi_epoch {
            lines.push(format!("compatibility.driver-abi-epoch = {driver_epoch}"));
        }
        if !self.required_features.is_empty() {
            lines.push(format!(
                "compatibility.required-features = {}",
                self.required_features.join(",")
            ));
        }
        if !self.critical_extensions.is_empty() {
            lines.push(format!(
                "compatibility.critical-extensions = {}",
                join_u32_csv(&self.critical_extensions)
            ));
        }
        if !self.non_critical_extensions.is_empty() {
            lines.push(format!(
                "compatibility.non-critical-extensions = {}",
                join_u32_csv(&self.non_critical_extensions)
            ));
        }
        lines.join("\n")
    }
}

/// Fail-closed parsing and structural validation errors for `PackageCompatibilitySpec`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManifestCompatibilityError {
    /// Input manifest text exceeded [`MAX_COMPAT_MANIFEST_BYTES`].
    ManifestTooLarge { actual: usize, maximum: usize },
    /// A line was neither a blank/comment line nor a `key = value` assignment.
    MalformedAssignment { line: usize },
    /// A `compatibility.*` key was not recognized.
    UnknownCompatibilityKey { line: usize, key: String },
    /// The same compatibility key appeared more than once.
    DuplicateCompatibilityKey { line: usize, key: String },
    /// Integer field failed `u32` parsing.
    InvalidInteger {
        line: usize,
        key: String,
        value: String,
    },
    /// Extension list exceeded [`MAX_EXTENSION_IDS`].
    TooManyExtensions {
        kind: &'static str,
        actual: usize,
        maximum: usize,
    },
    /// `required_features` exceeded [`MAX_REQUIRED_FEATURES`].
    TooManyRequiredFeatures { actual: usize, maximum: usize },
    /// A feature identifier was empty, oversized, or contained control characters.
    InvalidFeatureName(String),
}

impl fmt::Display for ManifestCompatibilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ManifestTooLarge { actual, maximum } => write!(
                formatter,
                "package manifest has {actual} bytes; maximum is {maximum}"
            ),
            Self::MalformedAssignment { line } => {
                write!(formatter, "malformed manifest assignment at line {line}")
            }
            Self::UnknownCompatibilityKey { line, key } => write!(
                formatter,
                "unknown compatibility key {key:?} at line {line}"
            ),
            Self::DuplicateCompatibilityKey { line, key } => write!(
                formatter,
                "duplicate compatibility key {key:?} at line {line}"
            ),
            Self::InvalidInteger { line, key, value } => write!(
                formatter,
                "invalid u32 value {value:?} for key {key:?} at line {line}"
            ),
            Self::TooManyExtensions {
                kind,
                actual,
                maximum,
            } => write!(
                formatter,
                "{kind} has {actual} entries; maximum is {maximum}"
            ),
            Self::TooManyRequiredFeatures { actual, maximum } => write!(
                formatter,
                "required_features has {actual} entries; maximum is {maximum}"
            ),
            Self::InvalidFeatureName(feature) => {
                write!(formatter, "invalid feature token {feature:?}")
            }
        }
    }
}

impl Error for ManifestCompatibilityError {}

/// Parses `PackageCompatibilitySpec` from a line-based `package.manifest` document.
///
/// Unrelated package directives (`package-id`, `version`, `entry`, `task`) are
/// ignored, while any `compatibility.*` (or top-level compatibility alias) line is
/// strictly validated fail-closed. Packages that omit explicit compatibility lines
/// default to SABI `1.0` (`target_sabi_epoch = 1`, `target_sabi_minor = 0`).
///
/// # Errors
///
/// Returns [`ManifestCompatibilityError`] on oversized manifests, malformed lines,
/// unknown `compatibility.*` keys, duplicate compatibility keys, invalid integers,
/// or out-of-bounds extension/feature lists.
pub fn parse_package_compatibility_manifest(
    manifest_text: &str,
) -> Result<PackageCompatibilitySpec, ManifestCompatibilityError> {
    if manifest_text.len() > MAX_COMPAT_MANIFEST_BYTES {
        return Err(ManifestCompatibilityError::ManifestTooLarge {
            actual: manifest_text.len(),
            maximum: MAX_COMPAT_MANIFEST_BYTES,
        });
    }

    let mut spec = PackageCompatibilitySpec::default();
    let mut seen_slots = [false; 6];

    for (index, raw_line) in manifest_text.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((raw_key, raw_val)) = trimmed.split_once('=') else {
            return Err(ManifestCompatibilityError::MalformedAssignment { line: line_no });
        };
        let key = raw_key.trim();
        let value = raw_val.trim();
        apply_compatibility_kv(&mut spec, &mut seen_slots, line_no, key, value)?;
    }

    spec.validate_bounds()?;
    Ok(spec)
}

fn apply_compatibility_kv(
    spec: &mut PackageCompatibilitySpec,
    seen_slots: &mut [bool; 6],
    line_no: usize,
    key: &str,
    value: &str,
) -> Result<(), ManifestCompatibilityError> {
    let slot = match key {
        "compatibility.target-sabi-epoch"
        | "compatibility.target_sabi_epoch"
        | "target-sabi-epoch"
        | "target_sabi_epoch" => 0,
        "compatibility.target-sabi-minor"
        | "compatibility.target_sabi_minor"
        | "target-sabi-minor"
        | "target_sabi_minor" => 1,
        "compatibility.driver-abi-epoch"
        | "compatibility.driver_abi_epoch"
        | "driver-abi-epoch"
        | "driver_abi_epoch" => 2,
        "compatibility.required-features"
        | "compatibility.required_features"
        | "required-features"
        | "required_features" => 3,
        "compatibility.critical-extensions"
        | "compatibility.critical_extensions"
        | "critical-extensions"
        | "critical_extensions" => 4,
        "compatibility.non-critical-extensions"
        | "compatibility.non_critical_extensions"
        | "non-critical-extensions"
        | "non_critical_extensions" => 5,
        other if other.starts_with("compatibility.") => {
            return Err(ManifestCompatibilityError::UnknownCompatibilityKey {
                line: line_no,
                key: other.to_owned(),
            });
        }
        _ => return Ok(()),
    };

    if seen_slots[slot] {
        return Err(ManifestCompatibilityError::DuplicateCompatibilityKey {
            line: line_no,
            key: key.to_owned(),
        });
    }
    seen_slots[slot] = true;

    match slot {
        0 => spec.target_sabi_epoch = parse_u32_field(line_no, key, value)?,
        1 => spec.target_sabi_minor = parse_u32_field(line_no, key, value)?,
        2 => {
            spec.driver_abi_epoch = if value.eq_ignore_ascii_case("none") || value.is_empty() {
                None
            } else {
                Some(parse_u32_field(line_no, key, value)?)
            };
        }
        3 => spec.required_features = parse_feature_csv(value)?,
        4 => spec.critical_extensions = parse_u32_csv(line_no, key, value)?,
        5 => spec.non_critical_extensions = parse_u32_csv(line_no, key, value)?,
        _ => unreachable!(),
    }
    Ok(())
}

fn parse_u32_field(
    line_no: usize,
    key: &str,
    value: &str,
) -> Result<u32, ManifestCompatibilityError> {
    value
        .parse::<u32>()
        .map_err(|_| ManifestCompatibilityError::InvalidInteger {
            line: line_no,
            key: key.to_owned(),
            value: value.to_owned(),
        })
}

fn parse_u32_csv(
    line_no: usize,
    key: &str,
    value: &str,
) -> Result<Vec<u32>, ManifestCompatibilityError> {
    if value.is_empty() || value.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|part| parse_u32_field(line_no, key, part.trim()))
        .collect()
}

fn parse_feature_csv(value: &str) -> Result<Vec<String>, ManifestCompatibilityError> {
    if value.is_empty() || value.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    let mut features = Vec::new();
    for raw in value.split(',') {
        let token = raw.trim();
        validate_feature_token(token)?;
        features.push(token.to_owned());
    }
    Ok(features)
}

fn validate_feature_token(feature: &str) -> Result<(), ManifestCompatibilityError> {
    if feature.is_empty()
        || feature.len() > MAX_FEATURE_NAME_BYTES
        || feature.chars().any(char::is_control)
        || feature.chars().any(char::is_whitespace)
    {
        return Err(ManifestCompatibilityError::InvalidFeatureName(
            feature.to_owned(),
        ));
    }
    Ok(())
}

fn join_u32_csv(values: &[u32]) -> String {
    values
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_roundtrips_package_manifest_compatibility_declarations() {
        let doc = r"
            # package metadata
            package-id = com.example.legacy-agent
            version = 1.4.2
            compatibility.target-sabi-epoch = 1
            compatibility.target-sabi-minor = 7
            compatibility.driver-abi-epoch = 1
            compatibility.required-features = sabi.wait.v1,sabi.takeover.v1
            compatibility.critical-extensions = 101, 205
            compatibility.non-critical-extensions = 42, 7001
        ";
        let spec = parse_package_compatibility_manifest(doc).expect("parse manifest");
        assert_eq!(spec.target_sabi_epoch, 1);
        assert_eq!(spec.target_sabi_minor, 7);
        assert_eq!(spec.driver_abi_epoch, Some(1));
        assert_eq!(
            spec.required_features,
            vec!["sabi.wait.v1".to_owned(), "sabi.takeover.v1".to_owned()]
        );
        assert_eq!(spec.critical_extensions, vec![101, 205]);
        assert_eq!(spec.non_critical_extensions, vec![42, 7001]);

        let rendered = spec.to_manifest_lines();
        let reparsed = parse_package_compatibility_manifest(&rendered).expect("reparse rendered");
        assert_eq!(reparsed, spec);
    }

    #[test]
    fn manifest_parser_fails_closed_on_unknown_duplicate_or_malformed_lines() {
        let unknown = "compatibility.magic-bypass = true";
        assert!(matches!(
            parse_package_compatibility_manifest(unknown),
            Err(ManifestCompatibilityError::UnknownCompatibilityKey { .. })
        ));

        let duplicate = "compatibility.target-sabi-epoch = 1\ntarget_sabi_epoch = 2";
        assert!(matches!(
            parse_package_compatibility_manifest(duplicate),
            Err(ManifestCompatibilityError::DuplicateCompatibilityKey { .. })
        ));

        let malformed = "package-id = ok\nnot_an_assignment_line";
        assert!(matches!(
            parse_package_compatibility_manifest(malformed),
            Err(ManifestCompatibilityError::MalformedAssignment { line: 2 })
        ));
    }
}
