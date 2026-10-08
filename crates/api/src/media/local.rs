//! Uploads on the local disk, under `UPLOAD_DIR`, at their key's path.
//!
//! Files are written to `.tmp/` first and renamed into place, so a reader
//! (the API's `/uploads` route or a static server) never sees half a file.
//! Files are 0644 and directories 0755 whatever the umask, so a static
//! server running as another user can read what only the API writes;
//! `.tmp/` is 0700.

use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use shared::media::UploadKey;
use uuid::Uuid;

const TMP_DIR: &str = ".tmp";
/// Temporary files older than this were left by a crash.
const STALE_TMP: Duration = Duration::from_secs(3600);

#[derive(Debug)]
pub struct LocalStorage {
    root: PathBuf,
}

impl LocalStorage {
    /// Creates the root (0755) if it doesn't exist and clears what a crash
    /// left in its temporary directory. An existing root keeps its mode.
    pub fn open(root: PathBuf) -> io::Result<Self> {
        if !root.exists() {
            fs::create_dir_all(&root)?;
            fs::set_permissions(&root, Permissions::from_mode(0o755))?;
        }
        let tmp = root.join(TMP_DIR);
        match DirBuilder::new().mode(0o700).create(&tmp) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        for entry in fs::read_dir(&tmp)? {
            let entry = entry?;
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|modified| {
                    SystemTime::now()
                        .duration_since(modified)
                        .is_ok_and(|age| age > STALE_TMP)
                });
            if stale {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub async fn put(&self, key: &UploadKey, bytes: Vec<u8>) -> io::Result<()> {
        let root = self.root.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || write_atomically(&root, &key, &bytes))
            .await
            .map_err(io::Error::other)?
    }

    /// A file already gone counts as deleted.
    pub async fn delete(&self, key: &UploadKey) -> io::Result<()> {
        match tokio::fs::remove_file(self.root.join(key.as_str())).await {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

fn write_atomically(root: &Path, key: &UploadKey, bytes: &[u8]) -> io::Result<()> {
    let mut dir = root.to_path_buf();
    let (dirs, file_name) = key.as_str().rsplit_once('/').unwrap_or(("", key.as_str()));
    for segment in dirs.split('/').filter(|s| !s.is_empty()) {
        dir.push(segment);
        match fs::create_dir(&dir) {
            Ok(()) => fs::set_permissions(&dir, Permissions::from_mode(0o755))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
    }

    let tmp = root
        .join(TMP_DIR)
        .join(format!("{}.part", Uuid::new_v4().simple()));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        file.set_permissions(Permissions::from_mode(0o644))?;
        fs::rename(&tmp, dir.join(file_name))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared::media::UploadKind;

    fn temp_root() -> PathBuf {
        std::env::temp_dir().join(format!("scrobblr_media_{}", Uuid::new_v4().simple()))
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn files_land_at_their_key_readable_by_others() {
        let root = temp_root();
        let storage = LocalStorage::open(root.clone()).unwrap();
        assert_eq!(mode(&root), 0o755);
        assert_eq!(mode(&root.join(TMP_DIR)), 0o700);

        let key = UploadKey::generate(UploadKind::Avatar);
        storage.put(&key, b"jpeg".to_vec()).await.unwrap();
        let path = root.join(key.as_str());
        assert_eq!(fs::read(&path).unwrap(), b"jpeg");
        assert_eq!(mode(&path), 0o644);
        assert_eq!(mode(path.parent().unwrap()), 0o755);
        assert_eq!(mode(path.parent().unwrap().parent().unwrap()), 0o755);
        assert_eq!(fs::read_dir(root.join(TMP_DIR)).unwrap().count(), 0);

        storage.delete(&key).await.unwrap();
        assert!(!path.exists());
        storage.delete(&key).await.unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn opening_clears_stale_temporary_files_only() {
        let root = temp_root();
        LocalStorage::open(root.clone()).unwrap();
        let stale = root.join(TMP_DIR).join("stale.part");
        let fresh = root.join(TMP_DIR).join("fresh.part");
        fs::write(&stale, b"x").unwrap();
        fs::write(&fresh, b"x").unwrap();
        fs::File::options()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_modified(SystemTime::now() - STALE_TMP * 2)
            .unwrap();

        LocalStorage::open(root.clone()).unwrap();
        assert!(!stale.exists());
        assert!(fresh.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn a_failed_write_leaves_nothing_behind() {
        let root = temp_root();
        let storage = LocalStorage::open(root.clone()).unwrap();
        let key = UploadKey::generate(UploadKind::AlbumImage);
        // The rename fails: a directory is in the way.
        fs::create_dir_all(root.join(key.as_str())).unwrap();
        assert!(storage.put(&key, b"jpeg".to_vec()).await.is_err());
        assert_eq!(fs::read_dir(root.join(TMP_DIR)).unwrap().count(), 0);
        fs::remove_dir_all(root).unwrap();
    }
}
