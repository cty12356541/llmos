use std::fs::File;
use std::io;

#[cfg(unix)]
use cap_fs_ext::OpenOptionsSyncExt;
use cap_fs_ext::{DirExt, FollowSymlinks, MetadataExt, OpenOptionsFollowExt};
use cap_std::fs::{Dir, Metadata, OpenOptions};

/// Opens one regular file relative to a trusted local workspace directory.
#[derive(Clone, Copy, Debug, Default)]
pub struct RootedWorkspaceOpener;

impl RootedWorkspaceOpener {
    /// Opens and validates a read-only file handle. The host supplies an
    /// authorized directory object in a managed local filesystem, including
    /// control over workspace mounts. The relative syntax uses "/" only.
    ///
    /// # Errors
    ///
    /// Fails for unsupported paths, links/reparse points, non-regular files,
    /// wrong file extent, or filesystem errors.
    pub fn open(self, root: &Dir, path: &str, expected_size: u64) -> io::Result<File> {
        validate_relative_path(path)?;
        validate_directory(&root.dir_metadata()?)?;

        let mut components = path.rsplitn(2, '/');
        let leaf = components
            .next()
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut parent = None;
        if let Some(prefix) = components.next() {
            for component in prefix.split('/') {
                let directory = parent
                    .as_ref()
                    .unwrap_or(root)
                    .open_dir_nofollow(component)?;
                validate_directory(&directory.dir_metadata()?)?;
                parent = Some(directory);
            }
        }

        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        options.nonblock(true);
        let file = parent.as_ref().unwrap_or(root).open_with(leaf, &options)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || is_reparse_point(&metadata) || metadata.nlink() != 1 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workspace file type",
            ));
        }
        if metadata.len() != expected_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "workspace file extent",
            ));
        }
        Ok(file.into_std())
    }
}

pub(super) fn validate_relative_path(path: &str) -> io::Result<()> {
    if path.len() > 1024 || path.split('/').count() > 32 || path.split('/').any(invalid_component) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "workspace relative path",
        ));
    }
    Ok(())
}

fn invalid_component(component: &str) -> bool {
    component.is_empty()
        || component.len() > 255
        || component.ends_with([' ', '.'])
        || component.chars().any(|c| {
            c.is_ascii_control() || matches!(c, '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*')
        })
        || windows_device_name(component)
}

fn windows_device_name(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
        .is_some_and(|port| {
            matches!(
                port,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
}

fn validate_directory(metadata: &Metadata) -> io::Result<()> {
    if metadata.is_dir() && !is_reparse_point(metadata) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::PermissionDenied,
        "workspace directory type",
    ))
}

#[cfg(windows)]
fn is_reparse_point(metadata: &Metadata) -> bool {
    use cap_fs_ext::OsMetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
const fn is_reparse_point(_metadata: &Metadata) -> bool {
    false
}
