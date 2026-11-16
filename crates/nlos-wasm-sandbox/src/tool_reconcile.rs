use std::io::Read;

use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use sha2::{Digest, Sha256};

use crate::{PreopenGrant, wasi_allowlist::open_root};

pub(crate) const MAX_ENTRIES: usize = 4096;
pub(crate) const MAX_SCAN_BYTES: u64 = 128 * 1024 * 1024;

pub(crate) fn observe(
    grants: &[PreopenGrant],
    max_entries: usize,
    max_bytes: u64,
) -> Option<[u8; 32]> {
    let mut scan = Scan {
        hash: Sha256::new(),
        entries_remaining: max_entries,
        bytes_remaining: max_bytes,
    };
    scan.hash.update(b"nlos/tool-workspace-observation/v1\0");
    for grant in grants.iter().filter(|grant| grant.allow_write()) {
        let root = open_root(grant.host_root()).ok()?;
        scan.field(grant.guest_dir().as_bytes());
        scan.directory(&root, 0)?;
    }
    Some(scan.hash.finalize().into())
}

struct Scan {
    hash: Sha256,
    entries_remaining: usize,
    bytes_remaining: u64,
}

impl Scan {
    fn field(&mut self, bytes: &[u8]) {
        self.hash.update((bytes.len() as u64).to_le_bytes());
        self.hash.update(bytes);
    }

    fn directory(&mut self, dir: &Dir, depth: usize) -> Option<()> {
        if depth >= 64 {
            return None;
        }
        let mut names = Vec::new();
        for entry in dir.entries().ok()? {
            self.entries_remaining = self.entries_remaining.checked_sub(1)?;
            let name = entry.ok()?.file_name().into_string().ok()?;
            names.push(name);
        }
        names.sort_unstable();
        for name in names {
            let metadata = dir.symlink_metadata(&name).ok()?;
            self.field(name.as_bytes());
            if metadata.is_dir() && !metadata.is_symlink() {
                self.hash.update([1]);
                self.directory(&dir.open_dir_nofollow(&name).ok()?, depth + 1)?;
            } else if metadata.is_file() {
                self.hash.update([2]);
                self.file(dir, &name)?;
            } else {
                return None;
            }
        }
        self.hash.update([0]);
        Some(())
    }

    fn file(&mut self, dir: &Dir, name: &str) -> Option<()> {
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut file = dir.open_with(name, &options).ok()?;
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > self.bytes_remaining {
            return None;
        }
        let expected = metadata.len();
        let mut hash = Sha256::new();
        let mut buf = [0; 8192];
        let mut size = 0;
        loop {
            let allowed = usize::try_from(self.bytes_remaining.min(buf.len() as u64)).ok()?;
            if allowed == 0 {
                if size != expected {
                    return None;
                }
                break;
            }
            let count = file.read(&mut buf[..allowed]).ok()?;
            if count == 0 {
                break;
            }
            size += count as u64;
            self.bytes_remaining -= count as u64;
            hash.update(&buf[..count]);
        }
        if size != expected || file.metadata().ok()?.len() != expected {
            return None;
        }
        self.hash.update(size.to_le_bytes());
        self.hash.update(hash.finalize());
        Some(())
    }
}
