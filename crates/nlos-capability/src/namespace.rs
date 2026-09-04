//! Zero-padded prefix encoding for mechanically verifiable Namespace hierarchy.
//!
//! A [`NamespaceId`] path occupies a leading byte sequence; trailing zero bytes
//! are padding. Namespace `child` is within namespace `ancestor` when the active
//! prefix of `child` equals that of `ancestor` (equal or narrower scope).

use nlos_types::NamespaceId;

use crate::model::CapabilityTarget;

/// Active path length: index after the last non-zero byte (0 for the root).
#[must_use]
pub(crate) const fn namespace_prefix_len(bytes: &[u8; 16]) -> usize {
    let mut len = 16usize;
    while len > 0 && bytes[len - 1] == 0 {
        len -= 1;
    }
    len
}

/// True when `child` names the same namespace or a descendant of `ancestor`.
#[must_use]
pub(crate) fn namespace_is_within(child: NamespaceId, ancestor: NamespaceId) -> bool {
    let child_bytes = child.as_bytes();
    let ancestor_bytes = ancestor.as_bytes();
    let prefix_len = namespace_prefix_len(ancestor_bytes);
    child_bytes[..prefix_len] == ancestor_bytes[..prefix_len]
}

/// Admission scope: `requested` must equal `granted` for tasks, or sit in the
/// namespace subtree rooted at `granted`.
#[must_use]
pub(crate) fn target_is_within(requested: CapabilityTarget, granted: CapabilityTarget) -> bool {
    match (requested, granted) {
        (CapabilityTarget::Task(requested_id), CapabilityTarget::Task(granted_id)) => {
            requested_id == granted_id
        }
        (CapabilityTarget::Namespace(requested_id), CapabilityTarget::Namespace(granted_id)) => {
            namespace_is_within(requested_id, granted_id)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ns(first: u8, second: u8) -> NamespaceId {
        NamespaceId::from_bytes([first, second, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
    }

    #[test]
    fn prefix_len_trims_trailing_zeros() {
        assert_eq!(namespace_prefix_len(&[0; 16]), 0);
        assert_eq!(
            namespace_prefix_len(&[1, 2, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            3
        );
        assert_eq!(namespace_prefix_len(&[0xff; 16]), 16);
    }

    #[test]
    fn namespace_within_allows_equal_and_descendant() {
        let parent = ns(0x44, 0x00);
        assert!(namespace_is_within(parent, parent));
        assert!(namespace_is_within(ns(0x44, 0x55), parent));
        assert!(!namespace_is_within(ns(0x45, 0x00), parent));
        assert!(namespace_is_within(
            ns(0x44, 0x00),
            NamespaceId::from_bytes([0; 16])
        ));
    }
}
