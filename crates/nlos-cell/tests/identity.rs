//! Type-level Cell identity: two identities exist, and neither encodes
//! OS-process location (`MODEL-ID-002`, `DIST-NAME-001`).

use nlos_cell::{CellAuthority, CellIdentity};
use nlos_types::SchedulerDomainId;

#[test]
fn two_cell_identities_are_distinct_stable_domains() {
    let alpha = CellIdentity::from_domain(SchedulerDomainId::from_bytes([0x11; 16]));
    let beta = CellIdentity::from_domain(SchedulerDomainId::from_bytes([0x22; 16]));

    assert_ne!(alpha, beta);
    assert_eq!(alpha.as_bytes(), &[0x11; 16]);
    assert_eq!(beta.as_bytes(), &[0x22; 16]);
    assert_eq!(alpha.domain(), SchedulerDomainId::from_bytes([0x11; 16]));
}

#[test]
fn claimed_identity_equals_domain_independent_of_os_process_id() {
    let domain = SchedulerDomainId::from_bytes([
        0xab, 0xcd, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
        0xcc,
    ]);
    let authority = CellAuthority::claim(domain).expect("claim");

    // MODEL-ID-002: stable identity is exactly the claimed domain.
    assert_eq!(authority.identity().as_bytes(), domain.as_bytes());
    // os_process_id is observational only — recorded separately, not fused into identity.
    assert_eq!(authority.os_process_id(), std::process::id());
}
