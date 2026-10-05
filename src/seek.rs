use crate::crawl::{crawl_urls, expand_second_hop, plan_urls, Page};
use crate::error::{MagpieError, Result};
use crate::pdf::render_pdf;
use crate::store::seek_dir;
use std::collections::HashSet;

#[derive(Debug, serde::Serialize)]
pub struct SeekDoc {
    pub query: String,
    pub depth: String,
    pub created: String,
    pub pages: Vec<Page>,
    pub hosts: Vec<String>,
    pub warnings: Vec<String>,
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
    // second-hop: harvest outbound links for diversity when engines throttle
    if pages.len() < pages_n {
        if let Some(tx) = &prog {
            let _ = tx.send("expanding outbound links…".into());
        }
        let extra = expand_second_hop(&pages, pages_n.saturating_sub(pages.len())).await;
        if !extra.is_empty() {
            if let Some(tx) = &prog {
                let _ = tx.send(format!("crawling {} extra urls…", extra.len()));
            }
            let mut more = crawl_urls(&extra, prog.clone()).await;
            pages.append(&mut more);
        }
    }
    // sort longest first, thorough
    pages.sort_by_key(|p| std::cmp::Reverse(p.chars));

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

    let doc = SeekDoc {
        query: query.clone(),
        depth: depth.clone(),
        created: chrono::Utc::now().to_rfc3339(),
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
            let _ = tx.send("rendering PDF…".into());
        }
        let pdf_path = dir.join("manual.pdf");
        render_pdf(&doc.query, &doc.pages, &doc.hosts, &pdf_path)?;
    }

    // evidence log for hung/stall diagnosis
    let _ = std::fs::write(
        dir.join("seek.log"),
        format!(
            "query={}\ndepth={}\ncreated={}\npages={} hosts={:?}\nwarnings={:?}\n",
            doc.query, doc.depth, doc.created,
            doc.pages.len(), doc.hosts, doc.warnings
        ),
    );

    if let Some(tx) = &prog {
        let _ = tx.send("done".into());
    }
    Ok(dir)
}
