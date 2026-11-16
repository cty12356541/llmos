use cap_std::{ambient_authority, fs::Dir};
use nlos_capability::blob_reference::RootedWorkspaceOpener;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_ROOT: AtomicUsize = AtomicUsize::new(0);

struct Scratch {
    base: PathBuf,
    root: PathBuf,
    outside: PathBuf,
}

impl Scratch {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "nlos-blob-open-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("root");
        let outside = base.join("outside");
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("nested/leaf"), b"inside").unwrap();
        fs::write(outside.join("leaf"), b"outside").unwrap();
        Self {
            base,
            root,
            outside,
        }
    }

    fn directory(&self) -> Dir {
        Dir::open_ambient_dir(&self.root, ambient_authority()).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn read(dir: &Dir, relative: &str, size: u64) -> io::Result<Vec<u8>> {
    let mut file = RootedWorkspaceOpener.open(dir, relative, size)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[test]
fn opens_only_a_single_link_regular_file_under_the_held_directory() {
    let scratch = Scratch::new();
    let dir = scratch.directory();
    assert_eq!(read(&dir, "nested/leaf", 6).unwrap(), b"inside");
    fs::hard_link(scratch.root.join("nested/leaf"), scratch.root.join("other")).unwrap();
    assert!(read(&dir, "nested/leaf", 6).is_err());
    assert!(read(&dir, "nested", 6).is_err());
}

#[test]
fn rejects_ambiguous_relative_paths_and_device_names_on_every_platform() {
    let scratch = Scratch::new();
    let dir = scratch.directory();
    for bad in [
        "",
        "/nested/leaf",
        "//host/share/leaf",
        "nested//leaf",
        "nested/",
        "nested\\leaf",
        "./nested/leaf",
        "nested/../outside",
        "C:leaf",
        "leaf:stream",
        "nested/.",
        "nested/..",
        "NUL",
        "com¹.txt",
        "LPT0.ext",
        "coN .file",
        "CONIN$",
        "a ",
        "a.",
        "a\0b",
    ] {
        assert_eq!(
            RootedWorkspaceOpener.open(&dir, bad, 0).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
    for bad in [
        "a/".repeat(32) + "b",
        "x".repeat(256),
        format!("{0}/{0}/{0}/{0}/{0}", "x".repeat(210)),
    ] {
        assert_eq!(
            RootedWorkspaceOpener
                .open(&dir, &bad, 0)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
    assert_eq!(
        RootedWorkspaceOpener
            .open(&dir, "nested/leaf", 5)
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

fn exercise_swaps(
    dir: &Dir,
    relative: &str,
    active_path: &Path,
    parked_path: &Path,
    replacement: &Path,
    extent: u64,
    expected_when_opened: &[u8],
) {
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let adversary = scope.spawn(|| {
            let mut swaps = 0;
            barrier.wait();
            for _ in 0..512 {
                if fs::rename(active_path, parked_path).is_ok() {
                    if fs::rename(replacement, active_path).is_ok() {
                        std::thread::yield_now();
                        restore(active_path, replacement);
                        swaps += 1;
                    }
                    restore(parked_path, active_path);
                }
                std::thread::yield_now();
            }
            swaps
        });
        barrier.wait();
        let mut inside = 0;
        let mut rejected = 0;
        let mut outside = 0;
        for _ in 0..2048 {
            match read(dir, relative, extent) {
                Ok(bytes) if bytes == expected_when_opened => inside += 1,
                Ok(_) => outside += 1,
                Err(_) => rejected += 1,
            }
            std::thread::yield_now();
        }
        let swaps = adversary.join().unwrap();
        let variant = replacement.file_name().unwrap().to_string_lossy();
        eprintln!(
            "{variant}: swaps={swaps} inside={inside} safe_rejection={rejected} exterior_or_unexpected={outside}"
        );
        assert!(swaps > 0, "the adversary did not install a replacement");
        assert_eq!(outside, 0);
    });
}

fn restore(from: &Path, to: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        match fs::rename(from, to) {
            Ok(()) => return,
            Err(_) if std::time::Instant::now() < deadline => std::thread::yield_now(),
            Err(err) => panic!("failed to restore attack fixture: {err}"),
        }
    }
}

#[test]
fn repeated_first_open_leaf_swaps_do_not_read_the_hard_link_outside_sentinel() {
    let scratch = Scratch::new();
    let dir = scratch.directory();
    let active = scratch.root.join("nested/leaf");
    let replacement = scratch.root.join("replacement");
    fs::write(scratch.outside.join("leaf"), b"outsid").unwrap();
    fs::hard_link(scratch.outside.join("leaf"), &replacement).unwrap();
    exercise_swaps(
        &dir,
        "nested/leaf",
        &active,
        &active.with_file_name("parked"),
        &replacement,
        6,
        b"inside",
    );
}

#[cfg(unix)]
#[test]
fn rooted_opener_rejects_a_fifo_before_reading() {
    let scratch = Scratch::new();
    let fifo = scratch.root.join("nested/fifo");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        RootedWorkspaceOpener
            .open(&scratch.directory(), "nested/fifo", 0)
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn rooted_opener_rejects_a_socket_before_reading() {
    let scratch = Scratch::new();
    let _listener =
        std::os::unix::net::UnixListener::bind(scratch.root.join("nested/socket")).unwrap();
    assert!(
        RootedWorkspaceOpener
            .open(&scratch.directory(), "nested/socket", 0)
            .is_err()
    );
}

#[cfg(unix)]
#[test]
fn rooted_opener_rejects_the_null_character_device_before_reading() {
    let root = Dir::open_ambient_dir("/dev", ambient_authority()).unwrap();
    let error = RootedWorkspaceOpener.open(&root, "null", 0).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

#[cfg(unix)]
#[test]
fn repeated_first_open_symlink_parent_and_leaf_swaps_do_not_escape() {
    let scratch = Scratch::new();
    let dir = scratch.directory();
    fs::write(scratch.outside.join("leaf"), b"outsid").unwrap();
    let nested = scratch.root.join("nested");
    let parent_link = scratch.root.join("link-parent");
    let leaf_link = scratch.root.join("link-leaf");
    std::os::unix::fs::symlink(&scratch.outside, &parent_link).unwrap();
    std::os::unix::fs::symlink(scratch.outside.join("leaf"), &leaf_link).unwrap();
    assert!(read(&dir, "link-parent/leaf", 6).is_err());
    assert!(read(&dir, "link-leaf", 6).is_err());
    exercise_swaps(
        &dir,
        "nested/leaf",
        &nested,
        &scratch.root.join("parked-nested"),
        &parent_link,
        6,
        b"inside",
    );
    exercise_swaps(
        &dir,
        "nested/leaf",
        &nested.join("leaf"),
        &nested.join("parked-leaf"),
        &leaf_link,
        6,
        b"inside",
    );
}

#[cfg(windows)]
#[test]
fn repeated_first_open_junction_parent_swaps_do_not_escape() {
    let scratch = Scratch::new();
    let dir = scratch.directory();
    fs::write(scratch.outside.join("leaf"), b"outsid").unwrap();
    let junction = scratch.root.join("junction");
    let output = std::process::Command::new("cmd.exe")
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&junction)
        .arg(&scratch.outside)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "Windows junction fixture requires a local NTFS filesystem"
    );
    assert!(read(&dir, "junction/leaf", 6).is_err());
    exercise_swaps(
        &dir,
        "nested/leaf",
        &scratch.root.join("nested"),
        &scratch.root.join("parked-nested"),
        &junction,
        6,
        b"inside",
    );
    fs::remove_dir(junction).unwrap();
    assert_eq!(fs::read(scratch.outside.join("leaf")).unwrap(), b"outsid");
}

#[cfg(windows)]
#[test]
#[ignore = "requires Windows symbolic link creation privilege"]
fn repeated_first_open_symlink_leaf_swaps_do_not_escape() {
    let scratch = Scratch::new();
    let dir = scratch.directory();
    fs::write(scratch.outside.join("leaf"), b"outsid").unwrap();
    let link = scratch.root.join("link-leaf");
    std::os::windows::fs::symlink_file(scratch.outside.join("leaf"), &link).unwrap();
    assert!(read(&dir, "link-leaf", 6).is_err());
    let nested = scratch.root.join("nested");
    exercise_swaps(
        &dir,
        "nested/leaf",
        &nested.join("leaf"),
        &nested.join("parked-leaf"),
        &link,
        6,
        b"inside",
    );
    fs::remove_file(link).unwrap();
}
