use adsconsole_core::logging;

#[test]
fn log_file_is_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let guard = logging::init(dir.path()).unwrap();
    tracing::error!("refresh_token=abc123");
    drop(guard);

    let file = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("adsconsole")
        })
        .expect("log file not found");
    let content = std::fs::read_to_string(file).unwrap();
    assert!(content.contains("refresh_token=***"), "{content}");
    assert!(!content.contains("abc123"), "{content}");
}
