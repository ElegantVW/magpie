use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// House TUI: fixed-height panels, footer-first budget, dirty-only redraw.
pub async fn run_tui() -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    execute!(out, crossterm::cursor::Hide)?;
    let mut input = String::new();
    let mut cursor: usize = 0; // char index into input
    let mut hist: Vec<String> = vec![];
    let mut hist_i: Option<usize> = None;
    let mut log: Vec<String> = vec!["magpie tui — type query, Enter to seek, Esc to quit".into()];
    let mut last_pdf = String::from("no seek yet");
    let mut seeking: Option<SeekJob> = None;
    let mut tick: usize = 0;
    let mut seek_start: Option<std::time::Instant> = None;
    let mut last_msg_at: Option<std::time::Instant> = None;
    let mut log_off: usize = 0;
    let mut auto_tail = true;
    let mut last_frame = String::new();

    draw(&mut out, &input, cursor, &log, &last_pdf, seeking.as_ref(), tick, seek_start, last_msg_at, log_off, &mut last_frame)?;

    loop {
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(k) = event::read()? {
                match k.code {
                    KeyCode::Esc => {
                        if let Some(mut job) = seeking.take() {
                            if let Some(h) = job.handle.take() {
                                h.abort();
                            }
                            log.push("cancelled — back to prompt".into());
                            seeking = None;
                            seek_start = None;
                            last_msg_at = None;
                        } else {
                            break;
                        }
                    }
                    KeyCode::Enter => {
                        if seeking.is_none() {
                            let q = input.trim().to_string();
                            if !q.is_empty() {
                                if hist.last().map(|s| s.as_str()) != Some(q.as_str()) {
                                    hist.push(q.clone());
                                }
                                hist_i = None;
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
                                cursor = 0;
                                auto_tail = true;
                                log_off = 0;
                            }
                        }
                    }
                    KeyCode::Char(c) => {
                        if seeking.is_none() {
                            if k.modifiers.contains(KeyModifiers::CONTROL) && (c == 'u' || c == 'U') {
                                input.clear();
                                cursor = 0;
                                hist_i = None;
                            } else {
                                let idx = cursor.min(input.chars().count());
                                let byte: usize = input.chars().take(idx).map(|ch| ch.len_utf8()).sum();
                                input.insert(byte, c);
                                cursor += 1;
                            }
                            auto_tail = true;
                        }
                    }
                    KeyCode::Backspace => {
                        if seeking.is_none() && cursor > 0 {
                            let idx = cursor - 1;
                            let byte: usize = input.chars().take(idx).map(|ch| ch.len_utf8()).sum();
                            input.remove(byte);
                            cursor -= 1;
                        }
                    }
                    KeyCode::Left => {
                        if seeking.is_none() && cursor > 0 {
                            cursor -= 1;
                        }
                    }
                    KeyCode::Right => {
                        if seeking.is_none() {
                            cursor = (cursor + 1).min(input.chars().count());
                        }
                    }
                    KeyCode::Up => {
                        if seeking.is_none() && !hist.is_empty() {
                            // history when prompt has content or empty: Up recalls
                            let ni = match hist_i {
                                None => hist.len() - 1,
                                Some(i) => i.saturating_sub(1),
                            };
                            hist_i = Some(ni);
                            input = hist[ni].clone();
                            cursor = input.chars().count();
                        } else if seeking.is_some() {
                            auto_tail = false;
                            log_off += 1;
                        }
                    }
                    KeyCode::Down => {
                        if seeking.is_none() && hist_i.is_some() {
                            let i = hist_i.unwrap();
                            if i + 1 >= hist.len() {
                                hist_i = None;
                                input.clear();
                                cursor = 0;
                            } else {
                                hist_i = Some(i + 1);
                                input = hist[i + 1].clone();
                                cursor = input.chars().count();
                            }
                        } else if log_off > 0 {
                            log_off -= 1;
                            if log_off == 0 {
                                auto_tail = true;
                            }
                        }
                    }
                    KeyCode::PageUp => {
                        auto_tail = false;
                        log_off += 5;
                    }
                    KeyCode::PageDown => {
                        log_off = log_off.saturating_sub(5);
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
            while log.len() > 200 {
                log.remove(0);
            }
            if auto_tail {
                log_off = 0;
            }
        }

        draw(&mut out, &input, cursor, &log, &last_pdf, seeking.as_ref(), tick, seek_start, last_msg_at, log_off, &mut last_frame)?;
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

#[allow(clippy::too_many_arguments)]
fn draw(
    out: &mut io::Stdout,
    input: &str,
    cursor: usize,
    log: &[String],
    last_pdf: &str,
    seeking: Option<&SeekJob>,
    tick: usize,
    seek_start: Option<std::time::Instant>,
    last_msg_at: Option<std::time::Instant>,
    log_off: usize,
    last_frame: &mut String,
) -> anyhow::Result<()> {
    use crossterm::cursor::MoveTo;
    use crossterm::terminal::ClearType;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let w = ((cols as usize).saturating_sub(2)).clamp(24, 200);
    let h = ((rows as usize).saturating_sub(1)).max(10);
    let spin = SPINNER[(tick / 2) % SPINNER.len()];
    let busy = seeking.is_some();

    // Fixed heights footer-first: runes 4 + sig 1 + gaps 2, seek 0|5, prompt 4.
    // Log gets the rest; tiny windows shrink log, never chrome.
    let runes_h = 4usize;
    let sig_h = 1usize;
    let gaps = 2usize;
    let prompt_h = 4usize;
    let seek_h = if busy { 5usize } else { 0 };
    let chrome = prompt_h + seek_h + runes_h + sig_h + gaps;
    let log_h = h.saturating_sub(chrome).max(5);
    let max_rows = log_h.saturating_sub(2).max(1);

    // Prompt body: NO literal borders (box_frame draws them). Caret as ▌.
    let before: String = input.chars().take(cursor).collect();
    let at: String = input.chars().skip(cursor).take(1).collect();
    let after: String = input.chars().skip(cursor + 1).collect();
    let caret_line = if busy {
        format!("{before}{after}  (busy — Esc cancels seek)")
    } else if at.is_empty() {
        format!("{before}▌")
    } else {
        format!("{before}▌{after}")
    };
    // focus brightens prompt when idle
    let prompt = crate::termart::panel(
        &[caret_line],
        &format!("Magpie {} seeking", if busy { spin } else { "·" }),
        w,
        Some(prompt_h),
        !busy,
    );

    let mut frame = String::new();
    frame.push_str(&prompt);
    frame.push('\n');

    if let Some(job) = seeking {
        let elapsed = seek_start.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let stale = last_msg_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
        let mut body = vec![format!("{} {} ({}s, update {}s ago)", spin, safe_head(&job.status, 90), elapsed, stale)];
        if stale > 15 {
            body.push("engines may be throttled (DDG/Marginalia). Esc cancels.".into());
        }
        frame.push_str(&crate::termart::panel(&body, "Seek", w, Some(seek_h), false));
        frame.push('\n');
    }

    // Log window with ↑/↓ inside budget
    let total = log.len();
    let off = log_off.min(total.saturating_sub(1).max(0));
    let mut rows_v: Vec<String> = vec![];
    if total == 0 {
        rows_v.push("· idle".into());
    } else if total <= max_rows {
        for l in log {
            rows_v.push(format!("· {}", safe_head(l, 100)));
        }
    } else {
        let need_up = off > 0;
        let mut room = max_rows.saturating_sub(if need_up { 1 } else { 0 });
        let remaining = total.saturating_sub(off);
        if remaining > room {
            room = room.saturating_sub(1).max(1);
        }
        let end = total.saturating_sub(off);
        let start = end.saturating_sub(room);
        if need_up {
            rows_v.push(format!("· ↑ {} above · Up/PgUp ·", off));
        }
        for l in &log[start..end] {
            rows_v.push(format!("· {}", safe_head(l, 100)));
        }
        if end < total {
            rows_v.push(format!("· ↓ {} more · Down resumes tail ·", total - end));
        }
    }
    while rows_v.len() > max_rows {
        rows_v.remove(0);
    }
    let subtitle = if off > 0 { format!("scroll · ↑{} · Down resumes tail", off) } else { String::new() };
    frame.push_str(&crate::termart::panel(&rows_v, "Log", w, Some(log_h), false));
    frame.push('\n');

    let chips = if busy { "[working…]  [Esc] cancel" } else { "[Enter] seek · [Esc] quit · [↑↓] history · [PgUp/PgDn] scroll · [←→] caret · [Ctrl-U] clear" };
    frame.push_str(&crate::termart::panel(&[chips.into(), format!("pdf: {last_pdf}")], "Runes", w, Some(runes_h), false));
    frame.push('\n');
    frame.push_str(&crate::termart::signature(w));

    // Dirty-only: skip write when identical (kills idle flicker).
    // Spinner still steps 2x/sec while seeking, so progress stays live.
    if frame == *last_frame {
        return Ok(());
    }
    *last_frame = frame.clone();
    execute!(out, MoveTo(0, 0))?;
    execute!(out, crossterm::terminal::Clear(ClearType::FromCursorDown))?;
    writeln!(out, "{frame}")?;
    out.flush()?;
    Ok(())
}

fn safe_head(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    out = out.replace('\n', " ").replace('\r', " ");
    out
}
