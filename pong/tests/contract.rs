//! Reproducibility of the reusable covenant shipped with the client.
use kaspa_pong::reusable;

#[test]
#[ignore = "recompiles the covenant; run explicitly when changing contracts or the compiler"]
fn current_artifact_compilation_is_reproducible() {
    let scratch = tempfile::tempdir().unwrap();
    let rebuilt = std::thread::Builder::new()
        .stack_size(32 * 1024 * 1024)
        .spawn(move || argent::build_file(concat!(env!("CARGO_MANIFEST_DIR"), "/contracts/reusable.ag"), scratch.path()).unwrap())
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(reusable::artifact().unwrap().id, rebuilt.id);
}
