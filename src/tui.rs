use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Crossterm TUI: prompt box on top, live progress + log. Seek runs in background.
pub async fn run_tui() -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut input = String::new();
    let mut log: Vec<String> = vec!["magpie tui — type query, Enter to seek, Esc to quit".into()];
    let mut last_pdf = String::from("no seek yet");
    let mut seeking: Option<SeekJob> = None;
    let mut tick: usize = 0;

    draw(&mut out, &input, &log, &last_pdf, seeking.as_ref(), tick)?;

    loop {
        // poll with timeout so spinner animates even without keys
        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Esc => break,
                    KeyCode::Enter => {
                        if seeking.is_none() {
                            let q = input.trim().to_string();
                            if !q.is_empty() {
                                let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                                let q2 = q.clone();
                                let handle = tokio::spawn(async move {
                                    crate::seek::run_seek_with_progress(
                                        q2, "deep".into(), 12, None, false, Some(tx),
                                    )
                                    .await
                                    .map_err(|e| e.to_string())
                                });
                                seeking = Some(SeekJob { query: q, rx, handle: Some(handle), status: "starting…".into() });
                                log.push("seek started…".into());
                                input.clear();
                            }
                        }
                    }
                    KeyCode::Char(c) => {
                        if seeking.is_none() {
                            input.push(c);
                        }
                    }
                    KeyCode::Backspace => {
                        if seeking.is_none() {
                            input.pop();
                        }
                    }
                    _ => {}
                }
            }
        }
        tick += 1;

        // drain progress
        if let Some(job) = seeking.as_mut() {
            while let Ok(msg) = job.rx.try_recv() {
                job.status = msg.clone();
                if msg != "done" {
                    log.push(msg);
                }
            }
            let finished = job.handle.as_ref().map(|h| h.is_finished()).unwrap_or(false);
            if finished {
                if let Some(h) = job.handle.take() {
                    match h.await {
                        Ok(Ok(dir)) => {
                            last_pdf = format!("{}/manual.pdf", dir.display());
                            log.push(format!("done -> {last_pdf}"));
                        }
                        Ok(Err(e)) => log.push(format!("error: {e}")),
                        Err(e) => log.push(format!("join error: {e}")),
                    }
                }
                seeking = None;
            }
            // keep log bounded (char-safe: chars, not bytes)
            while log.len() > 200 {
                log.remove(0);
            }
        }

        draw(&mut out, &input, &log, &last_pdf, seeking.as_ref(), tick)?;
    }

    disable_raw_mode()?;
    execute!(out, LeaveAlternateScreen)?;
    Ok(())
}

struct SeekJob {
    query: String,
    rx: tokio::sync::mpsc::UnboundedReceiver<String>,
    handle: Option<tokio::task::JoinHandle<std::result::Result<PathBuf, String>>>,
    status: String,
}

fn draw(
    out: &mut io::Stdout,
    input: &str,
    log: &[String],
    last_pdf: &str,
    seeking: Option<&SeekJob>,
    tick: usize,
) -> anyhow::Result<()> {
    execute!(out, crossterm::terminal::Clear(crossterm::terminal::ClearType::All))?;
    let spin = SPINNER[tick % SPINNER.len()];
    if let Some(job) = seeking {
        writeln!(out, "┌ seek > [{} seeking: {}] ───────────────────", spin, safe_head(&job.query, 40))?;
        writeln!(out, "│ {input} (busy — Esc quits, keys held)")?;
    } else {
        writeln!(out, "┌ seek > ─────────────────────────────────────────────")?;
        writeln!(out, "│ {input}")?;
    }
    writeln!(out, "└─────────────────────────────────────────────────────")?;
    if let Some(job) = seeking {
        writeln!(out, "{} {}", spin, safe_head(&job.status, 100))?;
    }
    writeln!(out, "log:")?;
    for l in log.iter().rev().take(16) {
        writeln!(out, "  {}", safe_head(l, 100))?;
    }
    writeln!(out, "pdf: {last_pdf}")?;
    if seeking.is_some() {
        writeln!(out, "[working… watch status above]  [Esc] quit")?;
    } else {
        writeln!(out, "[Enter] seek  [Esc] quit")?;
    }
    out.flush()?;
    Ok(())
}

fn safe_head(s: &str, n: usize) -> String {
    // char-boundary safe head (fixes is_char_boundary panic class)
    let mut out: String = s.chars().take(n).collect();
    // strip newlines for single-line TUI
    out = out.replace('\n', " ").replace('\r', " ");
    out
}
