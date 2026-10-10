mod cli;
mod crawl;
mod db;
mod error;
#[cfg(feature = "granary")]
mod granary;
mod judge;
mod llm;
#[cfg(feature = "nose")]
mod nose;
mod pdf;
mod process;
mod rinse;
mod seek;
mod store;
mod termart;
mod tui;

use clap::Parser;
use cli::{Cli, Cmd};

#[tokio::main]
async fn main() {
    let c = Cli::parse();
    let rc = run(c).await;
    if let Err(e) = rc {
        crate::error::eprint_next(&e.to_string(), "check network, then retry --depth extreme");
        std::process::exit(1);
    }
}

async fn run(c: Cli) -> anyhow::Result<()> {
    // compat: `magpie browse --dump URL`
    if let Some(Cmd::Open { dir }) = c.cmd.clone() {
        let dir = match dir {
            Some(d) => std::path::PathBuf::from(d),
            None => crate::store::latest_seek_dir().ok_or_else(|| anyhow::anyhow!("no seeks yet under ~/.cache/magpie/seek/"))?,
        };
        let pdf = {
            let m = dir.join("manual.pdf");
            if m.is_file() { m } else { dir.join("dump.pdf") }
        };
        if !pdf.is_file() {
            anyhow::bail!("no PDF yet in {} — run `magpie manual` first", dir.display());
        }
        // detached viewer, like house openers
        tokio::process::Command::new("xdg-open")
            .arg(&pdf)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| anyhow::anyhow!("xdg-open failed: {e}"))?;
        println!("opened {}", pdf.display());
        return Ok(());
    }
    if let Some(Cmd::Judge { dir }) = c.cmd.clone() {
        let dir = match dir {
            Some(d) => std::path::PathBuf::from(d),
            None => crate::store::latest_seek_dir().ok_or_else(|| anyhow::anyhow!("no seeks yet under ~/.cache/magpie/seek/"))?,
        };
        let dir = crate::judge::run_judge(dir, None).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        let rec: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("judgment.json")).unwrap_or_default()).unwrap_or(serde_json::Value::Null);
        println!("judge: {}", rec.get("overall").and_then(|o| o.as_str()).unwrap_or("?"));
        println!("{}", rec.get("comment").and_then(|c| c.as_str()).unwrap_or(""));
        return Ok(());
    }
    // compat: `magpie browse --dump URL`
    if let Some(Cmd::Browse { rest, dump }) = c.cmd.clone() {
        let url = if rest.is_empty() { String::new() } else { rest.split_whitespace().last().unwrap_or("").to_string() };
        if dump && !url.is_empty() {
            let (pages, _) = crate::crawl::crawl_urls(&[url], None).await;
            if let Some(p) = pages.first() {
                println!("{}\n\n{}", p.title, p.text.chars().take(6000).collect::<String>());
                return Ok(());
            }
            anyhow::bail!("fetch failed");
        }
    }
    if let Some(Cmd::Manual { query, out, depth, pages }) = c.cmd.clone() {
        let dir = crate::seek::run_seek(query.clone(), depth, pages, out, false).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        println!("seek dump -> {}/dump.pdf", dir.display());
        let dir = crate::process::run_process(dir, None).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        println!("manual -> {}/manual.pdf", dir.display());
        return Ok(());
    }
    if let Some(Cmd::Process { dir }) = c.cmd.clone() {
        let dir = crate::process::run_process(std::path::PathBuf::from(dir), None).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        println!("processed -> {}/processed.json", dir.display());
        println!("pdf -> {}/manual.pdf", dir.display());
        return Ok(());
    }
    #[cfg(feature = "nose")]
    if let Some(Cmd::Nose { query, rebuild, subject, dir, lang, limit }) = c.cmd.clone() {        let subject = match subject {
            Some(s) => s,
            None => {
                let dir = crate::store::latest_seek_dir().ok_or_else(|| anyhow::anyhow!("no seeks yet under ~/.cache/magpie/seek/"))?;
                let raw = std::fs::read_to_string(dir.join("seek.json")).map_err(|e| anyhow::anyhow!(e.to_string()))?;
                let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!(e.to_string()))?;
                v.get("query").and_then(|q| q.as_str()).unwrap_or("seek").to_string()
            }
        };
        if rebuild {
            let dirs: Vec<std::path::PathBuf> = dir.into_iter().map(std::path::PathBuf::from).collect();
            let (docs, syns) = crate::nose::rebuild(&subject, &dirs, None).map_err(|e| anyhow::anyhow!(e))?;
            println!("nose: {subject} — {docs} docs, {syns} synonym pairs");
            return Ok(());
        }
        let query = query.ok_or_else(|| anyhow::anyhow!("give a query or --rebuild"))?;
        let nose = crate::nose::Nose::open(&crate::nose::nose_dir(&subject)).map_err(|e| anyhow::anyhow!(e))?;
        let slang = crate::nose::xapian_lang(&lang);
        let hits = nose.search(&query, slang, limit).map_err(|e| anyhow::anyhow!(e))?;
        if hits.is_empty() {
            println!("nose: nothing for {query:?} in {subject}");
        }
        for (i, (w, u, t)) in hits.iter().enumerate() {
            println!("{:>2}. [{:.1}] {}\n    {}", i + 1, w, if t.is_empty() { u.clone() } else { t.clone() }, u);
        }
        for word in query.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() > 3) {
            if let Some(s) = nose.suggest(&word.to_lowercase()) {
                println!("did you mean: {s} (for {word})");
            }
        }
        return Ok(());
    }
    #[cfg(feature = "granary")]
    if let Some(Cmd::Granary { sql, rebuild, dir, limit }) = c.cmd.clone() {
        if rebuild {
            // explicit dirs or the whole cache: every finished run pressed
            let mut dirs: Vec<std::path::PathBuf> = dir.into_iter().map(std::path::PathBuf::from).collect();
            if dirs.is_empty() {
                let base = crate::store::cache_dir().join("seek");
                if let Ok(entries) = std::fs::read_dir(&base) {
                    for e in entries.flatten().filter(|e| e.path().is_dir()) {
                        dirs.push(e.path());
                    }
                }
            }
            let mut tables: std::collections::HashMap<String, usize> = Default::default();
            for d in dirs {
                match crate::granary::press_run(&d) {
                    Ok(rows) => {
                        for (t, n) in rows {
                            *tables.entry(t).or_insert(0) += n;
                        }
                        println!("pressed {}", d.display());
                    }
                    Err(e) => println!("skip {}: {e}", d.display()),
                }
            }
            let mut sum: Vec<(&String, &usize)> = tables.iter().collect();
            sum.sort();
            println!("granary: {}", sum.iter().map(|(t, n)| format!("{t}={n}")).collect::<Vec<_>>().join(" "));
            return Ok(());
        }
        let sql = sql.ok_or_else(|| anyhow::anyhow!("give SQL or --rebuild"))?;
        let (pretty, total) = crate::granary::sql(&sql, limit).await.map_err(|e| anyhow::anyhow!(e))?;
        println!("{pretty}\n({total} rows)");
        return Ok(());
    }
    if let Some(Cmd::Seek { query, out, depth, pages, no_pdf }) = c.cmd {
        let dir = crate::seek::run_seek(query.clone(), depth, pages, out, no_pdf).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        println!("seek done -> {}/seek.json", dir.display());
        if !no_pdf {
            println!("dump pdf -> {}/dump.pdf (raw — run `magpie process` for manual.pdf)", dir.display());
        }
        return Ok(());
    }
    if let Some(Cmd::Rinse { subject, dir, max_dist }) = c.cmd.clone() {
        let dirs: Vec<std::path::PathBuf> = dir.into_iter().map(std::path::PathBuf::from).collect();
        let runs = crate::rinse::collect_runs(&dirs, subject.as_deref());
        let report = crate::rinse::audit(&runs, max_dist);
        print!("{}", crate::rinse::render_report(&report));
        return Ok(());
    }
    // `magpie \"query\"` one-shot
    if let Some(q) = c.query {
        if c.tui {
            crate::tui::run_tui().await?;
            return Ok(());
        }
        let dir = crate::seek::run_seek(q, "deep".into(), if c.deep { 12 } else { 8 }, None, false).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        println!("seek done -> {}/dump.pdf (raw — `magpie process {}` for manual.pdf)", dir.display(), dir.display());
        return Ok(());
    }
    // plain `magpie` -> TUI with prompt box
    crate::tui::run_tui().await?;
    Ok(())
}
