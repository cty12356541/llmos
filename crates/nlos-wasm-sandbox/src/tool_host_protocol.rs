use std::io::{self, Read, Write};
use std::path::Path;

use cap_fs_ext::{FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::fs::{File, OpenOptions};
use sha2::{Digest, Sha256};

use crate::{error::HostReport, wasi_allowlist::open_root};

pub(crate) const MAX_STDOUT: usize = 256 * 1024;
pub(crate) const MAX_GUEST_STDERR: usize = 60 * 1024;
const MAX_REPORT: usize = 4 * 1024;
pub(crate) const MAX_FILE_WRITE: u64 = 8 * 1024 * 1024;
pub(crate) const MAX_WASM_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_WALL_MS: u64 = 30_000;
const REPORT_FRAME: u32 = 1 << 31;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolHostLimits {
    pub stdout: usize,
    pub stderr: usize,
    pub file_write: u64,
}

impl ToolHostLimits {
    pub fn valid(self) -> bool {
        (1..=MAX_STDOUT).contains(&self.stdout)
            && (1..=MAX_GUEST_STDERR).contains(&self.stderr)
            && self.file_write <= MAX_FILE_WRITE
    }
}

fn open_artifact(path: &Path) -> io::Result<File> {
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let dir = open_root(parent)?;
    let entry = dir.symlink_metadata(name)?;
    if !entry.is_file() || entry.len() > MAX_WASM_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let mut options = OpenOptions::new();
    let file = dir.open_with(name, options.read(true).follow(FollowSymlinks::No))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_WASM_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(file)
}

pub(crate) fn read_artifact(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open_artifact(path)?
        .take(MAX_WASM_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_WASM_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}

pub(crate) fn artifact_digest(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = open_artifact(path)?.take(MAX_WASM_BYTES + 1);
    let mut digest = Sha256::new();
    let mut buffer = [0; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    if file.limit() == 0 {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(digest.finalize().into())
}

pub(crate) fn digest_hex(digest: [u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in digest {
        write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

pub(crate) fn parse_digest(hex: &str) -> Option<[u8; 32]> {
    let mut digest = [0; 32];
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    for (index, byte) in digest.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(digest)
}

// The trusted host wraps guest diagnostic bytes in data frames. Only host
// code emits a report frame, so a guest controls content but never frame tags.
pub(crate) fn write_stderr_frame(
    mut stderr: impl Write,
    bytes: &[u8],
    report: bool,
) -> io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    let length = u32::try_from(bytes.len()).map_err(|_| io::ErrorKind::InvalidData)?;
    let header = if report {
        length | REPORT_FRAME
    } else {
        length
    };
    stderr.write_all(&header.to_le_bytes())?;
    stderr.write_all(bytes)?;
    stderr.flush()
}

#[derive(Debug)]
pub(crate) enum FrameError {
    Limit,
    Invalid,
}

pub(crate) fn read_stderr_frames(
    mut stderr: impl Read,
    guest_limit: usize,
) -> Result<Option<HostReport>, FrameError> {
    let mut report = None;
    let mut total = 0;
    let mut discard = [0; 8192];
    loop {
        let mut header = [0; 4];
        if stderr
            .read(&mut header[..1])
            .map_err(|_| FrameError::Invalid)?
            == 0
        {
            break;
        }
        stderr
            .read_exact(&mut header[1..])
            .map_err(|_| FrameError::Invalid)?;
        let header = u32::from_le_bytes(header);
        let length = usize::try_from(header & !REPORT_FRAME).map_err(|_| FrameError::Invalid)?;
        if report.is_some() || length == 0 {
            return Err(FrameError::Invalid);
        }
        if header & REPORT_FRAME != 0 {
            if length > MAX_REPORT {
                return Err(FrameError::Limit);
            }
            let mut data = vec![0; length];
            stderr
                .read_exact(&mut data)
                .map_err(|_| FrameError::Invalid)?;
            let line = std::str::from_utf8(&data).map_err(|_| FrameError::Invalid)?;
            report = Some(HostReport::parse(line).ok_or(FrameError::Invalid)?);
        } else {
            total += length;
            if total > guest_limit {
                return Err(FrameError::Limit);
            }
            let mut remaining = length;
            while remaining != 0 {
                let chunk = remaining.min(discard.len());
                stderr
                    .read_exact(&mut discard[..chunk])
                    .map_err(|_| FrameError::Invalid)?;
                remaining -= chunk;
            }
        }
    }
    Ok(report)
}
