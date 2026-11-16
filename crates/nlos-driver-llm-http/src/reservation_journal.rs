//! Length-framed, hash-chained append log for one gateway per authority directory.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use sha2::{Digest, Sha256};

use super::llm_gateway::GatewayError;

const NAME: &str = "llm-gateway.wal";
const MAGIC: &[u8; 8] = b"NLOSLGW1";
const MAX_RECORD_BYTES: usize = 128 * 1024;
const PREFIX_LEN: usize = 20;
const HEADER_LEN: usize = PREFIX_LEN + 32;
pub(super) const OVERHEAD: usize = HEADER_LEN + 32;
const MIN_RECORD_BYTES: usize = OVERHEAD + 1;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Capacity {
    pub bytes: u64,
    pub rows: u64,
}

impl Capacity {
    pub const ZERO: Self = Self::new(0, 0);

    pub const fn new(bytes: u64, rows: u64) -> Self {
        Self { bytes, rows }
    }

    pub fn add(self, other: Self) -> Result<Self, GatewayError> {
        Ok(Self::new(
            self.bytes
                .checked_add(other.bytes)
                .ok_or(GatewayError::Durability)?,
            self.rows
                .checked_add(other.rows)
                .ok_or(GatewayError::Durability)?,
        ))
    }

    pub fn sub(self, other: Self) -> Result<Self, GatewayError> {
        Ok(Self::new(
            self.bytes
                .checked_sub(other.bytes)
                .ok_or(GatewayError::Durability)?,
            self.rows
                .checked_sub(other.rows)
                .ok_or(GatewayError::Durability)?,
        ))
    }

    pub const fn fits(self, limit: Self) -> bool {
        self.bytes <= limit.bytes && self.rows <= limit.rows
    }
}

pub(crate) struct Journal {
    file: File,
    used: Capacity,
    checkpoint: [u8; 32],
    poisoned: bool,
    #[cfg(test)]
    pub(crate) fault: Option<JournalFault>,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum JournalFault {
    TornWrite,
    SyncFailure,
}

impl Journal {
    pub fn open(
        root: &Path,
        initialize: bool,
        maximum: Capacity,
        mut apply: impl FnMut(&[u8]) -> Result<(), GatewayError>,
    ) -> Result<Self, GatewayError> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create_new(initialize);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            // In particular, exclude FILE_SHARE_DELETE while this handle is live.
            options.share_mode(0x0000_0003);
        }
        let mut file = options
            .open(root.join(NAME))
            .map_err(|_| GatewayError::Durability)?;
        file.try_lock().map_err(|error| match error {
            TryLockError::WouldBlock => GatewayError::WriterBusy,
            TryLockError::Error(_) => GatewayError::Durability,
        })?;

        let file_len = file.metadata().map_err(|_| GatewayError::Durability)?.len();
        if file_len > maximum.bytes {
            return Err(GatewayError::Durability);
        }
        let mut used = Capacity::ZERO;
        let mut checkpoint = [0; 32];
        while used.bytes < file_len {
            let available = file_len - used.bytes;
            if used.rows >= maximum.rows {
                return Err(GatewayError::Durability);
            }

            let mut header = [0; HEADER_LEN];
            let read_len = usize::try_from(available.min(HEADER_LEN as u64))
                .map_err(|_| GatewayError::Durability)?;
            file.read_exact(&mut header[..read_len])
                .map_err(|_| GatewayError::Durability)?;
            let sequence = used.rows + 1;
            let mut expected_prefix = [0; 16];
            expected_prefix[..8].copy_from_slice(MAGIC);
            expected_prefix[8..].copy_from_slice(&sequence.to_be_bytes());
            let check_len = read_len.min(expected_prefix.len());
            if header[..check_len] != expected_prefix[..check_len] {
                return Err(GatewayError::Durability);
            }
            if read_len < HEADER_LEN {
                if read_len >= PREFIX_LEN {
                    let body_len = read_body_len(&header)?;
                    let hash = header_hash(&header[..PREFIX_LEN], &checkpoint);
                    if !valid_record_length(body_len)
                        || header[PREFIX_LEN..read_len] != hash[..read_len - PREFIX_LEN]
                    {
                        return Err(GatewayError::Durability);
                    }
                }
                break;
            }
            let body_len = read_body_len(&header)?;
            if !valid_record_length(body_len)
                || header[PREFIX_LEN..] != header_hash(&header[..PREFIX_LEN], &checkpoint)
            {
                return Err(GatewayError::Durability);
            }
            let complete_len = (OVERHEAD + body_len) as u64;
            if available < complete_len {
                break;
            }

            let mut body = vec![0; body_len];
            let mut checksum = [0; 32];
            file.read_exact(&mut body)
                .map_err(|_| GatewayError::Durability)?;
            file.read_exact(&mut checksum)
                .map_err(|_| GatewayError::Durability)?;
            if checksum != record_hash(&header, &body) {
                return Err(GatewayError::Durability);
            }
            apply(&body)?;
            used = used.add(Capacity::new(complete_len, 1))?;
            checkpoint = checksum;
        }

        if used.bytes < file_len {
            file.set_len(used.bytes)
                .map_err(|_| GatewayError::Durability)?;
        }
        file.seek(SeekFrom::Start(used.bytes))
            .map_err(|_| GatewayError::Durability)?;
        // Treat complete, recovered records as committed only after this barrier.
        file.sync_all().map_err(|_| GatewayError::Durability)?;
        #[cfg(unix)]
        if initialize {
            File::open(root)
                .and_then(|directory| directory.sync_all())
                .map_err(|_| GatewayError::Durability)?;
        }

        Ok(Self {
            file,
            used,
            checkpoint,
            poisoned: false,
            #[cfg(test)]
            fault: None,
        })
    }

    pub const fn check_ready(&self) -> Result<(), GatewayError> {
        if self.poisoned {
            Err(GatewayError::Durability)
        } else {
            Ok(())
        }
    }

    pub fn check_hold(&self, hold: Capacity, maximum: Capacity) -> Result<(), GatewayError> {
        if self.used.add(hold)?.fits(maximum) {
            Ok(())
        } else {
            Err(GatewayError::Durability)
        }
    }

    pub fn poison(&mut self) {
        self.poisoned = true;
    }

    pub fn append(
        &mut self,
        body: &[u8],
        remaining_hold: Capacity,
        admission: Option<Capacity>,
        maximum: Capacity,
    ) -> Result<(), GatewayError> {
        self.check_ready()?;
        if self.file.metadata().map(|meta| meta.len()).ok() != Some(self.used.bytes)
            || !valid_record_length(body.len())
        {
            self.poison();
            return Err(GatewayError::Durability);
        }
        let next = self
            .used
            .add(Capacity::new((OVERHEAD + body.len()) as u64, 1))?;
        if let Some(admission) = admission {
            if !next.fits(admission) || !next.add(remaining_hold)?.fits(maximum) {
                return Err(GatewayError::Quota);
            }
        } else if !next.add(remaining_hold)?.fits(maximum) {
            self.poison();
            return Err(GatewayError::Durability);
        }

        let mut header = [0; HEADER_LEN];
        header[..8].copy_from_slice(MAGIC);
        header[8..16].copy_from_slice(&next.rows.to_be_bytes());
        header[16..PREFIX_LEN].copy_from_slice(
            &u32::try_from(body.len())
                .map_err(|_| GatewayError::Durability)?
                .to_be_bytes(),
        );
        let header_digest = header_hash(&header[..PREFIX_LEN], &self.checkpoint);
        header[PREFIX_LEN..].copy_from_slice(&header_digest);
        let checksum = record_hash(&header, body);
        if self.write_record(&header, body, &checksum).is_err() {
            self.poison();
            return Err(GatewayError::Durability);
        }
        self.used = next;
        self.checkpoint = checksum;
        Ok(())
    }

    fn write_record(&mut self, header: &[u8], body: &[u8], checksum: &[u8]) -> std::io::Result<()> {
        #[cfg(test)]
        let fault = self.fault.take();
        self.file.write_all(header)?;
        #[cfg(test)]
        if matches!(fault, Some(JournalFault::TornWrite)) {
            self.file.write_all(&body[..body.len() / 2])?;
            return Err(std::io::Error::other("injected write failure"));
        }
        self.file.write_all(body)?;
        self.file.write_all(checksum)?;
        #[cfg(test)]
        if matches!(fault, Some(JournalFault::SyncFailure)) {
            return Err(std::io::Error::other("injected sync failure"));
        }
        self.file.sync_all()
    }
}

fn read_body_len(header: &[u8; HEADER_LEN]) -> Result<usize, GatewayError> {
    Ok(u32::from_be_bytes(
        header[16..PREFIX_LEN]
            .try_into()
            .map_err(|_| GatewayError::Durability)?,
    ) as usize)
}

fn valid_record_length(body_len: usize) -> bool {
    (MIN_RECORD_BYTES - OVERHEAD..=MAX_RECORD_BYTES - OVERHEAD).contains(&body_len)
}

fn header_hash(prefix: &[u8], previous: &[u8; 32]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"nlos/llm-gateway/journal-header/v1");
    hash.update(previous);
    hash.update(prefix);
    hash.finalize().into()
}

fn record_hash(header: &[u8], body: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"nlos/llm-gateway/journal-record/v1");
    hash.update(header);
    hash.update(body);
    hash.finalize().into()
}
