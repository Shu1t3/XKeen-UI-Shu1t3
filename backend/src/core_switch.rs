use crate::update_transaction::Workspace;
use std::{future::Future, path::Path};
use tokio::fs;

async fn install(init: &Path, staged: &Path) -> Result<(), String> {
    fs::rename(staged, init)
        .await
        .map_err(|e| format!("Замена init: {e}"))?;
    sync_directory(init.parent().ok_or("Нет каталога init")?).await
}

async fn sync_directory(path: &Path) -> Result<(), String> {
    let parent = path.to_owned();
    tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

async fn stage(path: &Path, bytes: &[u8], permissions: std::fs::Permissions) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await
        .map_err(|e| e.to_string())?;
    file.write_all(bytes).await.map_err(|e| e.to_string())?;
    file.set_permissions(permissions)
        .await
        .map_err(|e| e.to_string())?;
    file.sync_all().await.map_err(|e| e.to_string())
}

pub async fn transact<P, PF, C, CF, S>(
    init: &Path,
    old: &str,
    new: &str,
    was_running: bool,
    work: &mut Workspace,
    preflight: P,
    command: C,
    set_core: S,
) -> Result<(), String>
where
    P: FnOnce() -> PF,
    PF: Future<Output = Result<(), String>>,
    C: Fn(String, bool) -> CF,
    CF: Future<Output = Result<(), String>>,
    S: Fn(&str),
{
    preflight().await?;
    let original = fs::read(init)
        .await
        .map_err(|e| format!("Чтение init: {e}"))?;
    let text = std::str::from_utf8(&original).map_err(|e| e.to_string())?;
    let assignment = format!("name_client=\"{old}\"");
    // Reject ambiguous or unsupported init formats instead of stopping a working core.
    let matches = text
        .lines()
        .filter(|line| line.trim() == assignment)
        .count();
    let assignments = text
        .lines()
        .filter(|line| line.trim_start().starts_with("name_client="))
        .count();
    if matches != 1 || assignments != 1 {
        return Err("Init должен содержать единственное name_client с текущим ядром".into());
    }
    let replacement = text
        .split_inclusive('\n')
        .map(|line| {
            if line.trim() == assignment {
                line.replacen(&assignment, &format!("name_client=\"{new}\""), 1)
            } else {
                line.to_owned()
            }
        })
        .collect::<String>();
    let permissions = fs::metadata(init)
        .await
        .map_err(|e| e.to_string())?
        .permissions();
    let backup = work.path.join("init.backup");
    stage(&backup, &original, permissions.clone()).await?;
    // Both staged files are beside init, so rename cannot cross filesystems.
    let parent = init.parent().ok_or("Нет каталога init")?;
    let token = uuid::Uuid::new_v4();
    let next = parent.join(format!(".xkeen-switch-{token}.new"));
    let restore = parent.join(format!(".xkeen-switch-{token}.restore"));
    if let Err(e) = stage(&next, replacement.as_bytes(), permissions.clone()).await {
        let _ = fs::remove_file(&next).await;
        return Err(e);
    }
    if let Err(e) = stage(&restore, &original, permissions).await {
        let _ = fs::remove_file(&next).await;
        let _ = fs::remove_file(&restore).await;
        return Err(e);
    }
    if let Err(e) = async {
        sync_directory(parent).await?;
        sync_directory(&work.path).await
    }
    .await
    {
        let _ = fs::remove_file(&next).await;
        let _ = fs::remove_file(&restore).await;
        return Err(e);
    }
    let result = async {
        command("stop".into(), false).await?;
        install(init, &next).await?;
        set_core(new);
        command("start".into(), true).await
    }
    .await;
    if let Err(error) = result {
        // A failed stop/start may still have changed process state; attempt full recovery.
        let stop_error = command("stop".into(), false).await.err();
        let restored = install(init, &restore).await;
        if restored.is_ok() {
            set_core(old);
        }
        let restart = if restored.is_ok() && was_running && stop_error.is_none() {
            command("start".into(), true).await
        } else {
            Ok(())
        };
        let recovery = restored.err().or(stop_error).or(restart.err());
        let _ = fs::remove_file(&next).await;
        if let Some(e) = recovery {
            work.preserve();
            return Err(format!(
                "{error}; восстановление не завершено: {e}; резервная копия: {}",
                backup.display()
            ));
        }
        return Err(format!("{error}; прежнее ядро и init восстановлены"));
    }
    let _ = fs::remove_file(&restore).await;
    Ok(())
}

#[cfg(test)]
#[path = "core_switch_tests.rs"]
mod tests;
