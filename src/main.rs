mod cli;
mod crawl;
mod error;
mod pdf;
mod seek;
mod store;
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
    if let Some(Cmd::Browse { rest, dump }) = c.cmd.clone() {
        let url = if rest.is_empty() { String::new() } else { rest.split_whitespace().last().unwrap_or("").to_string() };
        if dump && !url.is_empty() {
            let pages = crate::crawl::crawl_urls(&[url], None).await;
            if let Some(p) = pages.first() {
                println!("{}\n\n{}", p.title, p.text.chars().take(6000).collect::<String>());
                return Ok(());
            }
            anyhow::bail!("fetch failed");
        }
    }
    if let Some(Cmd::Seek { query, out, depth, pages, no_pdf }) = c.cmd {
        let dir = crate::seek::run_seek(query.clone(), depth, pages, out, no_pdf).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        println!("seek done -> {}/seek.json", dir.display());
        if !no_pdf {
            println!("pdf -> {}/manual.pdf", dir.display());
        }
        return Ok(());
    }
    // `magpie \"query\"` one-shot
    if let Some(q) = c.query {
        if c.tui {
            crate::tui::run_tui().await?;
            return Ok(());
        }
        let dir = crate::seek::run_seek(q, "deep".into(), if c.deep { 12 } else { 8 }, None, false).await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
        println!("seek done -> {}/manual.pdf", dir.display());
        return Ok(());
    }
    // plain `magpie` -> TUI with prompt box
    crate::tui::run_tui().await?;
    Ok(())
}
