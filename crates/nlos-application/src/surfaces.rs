//! The manifest-additive `surfaces` declaration segment and its durable
//! registration face (W32-F / B2-2, the UI-Surface dimension of
//! ROAD-B-002).
//!
//! Following the W28-B `tasks`-template pattern, the segment is
//! *declaration data only*: it answers **what surfaces an application
//! offers** (identity, kind, title-ish metadata, an optional content
//! entry reference) and **which installed package content the
//! declaration belongs to** (the manifest digest of the installation it
//! is registered against). It is deliberately not a windowing/layout
//! framework: no geometry, no focus/input routing, no surface lifecycle
//! management — presentation of the declared facts is the desktop
//! shell's business (`[DUI-*]` contracts stay future lanes), and this
//! module owns only the declared shape and its typed admission rules.
//!
//! Because the signed package-file format lives in `nlos-artifact`
//! (outside this crate), the segment binds to the installed application
//! through the application authority itself: [`ApplicationAuthority::
//! register_surfaces`] admits one declared segment per idempotent call,
//! pinned to the application's *current* installation generation and
//! manifest digest (a content binding mirroring the installation's
//! digest-binding discipline — a declaration naming stale content is a
//! typed refusal, never a silent override), and [`ApplicationAuthority::
//! inspect_surfaces`] reads the durable registrations back.

use std::error::Error;
use std::fmt;

/// Admission bound for one declared segment (mirrors the artifact
/// authority's task-template admission bound: a bound exists so a
/// malformed manifest cannot mint unbounded rows; it is not a product
/// limit).
pub const MAX_SURFACES_PER_SEGMENT: usize = 100_000;

/// Title/entry-name bound in bytes (the package-file entry-name bound is
/// the precedent: ≤255 bytes, non-empty, no NUL).
pub const MAX_SURFACE_TEXT_BYTES: usize = 255;

/// What a declared surface presents as. The two-value surface and its
/// one-byte durable encoding mirror `PackageTaskKind`'s shape: a
/// minimal, honest set covering the desktop shell's minimal
/// presentation (a window-like panel vs. an inline panel), nothing more
/// — kinds beyond these are additive schema work, not remaps.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[repr(u8)]
pub enum PackageSurfaceKind {
    /// A window-like surface: the desktop presents it as a standalone
    /// window-shaped panel.
    Window = 1,
    /// An inline panel surface: the desktop presents it as an embedded
    /// panel inside a host view.
    Panel = 2,
}

impl PackageSurfaceKind {
    #[must_use]
    pub const fn encode(self) -> u8 {
        self as u8
    }

    pub(crate) const fn decode(value: i64) -> Result<Self, SurfaceSegmentError> {
        match value {
            1 => Ok(Self::Window),
            2 => Ok(Self::Panel),
            _ => Err(SurfaceSegmentError::CorruptKind),
        }
    }
}

/// One declared surface of a package's `surfaces` segment: the minimal
/// declaration a desktop needs to present something honest — a stable
/// segment-local identity, the presentation kind, human-facing title
/// metadata, and an optional content reference naming a package
/// manifest entry whose payload is the surface's declared content.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageSurfaceDeclaration {
    /// Declaration-local stable identity, unique within the segment
    /// (the `node_key` precedent: 16 declared bytes, not an
    /// authority-derived id).
    pub surface_id: [u8; 16],
    pub kind: PackageSurfaceKind,
    /// Human-facing title metadata. Non-empty, ≤255 bytes, no NUL.
    pub title: String,
    /// Optional content reference: the package manifest entry name whose
    /// payload the surface declares as its content. `None` means the
    /// surface declares metadata only (the desktop renders a content
    /// placeholder, never invented content).
    pub entry_name: Option<String>,
}

/// Fail-closed typed errors of the `surfaces` segment shape contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SurfaceSegmentError {
    /// A segment must declare at least one surface.
    Empty,
    /// The segment exceeds the admission bound.
    TooManySurfaces,
    /// Two declarations share one surface identity.
    DuplicateSurfaceId,
    /// A title violates the text bound (empty, >255 bytes, or NUL).
    InvalidTitle,
    /// An entry name violates the text bound (empty, >255 bytes, or
    /// NUL).
    InvalidEntryName,
    /// A stored kind byte is not a declared kind (durable decode only).
    CorruptKind,
}

impl fmt::Display for SurfaceSegmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("surface segment must declare at least one surface"),
            Self::TooManySurfaces => {
                formatter.write_str("surface segment exceeds the admission bound")
            }
            Self::DuplicateSurfaceId => {
                formatter.write_str("duplicate surface id in the declared segment")
            }
            Self::InvalidTitle => {
                formatter.write_str("surface title must be 1..=255 bytes with no NUL")
            }
            Self::InvalidEntryName => {
                formatter.write_str("surface entry name must be 1..=255 bytes with no NUL")
            }
            Self::CorruptKind => formatter.write_str("unknown surface kind encoding"),
        }
    }
}

impl Error for SurfaceSegmentError {}

/// Validates the `surfaces` segment shape: non-empty, within the
/// admission bound, unique surface identities, and every text field
/// within the package-file text bounds. This is the one shape authority
/// shared by [`crate::ApplicationAuthority::register_surfaces`] and any
/// caller-side declaration builder, so the two faces cannot drift apart
/// (the `validate_task_templates` precedent).
///
/// # Errors
///
/// Returns the first violated [`SurfaceSegmentError`] fail-closed.
pub fn validate_surface_declarations(
    surfaces: &[PackageSurfaceDeclaration],
) -> Result<(), SurfaceSegmentError> {
    if surfaces.is_empty() {
        return Err(SurfaceSegmentError::Empty);
    }
    if surfaces.len() > MAX_SURFACES_PER_SEGMENT {
        return Err(SurfaceSegmentError::TooManySurfaces);
    }
    let valid_text = |text: &str| !text.is_empty() && text.len() <= MAX_SURFACE_TEXT_BYTES;
    let mut seen = std::collections::HashSet::with_capacity(surfaces.len());
    for surface in surfaces {
        if !seen.insert(surface.surface_id) {
            return Err(SurfaceSegmentError::DuplicateSurfaceId);
        }
        if !valid_text(&surface.title) || surface.title.contains('\0') {
            return Err(SurfaceSegmentError::InvalidTitle);
        }
        if let Some(entry_name) = &surface.entry_name
            && (!valid_text(entry_name) || entry_name.contains('\0'))
        {
            return Err(SurfaceSegmentError::InvalidEntryName);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_SURFACES_PER_SEGMENT, PackageSurfaceDeclaration, PackageSurfaceKind,
        SurfaceSegmentError, validate_surface_declarations,
    };

    fn surface(id: [u8; 16], title: &str) -> PackageSurfaceDeclaration {
        PackageSurfaceDeclaration {
            surface_id: id,
            kind: PackageSurfaceKind::Window,
            title: title.to_string(),
            entry_name: None,
        }
    }

    #[test]
    fn segment_validation_admits_minimal_and_bounded_segments() {
        let segment = [
            surface([0x01; 16], "主窗口"),
            PackageSurfaceDeclaration {
                surface_id: [0x02; 16],
                kind: PackageSurfaceKind::Panel,
                title: "侧栏".to_string(),
                entry_name: Some("assets/panel.bin".to_string()),
            },
        ];
        validate_surface_declarations(&segment).expect("minimal segment must validate");
    }

    #[test]
    fn segment_validation_refuses_each_typed_violation() {
        assert_eq!(
            validate_surface_declarations(&[]),
            Err(SurfaceSegmentError::Empty)
        );
        assert_eq!(
            validate_surface_declarations(&[surface([0x01; 16], ""),]),
            Err(SurfaceSegmentError::InvalidTitle)
        );
        assert_eq!(
            validate_surface_declarations(&[surface([0x01; 16], "\0"),]),
            Err(SurfaceSegmentError::InvalidTitle)
        );
        let too_long = "x".repeat(256);
        assert_eq!(
            validate_surface_declarations(&[surface([0x01; 16], &too_long)]),
            Err(SurfaceSegmentError::InvalidTitle)
        );
        let mut bad_entry = surface([0x01; 16], "ok");
        bad_entry.entry_name = Some(String::new());
        assert_eq!(
            validate_surface_declarations(&[bad_entry]),
            Err(SurfaceSegmentError::InvalidEntryName)
        );
        assert_eq!(
            validate_surface_declarations(&[surface([0x07; 16], "a"), surface([0x07; 16], "b"),]),
            Err(SurfaceSegmentError::DuplicateSurfaceId)
        );
        let oversized: Vec<PackageSurfaceDeclaration> = (0..=MAX_SURFACES_PER_SEGMENT)
            .map(|index| {
                let mut id = [0_u8; 16];
                id[..8].copy_from_slice(&(index as u64).to_be_bytes());
                surface(id, "t")
            })
            .collect();
        assert_eq!(
            validate_surface_declarations(&oversized),
            Err(SurfaceSegmentError::TooManySurfaces)
        );
    }

    #[test]
    fn kind_encoding_round_trips_both_variants_only() {
        assert_eq!(PackageSurfaceKind::Window.encode(), 1);
        assert_eq!(PackageSurfaceKind::Panel.encode(), 2);
        assert_eq!(
            PackageSurfaceKind::decode(1).expect("window"),
            PackageSurfaceKind::Window
        );
        assert_eq!(
            PackageSurfaceKind::decode(2).expect("panel"),
            PackageSurfaceKind::Panel
        );
        assert_eq!(
            PackageSurfaceKind::decode(3),
            Err(SurfaceSegmentError::CorruptKind)
        );
    }
}
