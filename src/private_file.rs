use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

pub fn open_read(path: &Path) -> io::Result<File> {
    #[cfg(not(unix))]
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private input must be a regular file",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private input must be a regular file",
        ));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "private input must not have hard links",
        ));
    }
    Ok(file)
}

pub fn read_limited(path: &Path, max_bytes: usize) -> io::Result<Vec<u8>> {
    let file = open_read(path)?;
    if file.metadata()?.len() > max_bytes as u64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private input exceeds its size limit",
        ));
    }
    let mut bytes = Vec::new();
    file.take((max_bytes as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "private input exceeds its size limit",
        ));
    }
    Ok(bytes)
}

pub fn open_append(path: &Path, create: bool) -> io::Result<File> {
    #[cfg(not(unix))]
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log must be a regular file",
            ));
        }
    }
    let mut options = OpenOptions::new();
    options.append(true).create(create);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log must be a regular file",
        ));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "log must not have hard links",
        ));
    }
    Ok(file)
}

pub fn restrict_permissions(file: &File) -> io::Result<()> {
    #[cfg(unix)]
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    #[cfg(not(unix))]
    let _ = file;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn private_read_enforces_the_limit_without_changing_contents_or_permissions() {
        let path = std::env::temp_dir().join(format!("dfor77-private-read-{}", std::process::id()));
        std::fs::write(&path, b"private").unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(read_limited(&path, 7).unwrap(), b"private");
        assert_eq!(
            read_limited(&path, 6).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(read_limited(&path, 0).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"private");
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
        std::fs::write(&path, b"").unwrap();
        assert!(read_limited(&path, 0).unwrap().is_empty());
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            read_limited(&path, 7).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_read_rejects_links_and_special_files_without_waiting_or_chmod() {
        let root = std::env::temp_dir().join(format!("dfor77-read-links-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let target = root.join("target");
        std::fs::write(&target, b"private").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&target, root.join("symlink")).unwrap();
        assert!(read_limited(&root.join("symlink"), 1024).is_err());
        std::fs::hard_link(&target, root.join("hardlink")).unwrap();
        assert!(read_limited(&root.join("hardlink"), 1024).is_err());
        assert!(read_limited(&target, 1024).is_err());
        assert!(read_limited(&root, 1024).is_err());
        let fifo = root.join("fifo");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let started = std::time::Instant::now();
        assert!(read_limited(&fifo, 1024).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert_eq!(std::fs::read(&target).unwrap(), b"private");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_append_preserves_existing_contents_and_uses_private_permissions() {
        let path = std::env::temp_dir().join(format!("dfor77-private-file-{}", std::process::id()));
        std::fs::write(&path, b"before").unwrap();
        let mut file = open_append(&path, false).unwrap();
        restrict_permissions(&file).unwrap();
        file.write_all(b"after").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"beforeafter");
        #[cfg(unix)]
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn links_and_special_files_are_rejected_before_any_write_or_chmod() {
        let root = std::env::temp_dir().join(format!("dfor77-file-links-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let target = root.join("target");
        std::fs::write(&target, b"private").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o640)).unwrap();
        std::os::unix::fs::symlink(&target, root.join("symlink")).unwrap();
        assert!(open_append(&root.join("symlink"), true).is_err());
        std::fs::hard_link(&target, root.join("hardlink")).unwrap();
        assert!(open_append(&root.join("hardlink"), true).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"private");
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert!(open_append(&root, false).is_err());
        let fifo = root.join("fifo");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success());
        let started = std::time::Instant::now();
        assert!(open_append(&fifo, true).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        std::fs::remove_dir_all(root).unwrap();
    }
}
