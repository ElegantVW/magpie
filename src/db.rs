//! Programs database: append-only JSONL under ~/.local/share/magpie/db/.
//! Raw-first reserve the house can later rinse and filter:
//! dictionary (wiktionary), thesaurus (wikidata aliases), translations,
//! crawled page texts keyed by URL. Files-first, zero native deps.

use std::path::PathBuf;

pub fn db_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
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
