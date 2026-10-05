//! Many git-lex commands register their repositories at the same moment
//! whenever several agents work; no row may be lost (#51). Its own test
//! binary: it points HOME at a scratch directory for the whole process.

#[test]
fn concurrent_registrations_keep_every_row() {
    let base = std::env::temp_dir().join(format!("glx-registry-race-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let home = base.join("home");
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: this test binary has one test, so nothing else reads HOME.
    unsafe { std::env::set_var("HOME", &home) };

    let repos: Vec<std::path::PathBuf> = (0..32).map(|i| base.join(format!("repo-{i:02}"))).collect();
    for r in &repos {
        std::fs::create_dir_all(r).unwrap();
    }
    std::thread::scope(|s| {
        for r in &repos {
            s.spawn(move || git_lex::registry_touch(r));
        }
    });

    let text = std::fs::read_to_string(home.join(".lex").join("repos.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();
    let rows = doc["repos"].as_array().unwrap();
    for r in &repos {
        let key = r.canonicalize().unwrap().display().to_string();
        assert!(
            rows.iter().any(|e| e["path"].as_str() == Some(key.as_str())),
            "row lost for {key}; {} rows survived",
            rows.len()
        );
    }
    assert_eq!(rows.len(), repos.len());
    let _ = std::fs::remove_dir_all(&base);
}
