//! Programs database: append-only JSONL under ~/.local/share/magpie/db/.
//! Raw-first reserve the house can later rinse and filter:
//! dictionary (wiktionary), thesaurus (wikidata aliases), translations,
//! crawled page texts keyed by URL. Files-first, zero native deps.

use std::path::PathBuf;

pub fn db_dir() -> PathBuf {
    if let Ok(p) = std::env::var("MAGPIE_DB") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    // XDG gotcha: an exported-but-empty XDG_DATA_HOME yields Ok("") and
    // would make every path below relative to the CWD. Treat empty as unset.
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/home/evenweaker".into());
            PathBuf::from(home).join(".local/share")
        });
    base.join("magpie/db")
}

/// Append one JSON record (plus timestamp) to db/<kind>.jsonl.
pub fn append(kind: &str, mut obj: serde_json::Value) {
    let dir = db_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Some(o) = obj.as_object_mut() {
        o.insert(
            "_at".into(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
    }
    let mut line = serde_json::to_string(&obj).unwrap_or_default();
    line.push('\n');
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{kind}.jsonl")))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Append one JSON record (plus timestamp) to db/<subject>/<kind>.jsonl.
/// Subject partitions keep rejected works, noses and (later) catalogs
/// namespaced: quarantine for hermeticism never mingles with opel-corsa.
/// The global files stay the chronological amber; partitions are views.
pub fn append_subject(subject: &str, kind: &str, mut obj: serde_json::Value) {
    let dir = db_dir().join(crate::store::slugify(subject));
    let _ = std::fs::create_dir_all(&dir);
    if let Some(o) = obj.as_object_mut() {
        o.insert(
            "_at".into(),
            serde_json::Value::String(chrono::Utc::now().to_rfc3339()),
        );
    }
    let mut line = serde_json::to_string(&obj).unwrap_or_default();
    line.push('\n');
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{kind}.jsonl")))
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// SHA256 of a file, hex. Manifests pin artifacts by hash so any claim
/// traces to exact bytes (a re-rendered PDF will not match — honestly).
pub fn sha_file(path: &std::path::Path) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    match std::fs::read(path) {
        Ok(b) => {
            h.update(&b);
            format!("{:x}", h.finalize())
        }
        Err(_) => "missing".into(),
    }
}

/// Merge one stage's record into <dir>/manifest.json (created if absent).
/// Stages arrive across processes (seek → process → judge), so the merge
/// is read-modify-write, never overwrite: each stage only owns its key.
pub fn manifest_stage(dir: &std::path::Path, stage: &str, value: serde_json::Value) {
    let path = dir.join("manifest.json");
    let mut root: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({"stages": {}}));
    if let Some(stages) = root.get_mut("stages").and_then(|s| s.as_object_mut()) {
        stages.insert(stage.into(), value);
    }
    root["updated"] = serde_json::Value::String(chrono::Utc::now().to_rfc3339());
    let _ = std::fs::write(&path, serde_json::to_string_pretty(&root).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_merges_stages_without_clobbering() {
        let dir = tempfile::tempdir().unwrap();
        manifest_stage(dir.path(), "seek", serde_json::json!({"pages": 10}));
        manifest_stage(dir.path(), "process", serde_json::json!({"entries": 8}));
        let v: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(v["stages"]["seek"]["pages"], 10);
        assert_eq!(v["stages"]["process"]["entries"], 8);
        assert!(v.get("updated").and_then(|u| u.as_str()).is_some());
    }

    #[test]
    fn sha_pins_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.bin");
        std::fs::write(&p, b"magpie").unwrap();
        let h1 = sha_file(&p);
        assert_eq!(h1.len(), 64);
        std::fs::write(&p, b"magpie!").unwrap();
        assert_ne!(sha_file(&p), h1, "re-render must not match");
        assert_eq!(sha_file(&dir.path().join("nope")), "missing");
    }

    #[test]
    fn subject_partition_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("MAGPIE_DB", dir.path().join("db"));
        append_subject("Hermeticism", "quarantine", serde_json::json!({"title": "Dugin", "reason": "off-subject"}));
        std::env::remove_var("MAGPIE_DB");
        let p = dir.path().join("db/hermeticism/quarantine.jsonl");
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.contains("off-subject"), "refusal on file: {s}");
        assert!(s.contains("_at"), "timestamped: {s}");
    }
}
