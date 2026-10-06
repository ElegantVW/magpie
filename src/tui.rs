use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, PartialEq)]
enum View {
    Log,
    Entries,
}

struct EntryRow {
    kind: String,
    title: String,
    url: String,
    via: String,
    claimed_by: usize,
    section: String,
    bullets: Vec<String>,
    quotes: Vec<String>,
}

fn load_entries(dir: &PathBuf) -> Vec<EntryRow> {
    let raw = std::fs::read_to_string(dir.join("processed.json")).unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::Value::Null);
    v.get("entries")
        .and_then(|e| e.as_array())
        .map(|a| {
            a.iter()
                .map(|e| EntryRow {
                    kind: e.get("kind").and_then(|k| k.as_str()).unwrap_or("other").into(),
                    title: e.get("title").and_then(|k| k.as_str()).unwrap_or("").into(),
                    url: e.get("url").and_then(|k| k.as_str()).unwrap_or("").into(),
                    via: e.get("via").and_then(|k| k.as_str()).unwrap_or("?").into(),
                    claimed_by: e.get("claimed_by").and_then(|k| k.as_u64()).unwrap_or(0) as usize,
                    section: e.get("section").and_then(|k| k.as_str()).unwrap_or("").into(),
                    bullets: e
                        .get("bullets")
                        .and_then(|b| b.as_array())
                        .map(|b| b.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).take(3).collect())
                        .unwrap_or_default(),
                    quotes: e
                        .get("quotes")
                        .and_then(|b| b.as_array())
                        .map(|b| b.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).take(3).collect())
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

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
    let mut last_dir: Option<PathBuf> = None;
    let mut seeking: Option<SeekJob> = None;
    let mut tick: usize = 0;
    let mut seek_start: Option<std::time::Instant> = None;
    let mut last_msg_at: Option<std::time::Instant> = None;
    let mut log_off: usize = 0;
    let mut auto_tail = true;
    let mut last_frame = String::new();
    // entries preview (loaded after `process`)
    let mut view = View::Log;
    let mut entries: Vec<EntryRow> = vec![];
    let mut sel: usize = 0;

    draw(&mut out, &input, cursor, &log, &last_pdf, seeking.as_ref(), tick, seek_start, last_msg_at, log_off, view, &entries, sel, last_dir.is_some(), &mut last_frame)?;

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
                        } else if view == View::Entries {
                            view = View::Log;
                        } else {
                            break;
                        }
                    }
                    KeyCode::Enter => {
                        if view == View::Entries {
                            // open entry: drop bullets + url into log, back to Log
                            if let Some(e) = entries.get(sel) {
                                log.push(format!("[{}|{}] {} — {}", e.kind, e.via, e.section, e.title));
                                for b in &e.bullets {
                                    log.push(format!("  - {b}"));
                                }
                                for q in &e.quotes {
                                    log.push(format!("  \"{q}\""));
                                }
                                log.push(format!("  {}", e.url));
                            }
                            view = View::Log;
                            auto_tail = true;
                            log_off = 0;
                        } else if seeking.is_none() {
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
                                seeking = Some(SeekJob { query: q, rx, handle: Some(handle), status: "starting…".into(), kind: JobKind::Seek });
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
                        if k.modifiers.contains(KeyModifiers::CONTROL) && (c == 'u' || c == 'U') {
                            if seeking.is_none() {
                                input.clear();
                                cursor = 0;
                                hist_i = None;
                            }
                        } else if k.modifiers.contains(KeyModifiers::CONTROL) && (c == 'o' || c == 'O') {
                            // open rune: viewer on last pdf (never blocks the TUI)
                            if last_pdf != "no seek yet" {
                                let pdf = last_pdf.clone();
                                tokio::spawn(async move {
                                    let _ = tokio::process::Command::new("xdg-open")
                                        .arg(&pdf)
                                        .stdin(std::process::Stdio::null())
                                        .stdout(std::process::Stdio::null())
                                        .stderr(std::process::Stdio::null())
                                        .spawn();
                                });
                                log.push(format!("opened {last_pdf}"));
                            } else {
                                log.push("no PDF yet — seek first".into());
                            }
                        } else if c == 'p' && seeking.is_none() && view == View::Log && input.is_empty() && last_dir.is_some() {
                            // pixie pass on last seek dir
                            let dir = last_dir.clone().unwrap();
                            let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                            let handle = tokio::spawn(async move {
                                crate::process::run_process(dir, Some(tx)).await.map_err(|e| e.to_string())
                            });
                            seeking = Some(SeekJob { query: "process".into(), rx, handle: Some(handle), status: "pixie pass…".into(), kind: JobKind::Process });
                            seek_start = Some(std::time::Instant::now());
                            last_msg_at = Some(std::time::Instant::now());
                            log.push("process started… (Esc cancels)".into());
                            auto_tail = true;
                            log_off = 0;
                        } else if c == 'e' && seeking.is_none() && !entries.is_empty() && (view == View::Entries || input.is_empty()) {
                            view = if view == View::Entries { View::Log } else { View::Entries };
                            if view == View::Log {
                                auto_tail = true;
                                log_off = 0;
                            }
                        } else if seeking.is_none() && view == View::Log {
                            let idx = cursor.min(input.chars().count());
                            let byte: usize = input.chars().take(idx).map(|ch| ch.len_utf8()).sum();
                            input.insert(byte, c);
                            cursor += 1;
                            auto_tail = true;
                        } else if view == View::Entries && (c == 'j' || c == 'k') {
                            if !entries.is_empty() {
                                if c == 'j' {
                                    sel = (sel + 1).min(entries.len() - 1);
                                } else {
                                    sel = sel.saturating_sub(1);
                                }
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        if seeking.is_none() && view == View::Log && cursor > 0 {
                            let idx = cursor - 1;
                            let byte: usize = input.chars().take(idx).map(|ch| ch.len_utf8()).sum();
                            input.remove(byte);
                            cursor -= 1;
                        }
                    }
                    KeyCode::Left => {
                        if seeking.is_none() && view == View::Log && cursor > 0 {
                            cursor -= 1;
                        }
                    }
                    KeyCode::Right => {
                        if seeking.is_none() && view == View::Log {
                            cursor = (cursor + 1).min(input.chars().count());
                        }
                    }
                    KeyCode::Up => {
                        if view == View::Entries {
                            sel = sel.saturating_sub(1);
                        } else if seeking.is_none() && !hist.is_empty() {
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
                        if view == View::Entries {
                            if !entries.is_empty() {
                                sel = (sel + 1).min(entries.len() - 1);
                            }
                        } else if seeking.is_none() && hist_i.is_some() {
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
                        if view == View::Entries {
                            sel = sel.saturating_sub(5);
                        } else {
                            auto_tail = false;
                            log_off += 5;
                        }
                    }
                    KeyCode::PageDown => {
                        if view == View::Entries {
                            if !entries.is_empty() {
                                sel = (sel + 5).min(entries.len() - 1);
                            }
                        } else {
                            log_off = log_off.saturating_sub(5);
                            if log_off == 0 {
                                auto_tail = true;
                            }
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
                let was_manual = !matches!(job.kind, JobKind::Seek);
                if let Some(h) = job.handle.take() {
                    match h.await {
                        Ok(Ok(dir)) => {
                            last_dir = Some(dir.clone());
                            if was_manual {
                                last_pdf = format!("{}/manual.pdf", dir.display());
                                entries = load_entries(&dir);
                                sel = 0;
                                let n_llm = entries.iter().filter(|e| e.via == "llm").count();
                                log.push(format!("manual ready: {} entries (llm:{n_llm}) — [e] entries", entries.len()));
                            } else {
                                last_pdf = format!("{}/dump.pdf", dir.display());
                                log.push(format!("dump ready -> {last_pdf} — [p] process for manual.pdf"));
                            }
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

        draw(&mut out, &input, cursor, &log, &last_pdf, seeking.as_ref(), tick, seek_start, last_msg_at, log_off, view, &entries, sel, last_dir.is_some(), &mut last_frame)?;
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
    kind: JobKind,
}

#[derive(Clone, Copy, PartialEq)]
enum JobKind {
    Seek,
    Process,
    Manual,
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
    view: View,
    entries: &[EntryRow],
    sel: usize,
    can_process: bool,
    last_frame: &mut String,
) -> anyhow::Result<()> {
    use crossterm::cursor::MoveTo;
    use crossterm::terminal::ClearType;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let w = ((cols as usize).saturating_sub(2)).clamp(24, 200);
    let h = ((rows as usize).saturating_sub(1)).max(10);
    let spin = SPINNER[(tick / 2) % SPINNER.len()];
    let busy = seeking.is_some();

    let runes_h = 4usize;
    let sig_h = 1usize;
    let gaps = 2usize;
    let prompt_h = 4usize;
    let seek_h = if busy { 5usize } else { 0 };
    let chrome = prompt_h + seek_h + runes_h + sig_h + gaps;
    let log_h = h.saturating_sub(chrome).max(5);
    let max_rows = log_h.saturating_sub(2).max(1);

    let before: String = input.chars().take(cursor).collect();
    let at: String = input.chars().skip(cursor).take(1).collect();
    let after: String = input.chars().skip(cursor + 1).collect();
    let caret_line = if busy {
        format!("{before}{after}  (busy — Esc cancels)")
    } else if at.is_empty() {
        format!("{before}▌")
    } else {
        format!("{before}▌{after}")
    };
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
        let title = match job.kind {
            JobKind::Manual => "Manual",
            JobKind::Process => "Process",
            JobKind::Seek => "Seek",
        };
        frame.push_str(&crate::termart::panel(&body, title, w, Some(seek_h), false));
        frame.push('\n');
    }

    if view == View::Entries && !entries.is_empty() {
        let mut rows_v: Vec<String> = vec![];
        let start = sel.saturating_sub(max_rows - 1).min(sel);
        let end = (start + max_rows).min(entries.len());
        let off = if sel >= max_rows { sel - max_rows + 1 } else { 0 };
        let _ = off;
        for (i, e) in entries.iter().enumerate().skip(start).take(end - start) {
            let mark = if i == sel { "►" } else { "·" };
            let cor = if e.claimed_by > 0 { format!(" ×{}", e.claimed_by + 1) } else { String::new() };
            rows_v.push(format!("{mark} [{}|{}]{cor} {}", e.kind, e.via, safe_head(&e.title, 68)));
        }
        while rows_v.len() > max_rows {
            rows_v.remove(0);
        }
        frame.push_str(&crate::termart::panel(
            &rows_v,
            &format!("Entries {}/{} — Enter opens", sel + 1, entries.len()),
            w,
            Some(log_h),
            true,
        ));
        frame.push('\n');
    } else {
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
        let log_title = if off > 0 { format!("Log · ↑{off}") } else { "Log".to_string() };
        frame.push_str(&crate::termart::panel(&rows_v, &log_title, w, Some(log_h), false));
        frame.push('\n');
    }

    let mut chips = if busy {
        "[working…]  [Esc] cancel".to_string()
    } else {
        "[Enter] seek · [Esc] quit · [↑↓] history · [PgUp/PgDn] scroll · [←→] caret · [Ctrl-U] clear · [Ctrl-O] open pdf".to_string()
    };
    if !busy && can_process {
        chips.push_str(" · [p] process");
    }
    if !entries.is_empty() {
        chips.push_str(" · [e] entries");
    }
    frame.push_str(&crate::termart::panel(&[chips, format!("pdf: {last_pdf}")], "Runes", w, Some(runes_h), false));
    frame.push('\n');
    frame.push_str(&crate::termart::signature(w));

    if frame == *last_frame {
        return Ok(());
    }
    *last_frame = frame.clone();
    execute!(out, MoveTo(0, 0))?;
    execute!(out, crossterm::terminal::Clear(ClearType::FromCursorDown))?;
    // Raw mode disables OPOST: bare \n staircases. Emit CRLF like house tty_write.
    let wire = frame.replace('\n', "\r\n");
    use std::io::Write as _Write;
    out.write_all(wire.as_bytes())?;
    out.write_all(b"\r\n")?;
    out.flush()?;
    Ok(())
}

fn safe_head(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    out = out.replace('\n', " ").replace('\r', " ");
    out
}
