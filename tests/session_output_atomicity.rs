//! Reference bookkeeping failure must not publish a failed session update.
use mio_coding_agent::{
    session::Session,
    shell::output_store::{
        INDEX_VERSION, MAX_SOURCE_ENTRIES, OutputStore, ReferenceSource, ReferenceSourceIndex,
    },
};

#[test]
fn full_reference_registry_preserves_the_previous_session_on_save_failure() {
    let workspace = tempfile::tempdir().unwrap();
    let store = OutputStore::open(workspace.path()).unwrap();
    let root = workspace.path().canonicalize().unwrap();
    let sources = ReferenceSourceIndex {
        version: INDEX_VERSION,
        sources: (0..MAX_SOURCE_ENTRIES)
            .map(|number| ReferenceSource {
                path: root
                    .join(format!("existing-{number}.json"))
                    .to_str()
                    .unwrap()
                    .to_owned(),
                registered: 1,
            })
            .collect(),
    };
    std::fs::write(
        store.source_index_path(),
        serde_json::to_vec(&sources).unwrap(),
    )
    .unwrap();
    let path = root.join("custom-session.json");
    let mut session = Session::new(&root).unwrap();
    session.name = Some("previous".into());
    let previous = serde_json::to_vec_pretty(&session).unwrap();
    std::fs::write(&path, &previous).unwrap();
    session.name = Some("update must not be published".into());

    assert!(session.save(&path).is_err());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        previous,
        "failed reference registration must preserve the previous session snapshot"
    );
}
