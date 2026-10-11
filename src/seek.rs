use crate::crawl::{crawl_urls, expand_second_hop, plan_urls, FetchHost, Page, Quarantine, QueryCard};
use crate::error::{MagpieError, Result};
use crate::pdf::render_pdf;
use crate::store::seek_dir;
use std::collections::HashSet;

#[derive(Debug, serde::Serialize)]
pub struct SeekDoc {
    pub query: String,
    pub depth: String,
    pub created: String,
    pub query_card: QueryCard,
    pub legs: Vec<(String, usize)>,
    pub expansion: Vec<String>,
    pub translations: Vec<(String, String, String)>,
    pub pages: Vec<Page>,
    pub hosts: Vec<String>,
    pub warnings: Vec<String>,
    /// per-host fetch roll-up (ok/fail/other + median ms) — the ledger's
    /// summary lives with the run; full rows in fetch.jsonl + db
    #[serde(default)]
    pub fetch_summary: Vec<FetchHost>,
    /// refused works with reasons (quarantine file mirrors this)
    #[serde(default)]
    pub quarantine: Vec<Quarantine>,
}

fn norm_words(t: &str) -> HashSet<String> {
    t.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 3)
        .map(|w| w.to_string())
        .collect()
}

/// Content-defined chunking (FastCDC gear hash over bytes): cut points
/// derive from CONTENT, so identical text chunks identically across
/// re-crawls — seams survive, dedupe gets honest, the loop's floods stay
/// comparable. Cuts snap to paragraph breaks (verbatim quotes stay
/// readable); OCR walls fall back to whitespace, then raw char boundaries.
/// Same contract as the old window chunker: first chunk keeps the URL,
/// rest get #chunk-N fragments.
fn chunk_pages(pages: &mut Vec<Page>) {
    const LIMIT: usize = 12_000; // chars; below this, untouched
    const MIN_B: usize = 2048;
    const AVG_B: usize = 6144;
    const MAX_B: usize = 12_288;
    let mut out: Vec<Page> = Vec::with_capacity(pages.len());
    for p in pages.drain(..) {
        if p.chars <= LIMIT {
            out.push(p);
            continue;
        }
        let bytes = p.text.as_bytes();
        let cuts: Vec<usize> = {
            let mut v = vec![0usize];
            for c in fastcdc::v2020::FastCDC::new(bytes, MIN_B, AVG_B, MAX_B) {
                let end = c.offset + c.length;
                if end < bytes.len() {
                    v.push(snap_cut(bytes, end));
                }
            }
            v.push(bytes.len());
            v.sort_unstable();
            v.dedup();
            v
        };
        if cuts.len() < 3 {
            // single chunk (or degenerate): keep whole, no fragmentation
            out.push(p);
            continue;
        }
        for (n, w) in cuts.windows(2).enumerate() {
            let (a, b) = (w[0], w[1]);
            if a >= b {
                continue;
            }
            let text = p.text[a..b].to_string();
            if text.trim().is_empty() {
                continue;
            }
            let url = if n == 0 { p.url.clone() } else { format!("{}#chunk-{n}", p.url) };
            let title = if n == 0 {
                p.title.clone()
            } else {
                format!("{} (part {})", p.title, n + 1)
            };
            let chars = text.chars().count();
            out.push(Page { url, title, text, chars, dup_of: None });
        }
    }
    *pages = out;
}

/// Snap a byte cut to a paragraph break nearby (backward first, then
/// forward), else whitespace, else the nearest char boundary. \n is ASCII
/// so byte search never splits a codepoint; the final adjust guarantees it.
fn snap_cut(bytes: &[u8], mut cut: usize) -> usize {
    cut = cut.min(bytes.len());
    // backward \n\n within 1500B
    let from = cut.saturating_sub(1500);
    if let Some(pos) = bytes[from..cut].windows(2).rposition(|w| w == b"\n\n") {
        return adjust_boundary(bytes, from + pos + 2);
    }
    // forward \n\n within 1500B
    let to = (cut + 1500).min(bytes.len());
    if cut < to {
        if let Some(pos) = bytes[cut..to].windows(2).position(|w| w == b"\n\n") {
            return adjust_boundary(bytes, cut + pos + 2);
        }
    }
    // whitespace within 200B either way
    let wfrom = cut.saturating_sub(200);
    let wto = (cut + 200).min(bytes.len());
    let mut best: Option<usize> = None;
    let mut best_d = usize::MAX;
    for (i, b) in bytes[wfrom..wto].iter().enumerate() {
        if b.is_ascii_whitespace() {
            let d = (wfrom + i).abs_diff(cut);
            if d < best_d {
                best_d = d;
                best = Some(wfrom + i + 1);
            }
        }
    }
    adjust_boundary(bytes, best.unwrap_or(cut))
}

fn adjust_boundary(bytes: &[u8], mut i: usize) -> usize {
    i = i.min(bytes.len());
    while i < bytes.len() && !is_boundary(bytes, i) {
        i += 1;
    }
    if i > bytes.len() {
        i = bytes.len();
    }
    while i > 0 && !is_boundary(bytes, i) {
        i -= 1;
    }
    i
}

fn is_boundary(bytes: &[u8], i: usize) -> bool {
    if i >= bytes.len() {
        return true;
    }
    // UTF-8 continuation bytes are 10xxxxxx
    bytes[i] & 0xC0 != 0x80
}

/// Near-duplicate pass: flag pages whose normalized word overlap ≥80%
/// as `dup_of` the longer keeper (locale mirrors, syndication).
/// Repetition is signal: dups are kept as corroboration, not dropped.
/// Returns the number flagged.
fn mark_dupes(pages: &mut [Page]) -> usize {
    let sets: Vec<HashSet<String>> = pages.iter().map(|p| norm_words(&p.text)).collect();
    let mut flagged = 0;
    for i in 0..pages.len() {
        if pages[i].dup_of.is_some() {
            continue;
        }
        for j in (i + 1)..pages.len() {
            if pages[j].dup_of.is_some() {
                continue;
            }
            let (a, b) = (&sets[i], &sets[j]);
            if a.is_empty() || b.is_empty() {
                continue;
            }
            let inter = a.intersection(b).count();
            let union = a.union(b).count();
            if union > 0 && inter as f64 / union as f64 >= 0.8 {
                // keeper = longer text
                let (keeper, dup) = if pages[i].chars >= pages[j].chars { (i, j) } else { (j, i) };
                pages[dup].dup_of = Some(keeper);
                flagged += 1;
                if dup == i {
                    break;
                }
            }
        }
    }
    flagged
}

pub async fn run_seek(query: String, depth: String, pages_n: usize, out: Option<String>, no_pdf: bool) -> Result<std::path::PathBuf> {
    run_seek_with_progress(query, depth, pages_n, out, no_pdf, None).await
}

pub async fn run_seek_with_progress(
    query: String,
    depth: String,
    pages_n: usize,
    out: Option<String>,
    no_pdf: bool,
    prog: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> Result<std::path::PathBuf> {
    // depth floors: a manual needs volume. Explicit --pages below the
    // floor is raised (time no longer matters more than coverage).
    let pages_n = match depth.as_str() {
        "quick" => pages_n.max(4),
        "extreme" => pages_n.max(50),
        _ => pages_n.max(20),
    };
    let dir = seek_dir(&query, out);
    std::fs::create_dir_all(&dir).map_err(|e| MagpieError::Io(e.to_string()))?;

    if let Some(tx) = &prog {
        let _ = tx.send("planning queries…".into());
    }
    let plan = plan_urls(&query, &depth, pages_n, prog.clone()).await;
    if plan.urls.is_empty() {
        return Err(MagpieError::NoResults("no urls planned".into()));
    }
    if let Some(tx) = &prog {
        let _ = tx.send(format!("crawling {} urls…", plan.urls.len()));
    }
    let (mut pages, mut frecs) = crawl_urls(&plan.urls, prog.clone(), &plan.title_hints).await;
    // bulk legs already fetched content (WARC, structured APIs): merge in
    let n_pre = plan.prefetched.len();
    // raw reserve: bulk legs never pass through crawl_urls' archive, so
    // persist them here — TBs are spare, the DB is the house's hoard
    for p in plan.prefetched.iter() {
        crate::db::append(
            "crawl",
            serde_json::json!({"url": p.url, "title": p.title, "chars": p.chars, "text": p.text}),
        );
    }
    pages.extend(plan.prefetched);
    if n_pre > 0 {
        if let Some(tx) = &prog {
            let _ = tx.send(format!("bulk: +{n_pre} prefetched pages"));
        }
    }
    // second-hop: harvest outbound links for diversity when engines throttle
    if pages.len() < pages_n {
        if let Some(tx) = &prog {
            let _ = tx.send("expanding outbound links…".into());
        }
        let extra = expand_second_hop(&query, &pages, pages_n.saturating_sub(pages.len())).await;
        if !extra.is_empty() {
            if let Some(tx) = &prog {
                let _ = tx.send(format!("crawling {} extra urls…", extra.len()));
            }
            let (mut more, mut more_recs) = crawl_urls(&extra, prog.clone(), &Default::default()).await;
            frecs.append(&mut more_recs);
            pages.append(&mut more);
        }
    }
    // chapter chunking: long texts (books, long articles) split into
    // ~6000-char paragraph windows, one entry each. This is the density
    // answer: a 300KB book becomes ~50 entries instead of one lead excerpt.
    chunk_pages(&mut pages);
    // near-duplicate pass: flag mirrors as dup_of (kept as corroboration)
    let dupes = mark_dupes(&mut pages);
    // rank: spec density + length, drop spam hosts already filtered
    // score = chars/1000 + 3*spec_hits (numbers+units), prefer tables
    pages.sort_by_key(|p| {
        let t = p.text.to_lowercase();
        let mut hits = 0;
        for u in ["ps", "hp", "kw", "nm", "mm", "kg", "l/100", "g/km", "0-100", "top speed", "torque", "power", "fault", "problem", "recall", "service", "oil", "timing"] {
            if t.contains(u) {
                hits += 1;
            }
        }
        std::cmp::Reverse((p.chars / 1000) + hits * 3)
    });

    let mut hosts_set = HashSet::new();
    for p in &pages {
        if let Ok(u) = url::Url::parse(&p.url) {
            hosts_set.insert(u.host_str().unwrap_or("").to_string());
        }
    }
    let hosts: Vec<String> = {
        let mut h: Vec<String> = hosts_set.into_iter().collect();
        h.sort();
        h
    };

    let mut warnings = vec![];
    let wiki_only = hosts.iter().all(|h| h.contains("wikipedia.org"));
    if pages.len() < 4 {
        warnings.push("few pages fetched; results may be thin — retry --depth extreme".into());
    }
    if hosts.len() < 3 {
        warnings.push(format!("low host diversity ({} hosts); need many various sources", hosts.len()));
    }
    if wiki_only {
        warnings.push("wikipedia-only; refusing to call this thorough".into());
    }
    if dupes > 0 {
        warnings.push(format!("{dupes} mirror page(s) flagged as corroboration (dup_of)"));
    }
    // source-mix requirement for a real manual: want spec-ish + fault/review-ish
    {
        let t = pages.iter().map(|p| p.text.to_lowercase()).collect::<Vec<_>>().join("\n");
        let has_spec = ["ps", "hp", "kw ", "nm ", "mm", "0-100", "g/km"].iter().any(|u| t.contains(u));
        let has_life = ["fault", "problem", "recall", "review", "common"].iter().any(|u| t.contains(u));
        if !has_spec {
            warnings.push("no specification tables found — manual will lack specs".into());
        }
        if !has_life {
            warnings.push("no faults/reviews found — manual will lack reliability content".into());
        }
    }

    let doc = SeekDoc {
        query: query.clone(),
        depth: depth.clone(),
        created: chrono::Utc::now().to_rfc3339(),
        query_card: plan.query_card.clone(),
        legs: plan.legs.clone(),
        expansion: plan.expansion.clone(),
        translations: plan.translations.clone(),
        pages,
        hosts,
        warnings,
        fetch_summary: crate::crawl::summarize_fetches(&frecs),
        quarantine: plan.quarantine.clone(),
    };

    // quarantine: refused works with reasons — per-run file plus the
    // subject partition (quarantine is auditable, trust is not)
    {
        let _ = std::fs::write(
            dir.join("quarantine.json"),
            serde_json::to_string_pretty(&doc.quarantine).unwrap_or_default(),
        );
        for q in doc.quarantine.iter() {
            crate::db::append_subject(
                &query,
                "quarantine",
                serde_json::json!({"title": q.title, "url": q.url, "leg": q.leg, "reason": q.reason}),
            );
        }
    }

    // fetch ledger per run: one JSON line per attempt-chain (failures are
    // data — the next run plans around throttles and dead hosts)
    {
        let mut buf = String::new();
        for r in &frecs {
            buf.push_str(&serde_json::to_string(r).unwrap_or_default());
            buf.push('\n');
        }
        let _ = std::fs::write(dir.join("fetch.jsonl"), buf);
    }

    let json_path = dir.join("seek.json");
    let s = serde_json::to_string_pretty(&doc).map_err(|e| MagpieError::Io(e.to_string()))?;
    std::fs::write(&json_path, s).map_err(|e| MagpieError::Io(e.to_string()))?;

    // processed.json = minimal pixie-ready normalization (no LLM here)
    let processed = serde_json::json!({
        "query": doc.query,
        "title": format!("{} — seek", doc.query),
        "summary": doc.pages.first().map(|p| p.text.chars().take(800).collect::<String>()).unwrap_or_default(),
        "sources": doc.pages.iter().map(|p| serde_json::json!({"url": p.url, "title": p.title, "chars": p.chars})).collect::<Vec<_>>(),
        "hosts": doc.hosts,
        "warnings": doc.warnings,
    });
    std::fs::write(
        dir.join("processed.json"),
        serde_json::to_string_pretty(&processed).unwrap(),
    )
    .map_err(|e| MagpieError::Io(e.to_string()))?;

    if !no_pdf {
        if let Some(tx) = &prog {
            let _ = tx.send("rendering raw dump…".into());
        }
        // NOTE: this is a RAW crawl dump, not a manual.
        // Only `magpie process` / `magpie manual` may write manual.pdf.
        let pdf_path = dir.join("dump.pdf");
        render_pdf(&doc.query, &doc.pages, &doc.hosts, &pdf_path)?;
    }

    // evidence log: per-leg yields answer "where are the books" in one cat
    let legs_line = doc
        .legs
        .iter()
        .map(|(l, n)| format!("{l}={n}"))
        .collect::<Vec<_>>()
        .join(" ");
    let fetch_line = doc
        .fetch_summary
        .iter()
        .map(|f| format!("{}(ok:{} fail:{} other:{} {}ms)", f.host, f.ok, f.fail, f.other, f.ms_med))
        .collect::<Vec<_>>()
        .join(" ");
    let _ = std::fs::write(
        dir.join("seek.log"),
        format!(
            "query={}\ndepth={}\ncreated={}\ncard={} | {} | aliases={:?}\nlegs: {}\npages={} hosts={:?}\nfetch: {}\nrefused={}\nwarnings={:?}\n",
            doc.query,
            doc.depth,
            doc.created,
            doc.query_card.subject,
            doc.query_card.meaning.chars().take(120).collect::<String>(),
            doc.query_card.aliases,
            legs_line,
            doc.pages.len(),
            doc.hosts,
            fetch_line,
            doc.quarantine.len(),
            doc.warnings
        ),
    );

    // manifest stage seek: pin every artifact by hash so any claim
    // traces to exact bytes (claim → entry → page → fetch → run)
    crate::db::manifest_stage(
        &dir,
        "seek",
        serde_json::json!({
            "query": doc.query,
            "subject": doc.query_card.subject,
            "seek.json": crate::db::sha_file(&dir.join("seek.json")),
            "fetch.jsonl": crate::db::sha_file(&dir.join("fetch.jsonl")),
            "quarantine.json": crate::db::sha_file(&dir.join("quarantine.json")),
            "dump.pdf": crate::db::sha_file(&dir.join("dump.pdf")),
            "pages": doc.pages.len(),
            "refused": doc.quarantine.len(),
        }),
    );

    if let Some(tx) = &prog {
        let _ = tx.send("done".into());
    }
    Ok(dir)
}

#[cfg(test)]
mod chunk_tests {
    use super::{chunk_pages, Page};

    fn page(url: &str, text: String) -> Page {
        let chars = text.chars().count();
        Page { url: url.into(), title: "T".into(), text, chars, dup_of: None }
    }

    fn big_text() -> String {
        // 30KB of paragraph-structured prose with a hard seam phrase
        let mut s = String::new();
        for i in 0..100 {
            s.push_str(&format!("Paragraph {i} about Hermes Trismegistus and the Corpus Hermeticum, with alchemy and philosophy woven through dense esoteric prose meant to fill out chunk windows.\n\n"));
        }
        s
    }

    #[test]
    fn seams_are_deterministic() {
        let mut a = vec![page("https://x.example/book", big_text())];
        let mut b = vec![page("https://x.example/book", big_text())];
        chunk_pages(&mut a);
        chunk_pages(&mut b);
        assert!(a.len() > 1, "long text must fragment");
        let ta: Vec<&str> = a.iter().map(|p| p.text.as_str()).collect();
        let tb: Vec<&str> = b.iter().map(|p| p.text.as_str()).collect();
        assert_eq!(ta, tb, "same bytes → same seams across re-crawls");
    }

    #[test]
    fn chunks_are_verbatim_substrings() {
        let src = big_text();
        let mut v = vec![page("https://x.example/book", src.clone())];
        chunk_pages(&mut v);
        for c in &v {
            assert!(src.contains(&c.text), "chunk must occur verbatim (quotes depend on it): {:?}", &c.text[..60.min(c.text.len())]);
        }
        // first keeps the URL, rest fragment
        assert_eq!(v[0].url, "https://x.example/book");
        assert!(v[1].url.starts_with("https://x.example/book#chunk-"));
    }

    #[test]
    fn short_text_untouched_and_ocr_wall_survives() {
        let mut v = vec![page("https://x.example/s", "small text".into())];
        chunk_pages(&mut v);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].text, "small text");
        // 20KB single paragraph (OCR wall): must fragment without panic,
        // every piece a valid substring
        let wall = "lorem ipsum dolor sit amet ".repeat(900);
        let mut w = vec![page("https://x.example/wall", wall.clone())];
        chunk_pages(&mut w);
        assert!(w.len() > 1, "wall must fragment");
        for c in &w {
            assert!(wall.contains(&c.text));
        }
    }
}
