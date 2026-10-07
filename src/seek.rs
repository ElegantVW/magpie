use crate::crawl::{crawl_urls, expand_second_hop, plan_urls, Page, QueryCard};
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
    pub pages: Vec<Page>,
    pub hosts: Vec<String>,
    pub warnings: Vec<String>,
}

fn norm_words(t: &str) -> HashSet<String> {
    t.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 3)
        .map(|w| w.to_string())
        .collect()
}

/// Split texts over 12000 chars into ~6000-char paragraph windows.
/// First chunk keeps the URL; rest get #chunk-N fragments (canon keeps
/// fragments, so chunks survive dedupe as distinct entries).
fn chunk_pages(pages: &mut Vec<Page>) {
    const LIMIT: usize = 12_000;
    const WINDOW: usize = 6_000;
    let mut out: Vec<Page> = Vec::with_capacity(pages.len());
    for p in pages.drain(..) {
        if p.chars <= LIMIT {
            out.push(p);
            continue;
        }
        // paragraph boundaries: blank-line runs, else single newlines
        let paras: Vec<&str> = p.text.split("\n\n").collect();
        let mut cur = String::new();
        let mut cur_n = 0usize;
        let mut n = 0usize;
        let mut flush = |cur: &mut String, cur_n: &mut usize, n: &mut usize, out: &mut Vec<Page>, p: &Page| {
            if cur.trim().is_empty() {
                return;
            }
            let url = if *n == 0 { p.url.clone() } else { format!("{}#chunk-{n}", p.url) };
            let title = if *n == 0 {
                p.title.clone()
            } else {
                format!("{} (part {})", p.title, *n + 1)
            };
            let text = std::mem::take(cur);
            let chars = text.chars().count();
            out.push(Page { url, title, text, chars, dup_of: None });
            *cur_n = 0;
            *n += 1;
        };
        for para in paras {
            // hard-split pathological single paragraphs (OCR walls)
            if para.chars().count() > WINDOW {
                flush(&mut cur, &mut cur_n, &mut n, &mut out, &p);
                let mut w = String::new();
                let mut wn = 0usize;
                for word in para.split_whitespace() {
                    let l = word.chars().count() + 1;
                    if wn + l > WINDOW && !w.is_empty() {
                        flush(&mut w, &mut wn, &mut n, &mut out, &p);
                    }
                    if !w.is_empty() {
                        w.push(' ');
                        wn += 1;
                    }
                    w.push_str(word);
                    wn += word.chars().count();
                }
                cur = w;
                cur_n = wn;
                continue;
            }
            let l = para.chars().count() + 2;
            if cur_n + l > WINDOW && !cur.trim().is_empty() {
                flush(&mut cur, &mut cur_n, &mut n, &mut out, &p);
            }
            if !cur.is_empty() {
                cur.push_str("\n\n");
            }
            cur.push_str(para);
            cur_n += l;
        }
        flush(&mut cur, &mut cur_n, &mut n, &mut out, &p);
    }
    *pages = out;
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
    let mut pages = crawl_urls(&plan.urls, prog.clone()).await;
    // bulk legs already fetched content (WARC, structured APIs): merge in
    let n_pre = plan.prefetched.len();
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
            let mut more = crawl_urls(&extra, prog.clone()).await;
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
        pages,
        hosts,
        warnings,
    };

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
    let _ = std::fs::write(
        dir.join("seek.log"),
        format!(
            "query={}\ndepth={}\ncreated={}\ncard={} | {} | aliases={:?}\nlegs: {}\npages={} hosts={:?}\nwarnings={:?}\n",
            doc.query,
            doc.depth,
            doc.created,
            doc.query_card.subject,
            doc.query_card.meaning.chars().take(120).collect::<String>(),
            doc.query_card.aliases,
            legs_line,
            doc.pages.len(),
            doc.hosts,
            doc.warnings
        ),
    );

    if let Some(tx) = &prog {
        let _ = tx.send("done".into());
    }
    Ok(dir)
}
