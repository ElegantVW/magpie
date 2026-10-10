//! The rinse: deterministic audit over finished runs. No LLM, no network —
//! text verdicts only (a rejection costs no inference to read).
//! Two waters:
//! 1. simhash near-dup across runs: same work re-crawled under different
//!    chunking/OCR must read as one work, not two sources.
//! 2. quarantine audit: refused works/chunks that ALSO appear kept
//!    elsewhere are gate-instability flags (the gates evolved; refusals
//!    must survive re-examination, not trust).

use std::path::{Path, PathBuf};

fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// 64-bit simhash over word-TRIGRAM features. Phrases discriminate where
/// shared jargon does not: unigram simhash reads two esoteric texts as
/// twins (divine/tradition/magic everywhere); trigram simhash only moves
/// on shared phrasing. Identical texts → 0; re-chunked same text stays
/// small (only cut-crossing triples differ); unrelated lands far.
pub fn simhash(text: &str) -> u64 {
    let lowered = text.to_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 2)
        .collect();
    let mut acc = [0i32; 64];
    let mut feat = |s: &str| {
        let h = fnv1a64(s);
        for b in 0..64 {
            if (h >> b) & 1 == 1 {
                acc[b] += 1;
            } else {
                acc[b] -= 1;
            }
        }
    };
    if words.len() < 3 {
        for w in &words {
            feat(w);
        }
    } else {
        for w in words.windows(3) {
            // one allocation per triple would show in profiles; rinse is
            // offline batch, clarity wins over cleverness here
            feat(&format!("{} {} {}", w[0], w[1], w[2]));
        }
    }
    let mut out = 0u64;
    for b in 0..64 {
        if acc[b] > 0 {
            out |= 1 << b;
        }
    }
    out
}

pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// URL without #chunk-N fragments: chunk identity is per-work for audit.
pub fn work_url(u: &str) -> &str {
    u.split('#').next().unwrap_or(u)
}

pub struct RunIndex {
    pub dir: PathBuf,
    pub query: String,
    pub pages: Vec<(String, String, u64)>, // (url, title, simhash)
    pub quarantine: Vec<(String, String, String)>, // (title, url, reason)
    pub drops: Vec<(String, String, String)>, // (title, url, reason)
    pub kept: Vec<String>, // entry work-urls from processed.json
}

fn read_json(path: &Path) -> Option<serde_json::Value> {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok())
}

/// Collect runs: explicit dirs or the cache walk, optional subject filter.
pub fn collect_runs(dirs: &[PathBuf], subject: Option<&str>) -> Vec<RunIndex> {
    let mut sj_files: Vec<PathBuf> = vec![];
    if dirs.is_empty() {
        let base = crate::store::cache_dir().join("seek");
        if let Ok(entries) = std::fs::read_dir(&base) {
            for e in entries.flatten().filter(|e| e.path().is_dir()) {
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
    let mut runs = vec![];
    for sj in sj_files {
        let dir = sj.parent().unwrap_or(Path::new(".")).to_path_buf();
        let v = match read_json(&sj) {
            Some(v) => v,
            None => continue,
        };
        let query = v.get("query").and_then(|q| q.as_str()).unwrap_or("").to_string();
        if let Some(s) = subject {
            if crate::store::slugify(&query) != crate::store::slugify(s) {
                continue;
            }
        }
        let mut pages = vec![];
        if let Some(arr) = v.get("pages").and_then(|p| p.as_array()) {
            for p in arr {
                let url = p.get("url").and_then(|u| u.as_str()).unwrap_or("");
                let title = p.get("title").and_then(|t| t.as_str()).unwrap_or("");
                let text = p.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if url.is_empty() || text.chars().count() < 200 {
                    continue;
                }
                pages.push((url.to_string(), title.to_string(), simhash(text)));
            }
        }
        let mut quarantine = vec![];
        if let Some(arr) = v.get("quarantine").and_then(|q| q.as_array()) {
            for q in arr {
                quarantine.push((
                    q.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    q.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    q.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                ));
            }
        }
        // drops.json: chunk-level refusals with scores
        let mut drops = vec![];
        if let Some(d) = read_json(&dir.join("drops.json")) {
            if let Some(arr) = d.as_array() {
                for q in arr {
                    drops.push((
                        q.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        q.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                        q.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                    ));
                }
            }
        }
        let mut kept = vec![];
        if let Some(p) = read_json(&dir.join("processed.json")) {
            if let Some(arr) = p.get("entries").and_then(|e| e.as_array()) {
                for e in arr {
                    if let Some(u) = e.get("url").and_then(|x| x.as_str()) {
                        kept.push(work_url(u).to_string());
                    }
                }
            }
        }
        runs.push(RunIndex { dir, query, pages, quarantine, drops, kept });
    }
    runs
}

pub struct RinseReport {
    pub runs: usize,
    pub pages: usize,
    pub refused: usize,
    /// same work re-crawled across runs: (runA, url, runB, dist). Dist 0 =
    /// chunking stable (FastCDC seams); >0 = text drifted or chunker moved.
    pub recrawled: Vec<(String, String, String, u32)>,
    pub near_dupes: Vec<(String, String, String, String, u32)>, // runA, urlA, runB, urlB, dist
    pub unstable: Vec<(String, String, String)>, // kind, what, detail
    pub reasons: Vec<(String, usize)>,
}

/// Audit collected runs. Pure function over the index (testable without IO).
pub fn audit(runs: &[RunIndex], max_dist: u32) -> RinseReport {
    let mut near_dupes = vec![];
    let mut recrawled = vec![];
    // cross-run pairs only: within-run dupes already have dup_of
    for (ai, a) in runs.iter().enumerate() {
        for b in runs.iter().skip(ai + 1) {
            for (u1, _, h1) in &a.pages {
                for (u2, _, h2) in &b.pages {
                    let d = hamming(*h1, *h2);
                    if u1 == u2 {
                        // exact same URL re-crawled: chunking stability readout
                        recrawled.push((
                            a.dir.display().to_string(),
                            u1.clone(),
                            b.dir.display().to_string(),
                            d,
                        ));
                        continue;
                    }
                    // same work re-chunked (fragments differ) is expected
                    // granularity, not resemblance — skip it
                    if work_url(u1) == work_url(u2) {
                        continue;
                    }
                    if d <= max_dist {
                        near_dupes.push((
                            a.dir.display().to_string(),
                            u1.clone(),
                            b.dir.display().to_string(),
                            u2.clone(),
                            d,
                        ));
                        if near_dupes.len() >= 50 {
                            break;
                        }
                    }
                }
                if near_dupes.len() >= 50 {
                    break;
                }
            }
        }
    }
    // quarantine audit: refused somewhere, kept elsewhere
    let mut kept_by_work: std::collections::HashMap<String, String> = Default::default();
    for r in runs {
        for k in &r.kept {
            kept_by_work.entry(k.clone()).or_insert_with(|| r.query.clone());
        }
    }
    let mut unstable = vec![];
    for r in runs {
        for (title, url, reason) in r.quarantine.iter().chain(r.drops.iter()) {
            let w = work_url(url);
            if w.is_empty() {
                continue;
            }
            if let Some(kept_in) = kept_by_work.get(w) {
                if *kept_in != r.query {
                    unstable.push((
                        "refused-elsewhere-kept".into(),
                        format!("{title} [{reason}]"),
                        format!("refused in {:?}, kept in {kept_in:?}", r.query),
                    ));
                }
                // same-run refuse+keep (chunk refused, sibling chunk kept)
                // is normal granularity, not instability — skip it
            }
        }
        if unstable.len() >= 50 {
            break;
        }
    }
    let mut reasons: std::collections::HashMap<String, usize> = Default::default();
    for r in runs {
        for (_, _, reason) in r.quarantine.iter().chain(r.drops.iter()) {
            *reasons.entry(reason.clone()).or_insert(0) += 1;
        }
    }
    let mut reasons: Vec<(String, usize)> = reasons.into_iter().collect();
    reasons.sort_by(|a, b| b.1.cmp(&a.1));
    RinseReport {
        runs: runs.len(),
        pages: runs.iter().map(|r| r.pages.len()).sum(),
        refused: runs.iter().map(|r| r.quarantine.len() + r.drops.len()).sum(),
        recrawled,
        near_dupes,
        unstable,
        reasons,
    }
}

pub fn render_report(r: &RinseReport) -> String {
    let mut s = format!(
        "rinse: {} runs, {} pages, {} refusals\n",
        r.runs, r.pages, r.refused
    );
    s.push_str("reasons:\n");
    for (reason, n) in &r.reasons {
        s.push_str(&format!("  {n:>4}  {reason}\n"));
    }
    s.push_str(&format!("near-dupes across runs ({}):\n", r.near_dupes.len()));
    for (ra, ua, rb, ub, d) in r.near_dupes.iter().take(20) {
        s.push_str(&format!("  dist {d}: {ua}\n             {ub}\n             ({ra} ↔ {rb})\n"));
    }
    s.push_str(&format!("re-crawled same URLs ({}; dist 0 = seams stable):\n", r.recrawled.len()));
    for (ra, u, rb, d) in r.recrawled.iter().take(20) {
        s.push_str(&format!("  dist {d}: {u}\n             ({ra} ↔ {rb})\n"));
    }
    s.push_str(&format!("instability flags ({}):\n", r.unstable.len()));
    for (kind, what, detail) in r.unstable.iter().take(20) {
        s.push_str(&format!("  [{kind}] {what}\n    {detail}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simhash_separates() {
        let a = simhash("hermeticism philosophy Hermes Trismegistus Corpus Kybalion");
        let b = simhash("hermeticism philosophy Hermes Trismegistus Corpus Kybalion");
        assert_eq!(hamming(a, b), 0);
        let c = simhash("hermeticism philosophy Hermes Trismegistus Corpus Alchemy");
        assert!(hamming(a, c) < 12, "one word differs, still near");
        let d = simhash("opel corsa diesel engine specifications torque horsepower");
        assert!(hamming(a, d) > 12, "unrelated lands far");
    }

    #[test]
    fn audit_flags_refused_elsewhere_kept() {
        let runs = vec![
            RunIndex {
                dir: PathBuf::from("run-a"),
                query: "alpha".into(),
                pages: vec![],
                quarantine: vec![("W".into(), "https://w.example/book".into(), "off-subject".into())],
                drops: vec![],
                kept: vec![],
            },
            RunIndex {
                dir: PathBuf::from("run-b"),
                query: "beta".into(),
                pages: vec![],
                quarantine: vec![],
                drops: vec![],
                kept: vec!["https://w.example/book".into()],
            },
        ];
        let r = audit(&runs, 6);
        assert_eq!(r.unstable.len(), 1, "refused-in-alpha kept-in-beta must flag");
        // same-run refuse+keep is granularity, not instability
        let same = vec![RunIndex {
            dir: PathBuf::from("run-a"),
            query: "alpha".into(),
            pages: vec![],
            quarantine: vec![],
            drops: vec![("W part 2".into(), "https://w.example/book#chunk-2".into(), "relevance:35".into())],
            kept: vec!["https://w.example/book".into()],
        }];
        let r2 = audit(&same, 6);
        assert!(r2.unstable.is_empty(), "sibling-chunk granularity must not flag");
    }

    #[test]
    fn recrawled_measures_seam_stability() {
        let text = "hermeticism philosophy Hermes Trismegistus Corpus Kybalion alchemy";
        let runs = vec![
            RunIndex {
                dir: PathBuf::from("run-a"),
                query: "alpha".into(),
                pages: vec![("https://x.example/p".into(), "P".into(), simhash(text))],
                quarantine: vec![],
                drops: vec![],
                kept: vec![],
            },
            RunIndex {
                dir: PathBuf::from("run-b"),
                query: "alpha".into(),
                pages: vec![("https://x.example/p".into(), "P".into(), simhash(text))],
                quarantine: vec![],
                drops: vec![],
                kept: vec![],
            },
        ];
        let r = audit(&runs, 3);
        assert_eq!(r.recrawled.len(), 1);
        assert_eq!(r.recrawled[0].3, 0, "identical re-crawl reads dist 0");
        assert!(r.near_dupes.is_empty(), "same URL is stability, not resemblance");
    }
}
