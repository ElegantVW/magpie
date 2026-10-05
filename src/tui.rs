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
    execute!(out, crossterm::cursor::Hide)?;
    let mut input = String::new();
    let mut log: Vec<String> = vec!["magpie tui — type query, Enter to seek, Esc to quit".into()];
    let mut last_pdf = String::from("no seek yet");
    let mut seeking: Option<SeekJob> = None;
    let mut tick: usize = 0;
    let mut seek_start: Option<std::time::Instant> = None;
    let mut last_msg_at: Option<std::time::Instant> = None;
    let mut log_off: usize = 0; // scroll offset into log (0 = tail)
    let mut auto_tail = true;

    draw(&mut out, &input, &log, &last_pdf, seeking.as_ref(), tick, seek_start, last_msg_at, log_off)?;

    loop {
        // 250ms tick: calmer than 120ms full-clear (flicker fix).
        // Spinner advances every other tick so glyph doesn't race.
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Esc => {
                        if let Some(mut job) = seeking.take() {
                            if let Some(h) = job.handle.take() {
                                h.abort();
                            }
                            log.push("cancelled — back to prompt".into());
                            // job dropped here; next draw shows idle
                            seeking = None;
                        } else {
                            break;
                        }
                    }
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
                                seek_start = Some(std::time::Instant::now());
                                last_msg_at = Some(std::time::Instant::now());
                                log.push("seek started… (Esc cancels seek)".into());
                                input.clear();
                            }
                        }
                    }
                    KeyCode::Char(c) => {
                        if seeking.is_none() {
                            input.push(c);
                            auto_tail = true;
                        }
                    }
                    KeyCode::Backspace => {
                        if seeking.is_none() {
                            input.pop();
                        }
                    }
                    KeyCode::Up | KeyCode::PageUp => {
                        auto_tail = false;
                        log_off += 1;
                    }
                    KeyCode::Down | KeyCode::PageDown => {
                        if log_off > 0 {
                            log_off -= 1;
                        }
                        if log_off == 0 {
                            auto_tail = true;
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
                last_msg_at = Some(std::time::Instant::now());
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
                seek_start = None;
                last_msg_at = None;
            }
            // keep log bounded (char-safe: chars, not bytes)
            while log.len() > 200 {
                log.remove(0);
            }
            if auto_tail {
                log_off = 0;
            }
        }

        draw(&mut out, &input, &log, &last_pdf, seeking.as_ref(), tick, seek_start, last_msg_at, log_off)?;
    }

    disable_raw_mode()?;
    execute!(out, crossterm::cursor::Show)?;
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
    seek_start: Option<std::time::Instant>,
    last_msg_at: Option<std::time::Instant>,
    log_off: usize,
) -> anyhow::Result<()> {
    // House chrome: `╭─ ✦ Magpie ✦ ─` frames, adaptive like scroll/pixie.
    // Width from raw winsize each frame (no 96 cap for TUI); height budgets
    // footer-first so small windows clip log, never chrome.
    use crossterm::cursor::MoveTo;
    use crossterm::terminal::ClearType;
    execute!(out, MoveTo(0, 0))?;
    execute!(out, crossterm::terminal::Clear(ClearType::FromCursorDown))?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let w = ((cols as usize).saturating_sub(2)).clamp(24, 200);
    let h = ((rows as usize).saturating_sub(1)).max(10);
    let spin = SPINNER[(tick / 2) % SPINNER.len()];

    let prompt_body = if seeking.is_some() {
        vec![format!("│ {input} (busy — Esc cancels seek)")]
    } else {
        vec![format!("│ {input}")]
    };
    let prompt = crate::termart::box_frame(
        &prompt_body,
        &format!("Magpie {} seeking", if seeking.is_some() { spin } else { "·" }),
        "thorough seeker — type query, Enter seeks",
        w,
    );
    writeln!(out, "{prompt}")?;

    if let Some(job) = seeking {
        let elapsed = seek_start.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let stale = last_msg_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let status = format!("{} {} ({}s, update {}s ago)", spin, safe_head(&job.status, 90), elapsed, stale);
        let extra = if stale > 15 {
            "engines may be throttled (DDG/Marginalia). Esc cancels."
        } else {
            ""
        };
        let body = if extra.is_empty() { vec![status] } else { vec![status, extra.into()] };
        writeln!(out, "{}", crate::termart::box_frame(&body, "Seek", &format!("query: {}", safe_head(&job.query, 60)), w))?;
    }

    // Height budget footer-first (scroll pattern): prompt + seek + runes +
    // signature get fixed chrome, log gets the rest with ↑/↓ window.
    // box_frame heights: prompt 4 (top+2 body+bottom w/ subtitle rule),
    // seek 4-5, runes 4, signature 1, gaps 3.
    let prompt_h = 5usize;
    let seek_h = if seeking.is_some() { 5usize } else { 0 };
    let runes_h = 4usize;
    let sig_h = 1usize;
    let gaps = 3usize;
    let chrome = prompt_h + seek_h + runes_h + sig_h + gaps;
    let log_h = h.saturating_sub(chrome).max(5);
    // inner rows inside log borders
    let max_rows = log_h.saturating_sub(2).max(1);

    let total = log.len();
    let off = log_off.min(total.saturating_sub(1).max(0));
    // window with ↑/↓ indicators inside budget
    let mut rows: Vec<String> = vec![];
    if total == 0 {
        rows.push("· idle".into());
    } else if total <= max_rows {
        for l in log {
            rows.push(format!("· {}", safe_head(l, 100)));
        }
    } else {
        let need_up = off > 0;
        let mut room = max_rows.saturating_sub(if need_up { 1 } else { 0 });
        let remaining = total.saturating_sub(off);
        let need_down = remaining > room;
        if need_down {
            room = room.saturating_sub(1).max(1);
        }
        let need_down = off + room < total;
        // slice from tail backwards by off
        let end = total.saturating_sub(off);
        let start = end.saturating_sub(room);
        if need_up {
            rows.push(format!("· ↑ {} above · Up/PgUp ·", off));
        }
        for l in &log[start..end] {
            rows.push(format!("· {}", safe_head(l, 100)));
        }
        if need_down {
            rows.push(format!("· ↓ {} more · Down keeps tail ·", total - end));
        }
    }
    // hard cap: never exceed frame height (drop from top of log window)
    while rows.len() > max_rows {
        rows.remove(0);
    }
    let subtitle = if off > 0 { format!("scroll · ↑{} · Down resumes tail", off) } else { String::new() };
    writeln!(out, "{}", crate::termart::box_frame(&rows, "Log", &subtitle, w))?;

    let runes = if seeking.is_some() {
        "[working… Esc cancels seek, prompt held]"
    } else {
        "[Enter] seek · [Esc] quit"
    };
    writeln!(out, "{}", crate::termart::box_frame(&[runes.into(), format!("pdf: {last_pdf}")], "Runes", "", w))?;
    writeln!(out, "{}", crate::termart::signature(w))?;
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
