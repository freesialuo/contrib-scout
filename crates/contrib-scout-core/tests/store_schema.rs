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
