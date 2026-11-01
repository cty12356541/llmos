//! `DRV-PROTO-001` versioned-extension envelope: MCP / A2A / OpenAI-compatible
//! protocol bridges are wrapped as *versioned* driver extensions, negotiated
//! against a fixed supported-version set before any call is dispatched.

use std::error::Error;
use std::fmt;

/// Stable identity of one versioned protocol extension exposed by a driver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverExtensionDescriptor {
    /// Reverse-DNS style extension identifier
    /// (e.g. `llmos.ext.driver.mcp-bridge`).
    pub extension_id: String,
    /// Extension surface version `(major, minor)` — additive-only evolution.
    pub extension_version: (u16, u16),
    /// Wrapped wire protocol identifier (`mcp`, `a2a`, `openai-chat-completions`).
    pub protocol_id: String,
    /// Wire protocol versions this extension build can speak. Order is the
    /// adapter's preference order (first entry is the preferred proposal).
    pub supported_protocol_versions: Vec<String>,
    /// Frozen driver-provider codec channel this adapter rides on
    /// (`nlos.sabi.DriverProvider` v1.0 per ADR-0024).
    pub codec_freeze_tag: String,
}

impl DriverExtensionDescriptor {
    /// Builds a descriptor from stringly parts.
    #[must_use]
    pub fn new(
        extension_id: &str,
        extension_version: (u16, u16),
        protocol_id: &str,
        supported_protocol_versions: &[&str],
        codec_freeze_tag: &str,
    ) -> Self {
        Self {
            extension_id: extension_id.to_owned(),
            extension_version,
            protocol_id: protocol_id.to_owned(),
            supported_protocol_versions: supported_protocol_versions
                .iter()
                .map(|item| (*item).to_owned())
                .collect(),
            codec_freeze_tag: codec_freeze_tag.to_owned(),
        }
    }

    /// Renders `extension_id@major.minor` for diagnostics and seal records.
    #[must_use]
    pub fn qualified_id(&self) -> String {
        format!(
            "{}@{}.{}",
            self.extension_id, self.extension_version.0, self.extension_version.1
        )
    }
}

/// A successfully negotiated extension binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegotiatedExtension {
    /// The descriptor the negotiation ran against.
    pub descriptor: DriverExtensionDescriptor,
    /// The wire protocol version both sides agreed on.
    pub chosen_protocol_version: String,
}

/// Fail-closed negotiation errors (`DRV-NEGO-001`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ExtensionNegoError {
    /// The peer offered no version this extension build supports.
    UnsupportedProtocol {
        /// Extension id that was negotiating.
        extension_id: String,
        /// Versions the peer offered.
        offered: Vec<String>,
        /// Versions this build supports.
        supported: Vec<String>,
    },
    /// The descriptor or manifest is structurally unusable (empty supported
    /// set, malformed line, or missing mandatory field).
    MalformedDescriptor {
        /// Human-readable reason.
        reason: String,
    },
}

impl fmt::Display for ExtensionNegoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedProtocol {
                extension_id,
                offered,
                supported,
            } => write!(
                f,
                "extension {extension_id}: peer protocol versions [{}] do not intersect supported [{}]",
                offered.join(", "),
                supported.join(", ")
            ),
            Self::MalformedDescriptor { reason } => {
                write!(f, "malformed extension manifest: {reason}")
            }
        }
    }
}

impl Error for ExtensionNegoError {}

/// Negotiates the wire protocol version for `descriptor` against the versions
/// the peer offered (`DRV-NEGO-001`).
///
/// The chosen version is the earliest peer-offered version that this build
/// supports (deterministic, independent of the adapter's preference order);
/// an empty intersection fails closed.
///
/// # Errors
/// Returns [`ExtensionNegoError::UnsupportedProtocol`] or
/// [`ExtensionNegoError::MalformedDescriptor`].
pub fn negotiate_protocol_version(
    descriptor: &DriverExtensionDescriptor,
    offered: &[&str],
) -> Result<NegotiatedExtension, ExtensionNegoError> {
    if descriptor.supported_protocol_versions.is_empty() {
        return Err(ExtensionNegoError::MalformedDescriptor {
            reason: format!(
                "{} has an empty supported version set",
                descriptor.extension_id
            ),
        });
    }
    for candidate in offered {
        if descriptor
            .supported_protocol_versions
            .iter()
            .any(|supported| supported == candidate)
        {
            return Ok(NegotiatedExtension {
                descriptor: descriptor.clone(),
                chosen_protocol_version: (*candidate).to_owned(),
            });
        }
    }
    Err(ExtensionNegoError::UnsupportedProtocol {
        extension_id: descriptor.extension_id.clone(),
        offered: offered.iter().map(|item| (*item).to_owned()).collect(),
        supported: descriptor.supported_protocol_versions.clone(),
    })
}

/// Parses a line-based versioned-extension manifest from a package data
/// payload (`key = value` lines, `#` comments and blank lines skipped).
///
/// Recognized keys: `extension-id`, `extension-version` (`major.minor`),
/// `protocol-id`, `protocol-versions` (comma list), `codec-freeze-tag`.
///
/// # Errors
/// Fails closed with [`ExtensionNegoError::MalformedDescriptor`] on unknown
/// keys, malformed versions, or missing mandatory fields.
pub fn parse_extension_manifest(
    text: &str,
) -> Result<DriverExtensionDescriptor, ExtensionNegoError> {
    let mut parsed_id = String::new();
    let mut parsed_version = String::new();
    let mut parsed_protocol = String::new();
    let mut parsed_versions: Vec<String> = Vec::new();
    let mut parsed_codec = String::new();

    for (idx, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(ExtensionNegoError::MalformedDescriptor {
                reason: format!("line {}: expected 'key = value', got {line:?}", idx + 1),
            });
        };
        let key = key.trim();
        let value = value.trim();
        if value.is_empty() {
            return Err(ExtensionNegoError::MalformedDescriptor {
                reason: format!("line {}: directive {key:?} has an empty value", idx + 1),
            });
        }
        match key {
            "extension-id" => value.clone_into(&mut parsed_id),
            "extension-version" => value.clone_into(&mut parsed_version),
            "protocol-id" => value.clone_into(&mut parsed_protocol),
            "protocol-versions" => {
                parsed_versions = value.split(',').map(str::trim).map(str::to_owned).collect();
            }
            "codec-freeze-tag" => value.clone_into(&mut parsed_codec),
            _ => {
                return Err(ExtensionNegoError::MalformedDescriptor {
                    reason: format!("line {}: unknown directive {key:?}", idx + 1),
                });
            }
        }
    }

    let malformed = |reason: String| ExtensionNegoError::MalformedDescriptor { reason };
    if parsed_id.is_empty() {
        return Err(malformed("missing extension-id".to_owned()));
    }
    if parsed_protocol.is_empty() {
        return Err(malformed("missing protocol-id".to_owned()));
    }
    if parsed_codec.is_empty() {
        return Err(malformed("missing codec-freeze-tag".to_owned()));
    }
    if parsed_versions.is_empty() {
        return Err(malformed("missing or empty protocol-versions".to_owned()));
    }
    let mut parts = parsed_version.split('.');
    let major = parts.next().and_then(|t| t.parse::<u16>().ok());
    let minor = parts.next().and_then(|t| t.parse::<u16>().ok());
    let Some((major, minor)) = major.zip(minor) else {
        return Err(malformed(format!(
            "extension-version must be major.minor, got {parsed_version:?}"
        )));
    };

    let descriptor = DriverExtensionDescriptor {
        extension_id: parsed_id,
        extension_version: (major, minor),
        protocol_id: parsed_protocol,
        supported_protocol_versions: parsed_versions,
        codec_freeze_tag: parsed_codec,
    };
    // A descriptor that cannot negotiate is rejected at parse time already.
    Ok(descriptor)
}

#[cfg(test)]
mod tests {
    use super::{
        DriverExtensionDescriptor, ExtensionNegoError, negotiate_protocol_version,
        parse_extension_manifest,
    };

    fn mcp_descriptor() -> DriverExtensionDescriptor {
        DriverExtensionDescriptor::new(
            "llmos.ext.driver.mcp-bridge",
            (1, 0),
            "mcp",
            &["2025-03-26", "2024-11-05"],
            "nlos.sabi.DriverProvider/v1.0",
        )
    }

    #[test]
    fn negotiation_prefers_peer_order_and_fails_closed() {
        let negotiated =
            negotiate_protocol_version(&mcp_descriptor(), &["2024-11-05", "1999-01-01"])
                .expect("intersecting version set");
        assert_eq!(negotiated.chosen_protocol_version, "2024-11-05");
        assert_eq!(
            negotiated.descriptor.qualified_id(),
            "llmos.ext.driver.mcp-bridge@1.0"
        );

        assert!(matches!(
            negotiate_protocol_version(&mcp_descriptor(), &["1999-01-01"]),
            Err(ExtensionNegoError::UnsupportedProtocol { .. })
        ));
        assert!(matches!(
            negotiate_protocol_version(&mcp_descriptor(), &[]),
            Err(ExtensionNegoError::UnsupportedProtocol { .. })
        ));

        let mut empty = mcp_descriptor();
        empty.supported_protocol_versions.clear();
        assert!(matches!(
            negotiate_protocol_version(&empty, &["2024-11-05"]),
            Err(ExtensionNegoError::MalformedDescriptor { .. })
        ));
    }

    #[test]
    fn extension_manifest_round_trips_and_fails_closed() {
        let text = "# versioned extension manifest\n\
             extension-id = llmos.ext.driver.mcp-bridge\n\
             extension-version = 1.0\n\
             protocol-id = mcp\n\
             protocol-versions = 2025-03-26, 2024-11-05\n\
             codec-freeze-tag = nlos.sabi.DriverProvider/v1.0\n";
        let parsed = parse_extension_manifest(text).expect("parse manifest");
        assert_eq!(parsed, mcp_descriptor());

        assert!(parse_extension_manifest("extension-id = only-one").is_err());
        assert!(parse_extension_manifest("nonsense").is_err());
        assert!(
            parse_extension_manifest("extension-id = x\nextension-version = 1\nprotocol-id = mcp\nprotocol-versions = v1\ncodec-freeze-tag = t\n")
                .is_err()
        );
        assert!(
            parse_extension_manifest("extension-id = x\nextension-version = 1.0\nprotocol-id = mcp\nprotocol-versions = v1\ncodec-freeze-tag = t\nunknown-key = 1\n")
                .is_err()
        );
    }
}
