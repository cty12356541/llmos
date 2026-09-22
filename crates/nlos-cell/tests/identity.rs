//! Type-level Cell identity: two identities exist, and neither encodes
//! OS-process location (`MODEL-ID-002`, `DIST-NAME-001`).

use nlos_cell::CellIdentity;
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
fn stable_identity_bytes_do_not_embed_os_process_id() {
    let domain = SchedulerDomainId::from_bytes([
        0xab, 0xcd, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
        0xcc,
    ]);
    let identity = CellIdentity::from_domain(domain);
    let pid = std::process::id().to_le_bytes();

    assert_eq!(identity.as_bytes(), domain.as_bytes());
    assert!(
        !identity
            .as_bytes()
            .windows(pid.len())
            .any(|window| window == pid),
        "stable Cell identity must not embed the OS pid {pid:?}"
    );
}
