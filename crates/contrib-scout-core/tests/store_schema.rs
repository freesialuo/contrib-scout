use contrib_scout_core::store::{schema, Store};
use std::time::Duration;

#[test]
fn migration_creates_every_table() {
    let store = Store::memory().unwrap();
    assert_eq!(
        store.schema_version().unwrap() as usize,
        schema::MIGRATIONS.len()
    );
    let tables = store.tables().unwrap();
    let mut expected: Vec<String> = schema::TABLES.iter().map(|t| t.to_string()).collect();
    expected.sort();
    assert_eq!(tables, expected);
}

#[test]
fn file_store_reopens_without_remigrating() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "schema-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    ));
    let path = dir.join("nested").join("scout.db");
    let first = Store::open(&path).unwrap();
    assert_eq!(first.schema_version().unwrap(), 1);
    assert!(first.path().is_some());
    let guard = first
        .lock_scope("https://api.github.com/x", Duration::ZERO)
        .unwrap();
    assert!(guard.is_some());
    // A second handle in the same process uses a separate file handle, so the lock is contended.
    let second = Store::open(&path).unwrap();
    assert_eq!(second.schema_version().unwrap(), 1);
    assert!(second
        .lock_scope("https://api.github.com/x", Duration::from_millis(50))
        .unwrap()
        .is_none());
    drop(guard);
    assert!(second
        .lock_scope("https://api.github.com/x", Duration::ZERO)
        .unwrap()
        .is_some());
}

#[test]
fn scope_locks_share_a_fixed_pool_of_files() {
    use contrib_scout_core::store::{scope_slot, SCOPE_LOCK_SLOTS};
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!(
        "scope-pool-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap()
    ));
    let store = Store::open(dir.join("scout.db")).unwrap();
    for i in 0..500 {
        let guard = store
            .lock_scope(
                &format!("flight:https://api.github.com/repos/o/r/issues/{i}"),
                Duration::ZERO,
            )
            .unwrap();
        assert!(guard.is_some());
    }
    let locks = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".lock"))
        .count();
    assert!(
        locks > 1 && locks as u64 <= SCOPE_LOCK_SLOTS,
        "{locks} lock files"
    );

    // Two scopes in one slot serialize: the second waits for the first.
    let a = "flight:a";
    let b = (0..)
        .map(|i| format!("flight:b{i}"))
        .find(|b| scope_slot(b) == scope_slot(a))
        .unwrap();
    let other = Store::open(dir.join("scout.db")).unwrap();
    let held = store.lock_scope(a, Duration::ZERO).unwrap();
    assert!(held.is_some());
    assert!(other.lock_scope(&b, Duration::ZERO).unwrap().is_none());
    drop(held);
    assert!(other.lock_scope(&b, Duration::ZERO).unwrap().is_some());
}
