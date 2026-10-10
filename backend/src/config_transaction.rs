//! Serialized configuration snapshots and durable same-directory replacement.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub static CONFIG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Awaiting request may be cancelled; the spawned transaction still releases its lock only after finishing.
pub async fn run<T: Send + 'static>(
    work: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    tokio::spawn(async move {
        let _guard = CONFIG_LOCK.lock().await;
        work.await
    })
    .await
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// Resolve existing symlinks so replacement retains the link and updates its referent.
fn target_path(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(_) => fs::canonicalize(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| io::Error::other("Missing parent"))?;
            Ok(fs::canonicalize(parent)?.join(path.file_name().ok_or_else(|| io::Error::other("Missing filename"))?))
        }
        Err(e) => Err(e),
    }
}

pub fn write_atomic(path: &Path, content: &[u8], create_only: bool) -> Result<(), String> {
    write_with_hook(path, content, create_only, |_| Ok(()))
}

fn write_with_hook(
    path: &Path, content: &[u8], create_only: bool, mut hook: impl FnMut(&str) -> io::Result<()>,
) -> Result<(), String> {
    let parent_path = path.parent().ok_or_else(|| "Missing parent directory".to_string())?;
    let canonical_parent = fs::canonicalize(parent_path).map_err(|e| e.to_string())?;
    let target = target_path(path).map_err(|e| e.to_string())?;
    if !target.starts_with(&canonical_parent) {
        return Err("Канонический путь выходит за пределы целевого каталога".into());
    }
    if target.file_name().and_then(|n| n.to_str()) == Some("xkeen-ui.json") {
        return Err("Запрещена запись в файл настроек".into());
    }
    let parent = target.parent().unwrap();
    let existing = match fs::metadata(&target) {
        Ok(metadata) => Some(metadata),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.to_string()),
    };
    if create_only && existing.is_some() {
        return Err("File already exists".into());
    }
    let staging = parent.join(format!(".xkeen-config-{}", uuid::Uuid::new_v4()));
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&staging).map_err(|e| e.to_string())?;
    let staged = staging.join("new");
    let backup = staging.join("previous");
    let mut committed = false;
    let operation = (|| -> io::Result<()> {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&staged)?;
        if let Some(metadata) = &existing {
            file.set_permissions(metadata.permissions())?;
        }
        // The hook runs after a genuine partial staged write, never a live truncate.
        let mid = content.len() / 2;
        file.write_all(&content[..mid])?;
        hook("write")?;
        file.write_all(&content[mid..])?;
        hook("stage_sync")?;
        file.sync_all()?;
        if existing.is_some() {
            fs::copy(&target, &backup)?;
            hook("backup_sync")?;
            File::open(&backup)?.sync_all()?;
        }
        sync_dir(&staging)?;
        sync_dir(parent)?;
        hook("rename")?;
        if create_only {
            // Unlike rename, link cannot overwrite a concurrent external creation.
            fs::hard_link(&staged, &target)?;
        } else {
            fs::rename(&staged, &target)?;
        }
        committed = true;
        hook("commit_sync")?;
        sync_dir(parent)?;
        Ok(())
    })();
    if let Err(error) = operation {
        if committed {
            let restore = (|| -> io::Result<()> {
                hook("restore")?;
                if existing.is_some() {
                    let restore = staging.join("restore");
                    fs::copy(&backup, &restore)?;
                    File::open(&restore)?.sync_all()?;
                    fs::rename(restore, &target)?;
                } else {
                    fs::remove_file(&target)?;
                }
                sync_dir(parent)
            })();
            if let Err(recovery) = restore {
                return Err(format!(
                    "{error}; recovery failed: {recovery}; preserved backup: {}",
                    staging.display()
                ));
            }
        }
        let _ = fs::remove_dir_all(&staging);
        return Err(error.to_string());
    }
    // Commit was durably synced. Cleanup failure must not turn a successful commit into a retry.
    let _ = fs::remove_dir_all(&staging);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> PathBuf {
        let p = std::env::temp_dir().join(format!("config-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&p).unwrap();
        p
    }
    #[test]
    fn failed_staging_and_rename_never_change_live_file() {
        for point in ["write", "stage_sync", "backup_sync", "rename", "commit_sync"] {
            let dir = fixture();
            let p = dir.join("config.json");
            fs::write(&p, b"working").unwrap();
            let result = write_with_hook(&p, b"replacement", false, |step| {
                if step == point {
                    Err(io::Error::from_raw_os_error(28))
                } else {
                    Ok(())
                }
            });
            assert!(result.is_err(), "{point}");
            assert_eq!(fs::read(&p).unwrap(), b"working", "{point}");
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
            fs::remove_dir_all(dir).unwrap();
        }
    }
    #[test]
    fn new_file_rollback_and_create_only_collision() {
        let dir = fixture();
        let p = dir.join("config.json");
        assert!(write_with_hook(&p, b"new", true, |s| if s == "commit_sync" {
            Err(io::Error::other("sync"))
        } else {
            Ok(())
        })
        .is_err());
        assert!(!p.exists());
        assert!(write_with_hook(&p, b"new", true, |s| {
            if s == "rename" {
                fs::write(&p, b"external")?;
            }
            Ok(())
        })
        .is_err());
        assert_eq!(fs::read(&p).unwrap(), b"external");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn recovery_failure_preserves_previous_copy() {
        let dir = fixture();
        let p = dir.join("config.json");
        fs::write(&p, b"working").unwrap();
        let err = write_with_hook(&p, b"new", false, |s| {
            if s == "commit_sync" || s == "restore" {
                Err(io::Error::other(s))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(err.contains("preserved backup"));
        let stage = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.is_dir())
            .unwrap();
        assert_eq!(fs::read(stage.join("previous")).unwrap(), b"working");
        fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn retains_permissions_and_symlink() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = fixture();
        let p = dir.join("actual.json");
        let link = dir.join("link.json");
        fs::write(&p, b"old").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o640)).unwrap();
        symlink(&p, &link).unwrap();
        write_atomic(&link, b"new", false).unwrap();
        assert!(fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&p).unwrap(), b"new");
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o640);
        fs::remove_dir_all(dir).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn symlink_escaping_parent_directory_is_rejected() {
        use std::os::unix::fs::symlink;
        let dir = fixture();
        let outside = fixture();
        let target_file = outside.join("secret.json");
        fs::write(&target_file, b"unmodified").unwrap();
        let link = dir.join("link.json");
        symlink(&target_file, &link).unwrap();
        let res = write_atomic(&link, b"evil", false);
        assert!(res.is_err(), "Должен отклонить symlink вне каталога");
        assert_eq!(fs::read(&target_file).unwrap(), b"unmodified");
        fs::remove_dir_all(dir).unwrap();
        fs::remove_dir_all(outside).unwrap();
    }
}
