use usagi_core::infrastructure::store::dispatch::DispatchStore;

#[test]
fn cloned_dispatch_store_preserves_its_durable_root() {
    let directory = tempfile::tempdir().unwrap();
    let store = DispatchStore::new(directory.path());

    let cloned = store.clone();

    assert_eq!(cloned.registry_path(), store.registry_path());
}
