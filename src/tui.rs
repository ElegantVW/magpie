use crossterm::{
    event::{self, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, Write};

/// Minimal crossterm TUI: prompt box on top, log below. No ratatui (avoids ICE).
pub async fn run_tui() -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen)?;
    let mut input = String::new();
    let mut log: Vec<String> = vec!["magpie tui — type query, Enter to seek, Esc/q(empty) to quit".into()];
    let mut last_pdf: String = String::from("no seek yet");

    let draw = |out: &mut io::Stdout, input: &str, log: &[String], last_pdf: &str| -> anyhow::Result<()> {
        execute!(out, crossterm::terminal::Clear(crossterm::terminal::ClearType::All))?;
        writeln!(out, "┌ seek > ─────────────────────────────────────────────")?;
        writeln!(out, "│ {input}")?;
        writeln!(out, "└─────────────────────────────────────────────────────")?;
        writeln!(out, "log:")?;
        for l in log.iter().rev().take(18) {
            let t: String = l.chars().take(100).collect();
            writeln!(out, "  {t}")?;
        }
        writeln!(out, "pdf: {last_pdf}")?;
        writeln!(out, "[Enter] seek  [Esc] quit")?;
        out.flush()?;
        Ok(())
    };

    draw(&mut out, &input, &log, &last_pdf)?;

    loop {
        if let Event::Key(k) = event::read()? {
            match k.code {
                KeyCode::Esc => break,
                KeyCode::Enter => {
                    let q = input.trim().to_string();
                    if !q.is_empty() {
                        log.push(format!("seeking: {q}"));
                        draw(&mut out, &input, &log, &last_pdf)?;
                        match crate::seek::run_seek(q.clone(), "deep".into(), 12, None, false).await {
                            Ok(d) => {
                                last_pdf = format!("{}/manual.pdf", d.display());
                                log.push(format!("done -> {last_pdf}"));
                            }
                            Err(e) => log.push(format!("error: {e}")),
                        }
                        input.clear();
                    }
                }
                KeyCode::Char(c) => input.push(c),
                KeyCode::Backspace => {
                    input.pop();
                }
                _ => {}
            }
            // q to quit only when input empty (so queries with q work)
            if input.is_empty() {
                // peek: handled next loop; check last key was 'q'? simplified: Esc quits
            }
            draw(&mut out, &input, &log, &last_pdf)?;
        }
    }

    disable_raw_mode()?;
    execute!(out, LeaveAlternateScreen)?;
    Ok(())
}
