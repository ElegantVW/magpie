use directories::ProjectDirs;
use std::path::PathBuf;

pub fn cache_dir() -> PathBuf {
    if let Some(p) = ProjectDirs::from("", "", "magpie") {
        p.cache_dir().to_path_buf()
    } else {
        std::env::temp_dir().join("magpie")
    }
}

pub fn slugify(q: &str) -> String {
    let mut s = String::new();
    for c in q.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
        } else if c == ' ' || c == '-' || c == '_' {
            s.push('-');
        }
    }
    let mut out = String::new();
    let mut last_dash = false;
    for c in s.chars() {
        if c == '-' {
            if !last_dash {
                out.push(c);
            }
            last_dash = true;
        } else {
            out.push(c);
            last_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

pub fn seek_dir(query: &str, out: Option<String>) -> PathBuf {
    if let Some(o) = out {
        return PathBuf::from(o);
    }
    cache_dir().join("seek").join(slugify(query))
}
