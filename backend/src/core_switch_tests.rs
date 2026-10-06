use crate::{core_switch::transact, update_transaction::Workspace};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const ORIGINAL: &str = "#!/bin/sh\nname_client=\"xray\"\nARGS=\"--custom\"\n";

struct Fixture {
    dir: PathBuf,
    init: PathBuf,
    events: Arc<Mutex<Vec<String>>>,
    current: Arc<Mutex<String>>,
}

impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("xkeen-switch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let init = dir.join("S24xkeen");
        std::fs::write(&init, ORIGINAL).unwrap();
        std::fs::set_permissions(&init, std::fs::Permissions::from_mode(0o751)).unwrap();
        Self {
            dir,
            init,
            events: Arc::new(Mutex::new(Vec::new())),
            current: Arc::new(Mutex::new("xray".into())),
        }
    }

    async fn run(
        &self,
        work: &mut Workspace,
        was_running: bool,
        preflight_ok: bool,
        failures: Vec<String>,
    ) -> Result<(), String> {
        let preflight_events = self.events.clone();
        let command_events = self.events.clone();
        let command_current = self.current.clone();
        let set_events = self.events.clone();
        let set_current = self.current.clone();
        let init = self.init.clone();
        let failures = Arc::new(Mutex::new(failures));
        transact(
            &self.init,
            ("xray", "mihomo"),
            was_running,
            work,
            move || async move {
                preflight_events.lock().unwrap().push("preflight".into());
                if preflight_ok {
                    Ok(())
                } else {
                    Err("invalid candidate config".into())
                }
            },
            move |action, verify_health| {
                let events = command_events.clone();
                let current = command_current.lock().unwrap().clone();
                let init = init.clone();
                let failures = failures.clone();
                async move {
                    let event = format!("{action}:{current}");
                    events.lock().unwrap().push(event.clone());
                    if action == "start" {
                        assert!(verify_health, "every start must verify health");
                        let text = std::fs::read_to_string(&init).unwrap();
                        assert!(text.contains(&format!("name_client=\"{current}\"")));
                    }
                    let mut failures = failures.lock().unwrap();
                    if let Some(index) = failures.iter().position(|failure| failure == &event) {
                        failures.remove(index);
                        Err(format!("injected {event} failure"))
                    } else {
                        Ok(())
                    }
                }
            },
            move |core| {
                set_events.lock().unwrap().push(format!("set:{core}"));
                *set_current.lock().unwrap() = core.into();
            },
        )
        .await
    }

    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }

    fn assert_restored(&self) {
        assert_eq!(std::fs::read_to_string(&self.init).unwrap(), ORIGINAL);
        assert_eq!(*self.current.lock().unwrap(), "xray");
        assert_eq!(
            std::fs::metadata(&self.init).unwrap().permissions().mode() & 0o777,
            0o751
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn preflight_failure_never_stops_or_changes_old_core() {
    let fixture = Fixture::new();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    assert!(fixture.run(&mut work, true, false, vec![]).await.is_err());
    assert_eq!(fixture.events(), ["preflight"]);
    fixture.assert_restored();
}

#[tokio::test]
async fn success_preflights_before_stop_and_publishes_init_before_start() {
    let fixture = Fixture::new();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    fixture.run(&mut work, true, true, vec![]).await.unwrap();
    assert_eq!(
        fixture.events(),
        ["preflight", "stop:xray", "set:mihomo", "start:mihomo"]
    );
    assert_eq!(*fixture.current.lock().unwrap(), "mihomo");
    assert_eq!(
        std::fs::read_to_string(&fixture.init).unwrap(),
        ORIGINAL.replace("\"xray\"", "\"mihomo\"")
    );
    assert_eq!(
        std::fs::metadata(&fixture.init)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o751
    );
    let stage = work.path.clone();
    let lock = work.lock_path().to_path_buf();
    drop(work);
    assert!(!stage.exists());
    assert!(!lock.exists());
}

#[tokio::test]
async fn new_start_failure_restores_init_selection_and_old_running_core() {
    let fixture = Fixture::new();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    assert!(
        fixture
            .run(&mut work, true, true, vec!["start:mihomo".into()])
            .await
            .is_err()
    );
    fixture.assert_restored();
    assert_eq!(
        fixture.events(),
        [
            "preflight",
            "stop:xray",
            "set:mihomo",
            "start:mihomo",
            "stop:mihomo",
            "set:xray",
            "start:xray"
        ]
    );
}

#[tokio::test]
async fn failed_switch_of_stopped_core_does_not_start_old_core() {
    let fixture = Fixture::new();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    assert!(
        fixture
            .run(&mut work, false, true, vec!["start:mihomo".into()])
            .await
            .is_err()
    );
    fixture.assert_restored();
    assert!(!fixture.events().iter().any(|event| event == "start:xray"));
    assert!(fixture.events().iter().any(|event| event == "stop:mihomo"));
}

#[tokio::test]
async fn invalid_or_missing_init_never_stops_or_changes_selection() {
    for invalid in [
        "#!/bin/sh\n",
        "name_client=\"xray\"\nname_client=\"xray\"\n",
        "name_client=\"mihomo\"\n",
    ] {
        let fixture = Fixture::new();
        std::fs::write(&fixture.init, invalid).unwrap();
        let mut work = Workspace::create(&fixture.dir).unwrap();
        assert!(fixture.run(&mut work, true, true, vec![]).await.is_err());
        assert_eq!(std::fs::read_to_string(&fixture.init).unwrap(), invalid);
        assert_eq!(*fixture.current.lock().unwrap(), "xray");
        assert!(
            !fixture
                .events()
                .iter()
                .any(|event| event.starts_with("stop:")
                    || event.starts_with("start:")
                    || event.starts_with("set:"))
        );
    }
    let fixture = Fixture::new();
    std::fs::remove_file(&fixture.init).unwrap();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    assert!(fixture.run(&mut work, true, true, vec![]).await.is_err());
    assert!(!fixture.init.exists());
    assert_eq!(*fixture.current.lock().unwrap(), "xray");
    assert!(
        !fixture
            .events()
            .iter()
            .any(|event| event.starts_with("stop:")
                || event.starts_with("start:")
                || event.starts_with("set:"))
    );
}

#[tokio::test]
async fn stop_failure_keeps_init_and_recovers_old_running_core() {
    let fixture = Fixture::new();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    assert!(
        fixture
            .run(&mut work, true, true, vec!["stop:xray".into()])
            .await
            .is_err()
    );
    fixture.assert_restored();
    assert!(!fixture.events().iter().any(|event| event == "set:mihomo"));
    assert!(fixture.events().iter().any(|event| event == "start:xray"));
}

#[tokio::test]
async fn failed_old_core_recovery_preserves_backup_workspace_and_lock() {
    let fixture = Fixture::new();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    let stage = work.path.clone();
    let lock = work.lock_path().to_path_buf();
    assert!(
        fixture
            .run(
                &mut work,
                true,
                true,
                vec!["start:mihomo".into(), "start:xray".into()]
            )
            .await
            .is_err()
    );
    fixture.assert_restored();
    drop(work);
    assert!(stage.is_dir());
    assert!(lock.is_dir());
    assert_eq!(
        std::fs::read_to_string(lock.join("owner")).unwrap().trim(),
        stage.to_string_lossy()
    );
    assert!(
        std::fs::read_dir(stage).unwrap().any(|entry| {
            let entry = entry.unwrap();
            entry.file_type().unwrap().is_file()
                && std::fs::read_to_string(entry.path()).is_ok_and(|content| content == ORIGINAL)
        }),
        "workspace must retain original init for manual recovery"
    );
}

#[tokio::test]
async fn persistent_stop_failure_preserves_recovery_data_without_starting_another_core() {
    let fixture = Fixture::new();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    let stage = work.path.clone();
    let lock = work.lock_path().to_path_buf();
    assert!(
        fixture
            .run(
                &mut work,
                true,
                true,
                vec!["stop:xray".into(), "stop:xray".into()]
            )
            .await
            .is_err()
    );
    fixture.assert_restored();
    assert!(
        !fixture
            .events()
            .iter()
            .any(|event| event.starts_with("start:"))
    );
    drop(work);
    assert!(stage.is_dir());
    assert!(lock.is_dir());
}

#[tokio::test]
async fn switch_lock_excludes_concurrent_switches_and_updates_until_completion() {
    let fixture = Fixture::new();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    assert!(Workspace::create(&fixture.dir).is_err());
    fixture.run(&mut work, true, true, vec![]).await.unwrap();
    // Ownership lasts until the transaction's workspace drops, not just until start succeeds.
    assert!(Workspace::create(&fixture.dir).is_err());
    drop(work);
    let next = Workspace::create(&fixture.dir).unwrap();
    assert!(next.lock_path().is_dir());
}

#[tokio::test]
async fn cancelled_waiter_still_finishes_failed_start_rollback() {
    let fixture = Fixture::new();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    let init = fixture.init.clone();
    let current = fixture.current.clone();
    let set_current = current.clone();
    let events = fixture.events.clone();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let command_gate = gate.clone();
    let started = Arc::new(tokio::sync::Notify::new());
    let command_started = started.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(crate::update_transaction::run_to_completion(async move {
        let result = transact(
            &init,
            ("xray", "mihomo"),
            true,
            &mut work,
            || async { Ok(()) },
            move |action, verify_health| {
                let core = current.lock().unwrap().clone();
                let events = events.clone();
                let gate = command_gate.clone();
                let started = command_started.clone();
                async move {
                    events.lock().unwrap().push(format!("{action}:{core}"));
                    if action == "start" {
                        assert!(verify_health);
                    }
                    if action == "start" && core == "mihomo" {
                        started.notify_one();
                        let _permit = gate.acquire().await.unwrap();
                        Err("new core unhealthy".into())
                    } else {
                        Ok(())
                    }
                }
            },
            move |core| *set_current.lock().unwrap() = core.into(),
        )
        .await;
        drop(work);
        done_tx.send(result).unwrap();
    }));
    tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
        .await
        .unwrap();
    assert!(Workspace::create(&fixture.dir).is_err());
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    gate.add_permits(1);
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(5), done_rx)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    fixture.assert_restored();
    assert_eq!(
        fixture.events(),
        ["stop:xray", "start:mihomo", "stop:mihomo", "start:xray"]
    );
    assert!(!fixture.dir.join(".xkeen-ui-update.lock").exists());
}

async fn run_with_staged_file_removed(
    fixture: &Fixture,
    work: &mut Workspace,
    remove_restore: bool,
) -> Result<(), String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let current = fixture.current.clone();
    let set_current = current.clone();
    let events = fixture.events.clone();
    let dir = fixture.dir.clone();
    let removed = Arc::new(AtomicUsize::new(0));
    let command_removed = removed.clone();
    let result = transact(
        &fixture.init,
        ("xray", "mihomo"),
        true,
        work,
        || async { Ok(()) },
        move |action, verify_health| {
            let core = current.lock().unwrap().clone();
            let events = events.clone();
            let dir = dir.clone();
            let removed = command_removed.clone();
            async move {
                events.lock().unwrap().push(format!("{action}:{core}"));
                if action == "start" {
                    assert!(verify_health);
                }
                let should_remove = if remove_restore {
                    action == "start" && core == "mihomo"
                } else {
                    action == "stop" && core == "xray"
                };
                if should_remove && removed.load(Ordering::SeqCst) == 0 {
                    let suffix = if remove_restore { ".restore" } else { ".new" };
                    let staged = std::fs::read_dir(&dir)
                        .unwrap()
                        .find_map(|entry| {
                            let entry = entry.unwrap();
                            let name = entry.file_name();
                            let name = name.to_string_lossy();
                            (name.starts_with(".xkeen-switch-") && name.ends_with(suffix))
                                .then(|| entry.path())
                        })
                        .expect("transaction must stage init before stopping old core");
                    std::fs::remove_file(staged).unwrap();
                    removed.fetch_add(1, Ordering::SeqCst);
                }
                if remove_restore && action == "start" && core == "mihomo" {
                    Err("new core failed after restore file disappeared".into())
                } else {
                    Ok(())
                }
            }
        },
        move |core| *set_current.lock().unwrap() = core.into(),
    )
    .await;
    assert_eq!(removed.load(Ordering::SeqCst), 1);
    result
}

#[tokio::test]
async fn failed_init_install_restores_original_and_restarts_old_core() {
    let fixture = Fixture::new();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    assert!(
        run_with_staged_file_removed(&fixture, &mut work, false)
            .await
            .is_err()
    );
    fixture.assert_restored();
    assert_eq!(fixture.events(), ["stop:xray", "stop:xray", "start:xray"]);
    let stage = work.path.clone();
    let lock = work.lock_path().to_path_buf();
    drop(work);
    assert!(!stage.exists());
    assert!(!lock.exists());
}

#[tokio::test]
async fn failed_init_restore_preserves_backup_and_lock_without_starting_old_core() {
    let fixture = Fixture::new();
    let mut work = Workspace::create(&fixture.dir).unwrap();
    let stage = work.path.clone();
    let lock = work.lock_path().to_path_buf();
    assert!(
        run_with_staged_file_removed(&fixture, &mut work, true)
            .await
            .is_err()
    );
    assert_eq!(
        fixture.events(),
        ["stop:xray", "start:mihomo", "stop:mihomo"]
    );
    assert_eq!(*fixture.current.lock().unwrap(), "mihomo");
    assert_eq!(
        std::fs::read_to_string(&fixture.init).unwrap(),
        ORIGINAL.replace("\"xray\"", "\"mihomo\"")
    );
    assert_eq!(
        std::fs::read_to_string(stage.join("init.backup")).unwrap(),
        ORIGINAL
    );
    drop(work);
    assert!(stage.is_dir());
    assert!(lock.is_dir());
    assert_eq!(
        std::fs::read_to_string(lock.join("owner")).unwrap().trim(),
        stage.to_string_lossy()
    );
}
