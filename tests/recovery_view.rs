//! Recovery is an offline, read-only view of persisted history and intent.
use mio_coding_agent::{journal::Journal, session::Session};
use serde_json::Value;
use std::{path::Path, process::Command};

fn recovery(root: &Path, json: bool) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mio"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("MIO_") {
            command.env_remove(key);
        }
    }
    command
        .env("MIO_CONFIG_DIR", root.join("config"))
        .env("MIO_PROVIDER", "invalid-provider-must-not-be-loaded")
        .arg("--cwd")
        .arg(root)
        .arg("--recovery");
    if json {
        command.arg("--json");
    }
    command.output().unwrap()
}

#[test]
fn recovery_shows_unsaved_goals_and_never_repairs_corrupt_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut session = Session::new(&root).unwrap();
    let id = session.id.to_string();
    let journal = Journal::open(&root, &id);
    journal
        .begin_run_with_goal(Some("continue task\n\u{1b}[2J"))
        .unwrap();
    let output = recovery(&root, true);
    assert!(output.status.success());
    let event: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["view"]["sessionId"], id);
    assert_eq!(event["view"]["sessionSaved"], false);
    assert!(event["view"]["savedMessages"].is_null());
    assert_eq!(event["view"]["automaticReplay"], false);
    assert_eq!(
        event["data"]["unfinished_goals"][0]["goal"],
        "continue task\n\u{1b}[2J"
    );
    let text = recovery(&root, false);
    assert!(text.status.success());
    assert!(
        !text.stdout.contains(&0x1b),
        "goal escaped into terminal control"
    );
    assert!(
        String::from_utf8(text.stdout)
            .unwrap()
            .contains("Unfinished request")
    );

    let path = root.join(".mio/sessions/custom-name.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    session.save(&path).unwrap();
    let saved: Value = serde_json::from_slice(&recovery(&root, true).stdout).unwrap();
    assert_eq!(saved["view"]["sessionSaved"], true);
    assert_eq!(saved["view"]["savedMessages"], 0);
    assert_eq!(saved["view"]["sessionFile"], path.to_str().unwrap());
    let duplicate = root.join(".mio/sessions/duplicate.json");
    std::fs::copy(&path, &duplicate).unwrap();
    let ambiguous: Value = serde_json::from_slice(&recovery(&root, true).stdout).unwrap();
    assert_eq!(ambiguous["view"]["sessionSaved"], false);
    assert!(
        ambiguous["view"]["sessionError"]
            .as_str()
            .unwrap()
            .contains("Multiple")
    );
    std::fs::remove_file(duplicate).unwrap();
    // A corrupt UUID-named file remains discoverable without trusting its JSON.
    let uuid_path = root.join(".mio/sessions").join(format!("{id}.json"));
    std::fs::rename(&path, &uuid_path).unwrap();
    let path = uuid_path;
    std::fs::write(&path, "corrupt history").unwrap();
    let corrupt: Value = serde_json::from_slice(&recovery(&root, true).stdout).unwrap();
    assert_eq!(corrupt["view"]["sessionSaved"], false);
    assert!(corrupt["view"]["sessionError"].is_string());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "corrupt history");
}
