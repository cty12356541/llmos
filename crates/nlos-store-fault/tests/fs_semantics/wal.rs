//! WAL file byte-level parsing and surgery helpers.
//!
//! The fault VFS models torn writes at `xWrite`-call granularity. This module
//! parses the real on-disk WAL format (see `SQLite` documentation, "The WAL
//! file format": 32-byte header, then frames of 24-byte frame header +
//! page-size payload, all integers big-endian) so tests can apply byte-exact
//! damage aligned with the model's tear points.

/// Parsed WAL structure: header fields plus one entry per fully present
/// frame.
pub(crate) struct WalLayout {
    /// Page size from the WAL header (big-endian at offset 8).
    pub(crate) page_size: u32,
    /// Byte length of one frame (24-byte header + payload).
    pub(crate) frame_size: usize,
    /// Byte offsets of every commit frame (frame with nonzero db-size field
    /// at bytes 4..8 of its header).
    pub(crate) commit_ends: Vec<usize>,
    /// Total WAL byte length.
    pub(crate) len: usize,
}

pub(crate) const WAL_HEADER_LEN: usize = 32;
pub(crate) const FRAME_HEADER_LEN: usize = 24;

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

/// Parses `wal` bytes; frames past the end (torn) are not reported.
pub(crate) fn parse(wal: &[u8]) -> WalLayout {
    assert!(
        wal.len() >= WAL_HEADER_LEN,
        "WAL shorter than its 32-byte header: {}",
        wal.len()
    );
    let page_size = be_u32(&wal[8..12]);
    assert!(
        (512..=65_536).contains(&page_size) && page_size.is_power_of_two(),
        "implausible WAL page size {page_size}"
    );
    let frame_size = FRAME_HEADER_LEN + page_size as usize;
    let mut commit_ends = Vec::new();
    let mut offset = WAL_HEADER_LEN;
    while offset + frame_size <= wal.len() {
        let frame = &wal[offset..offset + frame_size];
        if be_u32(&frame[4..8]) != 0 {
            commit_ends.push(offset + frame_size);
        }
        offset += frame_size;
    }
    WalLayout {
        page_size,
        frame_size,
        commit_ends,
        len: wal.len(),
    }
}

/// Number of commits whose commit frame ends at or before `truncate_to`
/// bytes — i.e. the committed prefix `SQLite` must recover from a WAL file
/// truncated to that length.
pub(crate) fn committed_prefix_count(layout: &WalLayout, truncate_to: usize) -> usize {
    layout
        .commit_ends
        .iter()
        .filter(|end| **end <= truncate_to)
        .count()
}
