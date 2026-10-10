//! The magpie's nose: a Xapian BM25 index over the hoard, one per subject.
//! The amber (JSONL + seek dirs) stays source of truth; the nose is derived
//! and rebuildable — `magpie nose --rebuild` re-presses it from seek dirs.
//! Compartmentalized: db/<subject>/nose/, so partitions never re-mix.
//! Feature-gated (`nose`, default on): --no-default-features builds pure Rust.

use std::ffi::{CStr, CString};
use std::path::PathBuf;

#[repr(C)]
struct CNoseHit {
    docid: u32,
    weight: f64,
    url: *mut std::ffi::c_char,
}

#[repr(C)]
struct CNoseResults {
    hits: *mut CNoseHit,
    n: usize,
    err: [std::ffi::c_char; 256],
}

extern "C" {
    fn nose_open(path: *const std::ffi::c_char, err: *mut std::ffi::c_char, errlen: usize) -> *mut std::ffi::c_void;
    fn nose_close(db: *mut std::ffi::c_void);
    fn nose_commit(db: *mut std::ffi::c_void, err: *mut std::ffi::c_char, errlen: usize) -> i32;
    fn nose_add_doc(
        db: *mut std::ffi::c_void,
        data: *const std::ffi::c_char,
        title: *const std::ffi::c_char,
        text: *const std::ffi::c_char,
        lang: *const std::ffi::c_char,
        err: *mut std::ffi::c_char,
        errlen: usize,
    ) -> u32;
    fn nose_add_synonym(
        db: *mut std::ffi::c_void,
        term: *const std::ffi::c_char,
        synonym: *const std::ffi::c_char,
        err: *mut std::ffi::c_char,
        errlen: usize,
    ) -> i32;
    fn nose_doc_count(db: *mut std::ffi::c_void) -> u32;
    fn nose_search(
        db: *mut std::ffi::c_void,
        query: *const std::ffi::c_char,
        lang: *const std::ffi::c_char,
        limit: usize,
    ) -> *mut CNoseResults;
    fn nose_free_results(r: *mut CNoseResults);
    fn nose_suggest(db: *mut std::ffi::c_void, word: *const std::ffi::c_char, out: *mut std::ffi::c_char, outlen: usize) -> i32;
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

fn take_err(buf: &[std::ffi::c_char; 256]) -> String {
    unsafe { CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned() }
}

fn errbuf() -> ([std::ffi::c_char; 256], usize) {
    ([0; 256], 256)
}

pub struct Nose {
    db: *mut std::ffi::c_void,
}

// Xapian is single-writer; the CLI is short-lived processes. Not Sync by
// design — never share across threads (crawl_urls stays nose-free).
unsafe impl Send for Nose {}

impl Nose {
    pub fn open(path: &std::path::Path) -> Result<Nose, String> {
        let p = cstr(&path.to_string_lossy());
        let (mut eb, len) = errbuf();
        let db = unsafe { nose_open(p.as_ptr(), eb.as_mut_ptr(), len) };
        if db.is_null() {
            return Err(take_err(&eb));
        }
        Ok(Nose { db })
    }

    pub fn commit(&self) -> Result<(), String> {
        let (mut eb, len) = errbuf();
        if unsafe { nose_commit(self.db, eb.as_mut_ptr(), len) } != 0 {
            return Err(take_err(&eb));
        }
        Ok(())
    }

    pub fn doc_count(&self) -> u32 {
        unsafe { nose_doc_count(self.db) }
    }

    /// Index one page. data packs url+title as JSON (the C seam is opaque).
    pub fn add_doc(&self, url: &str, title: &str, text: &str, lang: &str) -> Result<u32, String> {
        let data = serde_json::json!({"u": url, "t": title}).to_string();
        let (mut eb, len) = errbuf();
        let id = unsafe {
            nose_add_doc(
                self.db,
                cstr(&data).as_ptr(),
                cstr(title).as_ptr(),
                cstr(text).as_ptr(),
                cstr(lang).as_ptr(),
                eb.as_mut_ptr(),
                len,
            )
        };
        if id == 0 {
            return Err(take_err(&eb));
        }
        Ok(id)
    }

    pub fn add_synonym(&self, term: &str, synonym: &str) -> Result<(), String> {
        let (mut eb, len) = errbuf();
        if unsafe { nose_add_synonym(self.db, cstr(term).as_ptr(), cstr(&synonym).as_ptr(), eb.as_mut_ptr(), len) } != 0 {
            return Err(take_err(&eb));
        }
        Ok(())
    }

    pub fn suggest(&self, word: &str) -> Option<String> {
        let mut out = vec![0 as std::ffi::c_char; 256];
        let hit = unsafe { nose_suggest(self.db, cstr(word).as_ptr(), out.as_mut_ptr(), out.len()) };
        if hit == 0 {
            return None;
        }
        let s = unsafe { CStr::from_ptr(out.as_ptr()).to_string_lossy().into_owned() };
        if s.is_empty() {
            return None;
        }
        Some(s)
    }

    /// BM25 ranked hits: (weight, url, title). err text on failure.
    pub fn search(&self, query: &str, lang: &str, limit: usize) -> Result<Vec<(f64, String, String)>, String> {
        let r = unsafe { nose_search(self.db, cstr(query).as_ptr(), cstr(lang).as_ptr(), limit) };
        if r.is_null() {
            return Err("nose_search returned null".into());
        }
        let out = self.collect_results(r);
        unsafe { nose_free_results(r) };
        out
    }

    fn collect_results(&self, r: *mut CNoseResults) -> Result<Vec<(f64, String, String)>, String> {
        unsafe {
            let e = CStr::from_ptr((*r).err.as_ptr()).to_string_lossy().into_owned();
            let mut out = vec![];
            for i in 0..(*r).n {
                let h = &*(*r).hits.add(i);
                let data = if h.url.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(h.url).to_string_lossy().into_owned()
                };
                // data packed as JSON {"u","t"}; fall back to raw url
                let (u, t) = match serde_json::from_str::<serde_json::Value>(&data) {
                    Ok(v) => (
                        v.get("u").and_then(|x| x.as_str()).unwrap_or(&data).to_string(),
                        v.get("t").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    ),
                    Err(_) => (data, String::new()),
                };
                out.push((h.weight, u, t));
            }
            if !e.is_empty() {
                return Err(e);
            }
            Ok(out)
        }
    }
}

impl Drop for Nose {
    fn drop(&mut self) {
        unsafe { nose_close(self.db) };
    }
}

/// ISO code → Xapian stemmer name. No stemmer beats a wrong stemmer:
/// unknown tongues index unstemmed ("none") — recall preserved.
/// (Xapian 1.4 set; codes verified against Stem::get_available_languages.)
pub fn xapian_lang(iso: &str) -> &'static str {
    match iso {
        "en" => "english",
        "de" => "german",
        "fr" => "french",
        "es" => "spanish",
        "it" => "italian",
        "pt" => "portuguese",
        "nl" => "dutch",
        "pl" => "polish",
        "ru" => "russian",
        "sv" => "swedish",
        "fi" => "finnish",
        "no" => "norwegian",
        "da" => "danish",
        "hu" => "hungarian",
        "ro" => "romanian",
        "tr" => "turkish",
        "ar" => "arabic",
        "hi" => "hindi",
        "id" => "indonesian",
        "el" => "greek",
        "ca" => "catalan",
        "eu" => "basque",
        _ => "none",
    }
}

pub fn nose_dir(subject: &str) -> PathBuf {
    crate::db::db_dir().join(crate::store::slugify(subject)).join("nose")
}

/// Split to indexable lowercase alphanumeric tokens.
fn toks(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 2)
        .map(|t| t.to_string())
        .collect()
}

/// Re-press the nose for one subject from seek dirs (cache walk or
/// explicit --dir list). Synonyms: alias words + translation labels ↔
/// subject words, both directions (cross-lingual recall rides on these,
/// since stemmers are per-language and never meet). Wipes the old index
/// first: the nose is derived, never merged.
pub fn rebuild(
    subject: &str,
    dirs: &[std::path::PathBuf],
    prog: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> Result<(usize, usize), String> {
    let send = |m: &str| {
        if let Some(tx) = &prog {
            let _ = tx.send(m.to_string());
        }
    };
    let dir = nose_dir(subject);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| e.to_string())?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let nose = Nose::open(&dir)?;
    // explicit dirs (custom --out runs) or the cache walk
    let mut sj_files: Vec<PathBuf> = vec![];
    if dirs.is_empty() {
        let seek_base = crate::store::cache_dir().join("seek");
        if let Ok(entries) = std::fs::read_dir(&seek_base) {
            for e in entries.flatten() {
                let sj = e.path().join("seek.json");
                if sj.is_file() {
                    sj_files.push(sj);
                }
            }
        }
    } else {
        for d in dirs {
            let sj = d.join("seek.json");
            if sj.is_file() {
                sj_files.push(sj);
            }
        }
    }
    let mut docs = 0usize;
    let mut syns = 0usize;
    for sj in sj_files {
        let raw = std::fs::read_to_string(&sj).map_err(|e| e.to_string())?;
        let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
        let query = v.get("query").and_then(|q| q.as_str()).unwrap_or("");
        // explicit --dir runs are trusted as given (custom outs carry the
        // subject in --subject); cache walk filters by slug
        if dirs.is_empty() && crate::store::slugify(query) != crate::store::slugify(subject) {
            continue;
        }
        // per-url language from the translation fan-out; default english
        let mut lang_of: std::collections::HashMap<String, String> = Default::default();
        if let Some(arr) = v.get("translations").and_then(|t| t.as_array()) {
            for t in arr {
                let a = t.as_array().cloned().unwrap_or_default();
                if a.len() >= 3 {
                    if let (Some(l), Some(u)) = (a[0].as_str(), a[2].as_str()) {
                        lang_of.insert(u.to_string(), l.to_string());
                    }
                }
            }
        }
        if let Some(pages) = v.get("pages").and_then(|p| p.as_array()) {
            for p in pages {
                let url = p.get("url").and_then(|u| u.as_str()).unwrap_or("");
                let title = p.get("title").and_then(|t| t.as_str()).unwrap_or("");
                let text = p.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if url.is_empty() || text.chars().count() < 200 {
                    continue;
                }
                let iso = lang_of.get(url).map(|s| s.as_str()).unwrap_or("en");
                match nose.add_doc(url, title, text, xapian_lang(iso)) {
                    Ok(_) => docs += 1,
                    Err(e) => send(&format!("nose skip {url}: {e}")),
                }
            }
        }
        // synonyms: subject words ↔ alias words + translation labels
        let mut subjects: Vec<String> = toks(query);
        if let Some(s) = v.get("query_card").and_then(|c| c.get("subject")).and_then(|s| s.as_str()) {
            subjects.extend(toks(s));
        }
        let mut others: Vec<String> = vec![];
        if let Some(arr) = v.get("query_card").and_then(|c| c.get("aliases")).and_then(|a| a.as_array()) {
            for a in arr.iter().filter_map(|x| x.as_str()) {
                others.extend(toks(a));
            }
        }
        if let Some(arr) = v.get("translations").and_then(|t| t.as_array()) {
            for t in arr {
                let a = t.as_array().cloned().unwrap_or_default();
                if a.len() >= 2 {
                    if let Some(label) = a[1].as_str() {
                        others.extend(toks(label));
                    }
                }
            }
        }
        for s in subjects.iter() {
            for o in others.iter() {
                if s != o {
                    if nose.add_synonym(o, s).is_ok() {
                        syns += 1;
                    }
                    if nose.add_synonym(s, o).is_ok() {
                        syns += 1;
                    }
                }
            }
        }
        send(&format!("nose: {} docs from {}", docs, sj.display()));
    }
    nose.commit()?;
    Ok((docs, syns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nose_roundtrip_ranks_canon_first() {
        let dir = tempfile::tempdir().unwrap();
        let idx = dir.path().join("nose");
        let nose = Nose::open(&idx).unwrap();
        // canon: saturated; pretender: one passing mention; rival: other subject
        nose.add_doc("https://en.wikipedia.org/wiki/Hermeticism", "Hermeticism", &"hermeticism philosophy Hermes Trismegistus Corpus ".repeat(30), "english").unwrap();
        nose.add_doc("https://archive.org/details/politica-test", "Politica Test", &"political theory state power ".repeat(25).to_string().replace("power", "power hermeticism"), "english").unwrap();
        nose.add_doc("https://en.wikipedia.org/wiki/Mojito", "Mojito", &"mojito cocktail rum mint lime ".repeat(30), "english").unwrap();
        nose.add_synonym("hermetism", "hermeticism").unwrap();
        nose.add_synonym("hermeticism", "hermetism").unwrap();
        nose.commit().unwrap();
        assert_eq!(nose.doc_count(), 3);
        let hits = nose.search("hermeticism", "english", 10).unwrap();
        // Xapian returns only matching docs: the rival (no shared terms)
        // is correctly absent, and saturated canon outranks one mention
        assert_eq!(hits.len(), 2, "canon + pretender, rival absent: {hits:?}");
        assert!(hits[0].1.contains("Hermeticism"), "canon first, got {}", hits[0].1);
        assert!(hits[1].1.contains("politica"), "passing mention below canon, got {}", hits[1].1);
        assert!(hits[0].0 > hits[1].0, "canon weight above pretender");
        // alias recall through synonyms
        let alias = nose.search("hermetism", "english", 10).unwrap();
        assert!(alias.iter().any(|(_, u, _)| u.contains("Hermeticism")), "alias must recall canon");
        // spelling table populated: a typo knows its correction
        let sug = nose.suggest("hermeticis");
        assert_eq!(sug.as_deref(), Some("hermeticism"), "typo must suggest, got {sug:?}");
    }

    #[test]
    fn lang_map_never_wrong_stems() {
        assert_eq!(xapian_lang("de"), "german");
        assert_eq!(xapian_lang("ja"), "none");
        assert_eq!(xapian_lang("xx"), "none");
    }
}
