//! House chrome for magpie — `╭─ ✦ Title ✦ ─` frames, no Python at runtime.

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const ITALIC: &str = "\x1b[3m";
pub const DIM: &str = "\x1b[2m";
pub const PINK: &str = "\x1b[38;5;175m";
pub const DARK: &str = "\x1b[38;5;168m";
pub const PINK_DIM: &str = "\x1b[38;5;132m";
pub const BLUSH: &str = "\x1b[38;5;218m";
pub const SILVER: &str = "\x1b[38;5;252m";
pub const MUTED: &str = "\x1b[38;5;245m";
pub const OK: &str = "\x1b[38;5;78m";
pub const WARN: &str = "\x1b[38;5;214m";

pub fn color_ok() -> bool {
    if std::env::var("NO_COLOR").map(|v| !v.trim().is_empty()).unwrap_or(false) {
        return false;
    }
    true
}

pub fn paint(text: &str, codes: &[&str]) -> String {
    if !color_ok() || codes.is_empty() {
        return text.to_string();
    }
    let mut out = String::new();
    for c in codes {
        out.push_str(c);
    }
    out.push_str(text);
    out.push_str(RESET);
    out
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for n in chars.by_ref() {
                    if n.is_ascii_alphabetic() {
                        break;
                    }
                }
                continue;
            }
        }
        out.push(c);
    }
    out
}

fn wide(ch: char) -> bool {
    let o = ch as u32;
    (0x1100..=0x115F).contains(&o)
        || (0x2E80..=0xA4CF).contains(&o)
        || (0xAC00..=0xD7A3).contains(&o)
        || (0xFF00..=0xFF60).contains(&o)
        || (0x1F300..=0x1FAFF).contains(&o)
        || (0x20000..=0x2FFFD).contains(&o)
}

pub fn vis_len(s: &str) -> usize {
    strip_ansi(s).chars().map(|c| if (c as u32) < 32 { 0 } else if wide(c) { 2 } else { 1 }).sum()
}

fn pad_vis(s: &str, width: usize) -> String {
    let n = vis_len(s);
    if n >= width {
        // char-safe cut
        let mut out = String::new();
        let mut w = 0;
        for ch in strip_ansi(s).chars() {
            let cw = if wide(ch) { 2 } else { 1 };
            if w + cw > width.saturating_sub(3).max(1) {
                break;
            }
            out.push(ch);
            w += cw;
        }
        out.push_str("...");
        let m = vis_len(&out);
        if m < width {
            out.push_str(&" ".repeat(width - m));
        }
        return out;
    }
    format!("{s}{}", " ".repeat(width - n))
}

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut lines = vec![];
    for raw in text.split('\n') {
        if raw.trim().is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut cur_w = 0;
        for w in raw.split_whitespace() {
            let wn: usize = w.chars().map(|c| if wide(c) { 2 } else { 1 }).sum();
            if cur_w + wn + 1 > width && !cur.is_empty() {
                lines.push(cur.clone());
                cur.clear();
                cur_w = 0;
            }
            if !cur.is_empty() {
                cur.push(' ');
                cur_w += 1;
            }
            cur.push_str(w);
            cur_w += wn;
        }
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// House frame: `╭─ ✦ Title ✦ ─╮`. Width = outer columns.
pub fn box_frame(body: &[String], title: &str, subtitle: &str, width: usize) -> String {
    let outer = width.clamp(8, 120);
    let inner = outer - 2;
    let body_w = inner.saturating_sub(2).max(1);
    let (tl, tr, bl, br, hz, vt, mark, soft) = ("╭", "╮", "╰", "╯", "─", "│", "✦", "·");
    let acc = PINK_DIM;
    let mut out = vec![];
    if !title.is_empty() {
        let tplain = strip_ansi(title);
        let mut label = format!(" {mark} {tplain} {mark} ");
        if vis_len(&label) > inner.saturating_sub(2) {
            let keep = inner.saturating_sub(8).max(1);
            let cut: String = tplain.chars().take(keep).collect();
            label = format!(" {mark} {cut}… {mark} ");
        }
        let fill = inner.saturating_sub(vis_len(&label)).saturating_sub(1);
        out.push(format!(
            "{}{}{}",
            paint(&format!("{tl}{hz}"), &[acc]),
            paint(&label, &[BOLD, PINK]),
            paint(&format!("{}{tr}", hz.repeat(fill)), &[acc])
        ));
    } else {
        out.push(paint(&format!("{tl}{}{tr}", hz.repeat(inner)), &[acc]));
    }
    if !subtitle.is_empty() {
        for ln in wrap_plain(&strip_ansi(subtitle), body_w) {
            out.push(format!(
                "{} {} {}",
                paint(vt, &[acc]),
                paint(&pad_vis(&ln, body_w), &[ITALIC, DARK]),
                paint(vt, &[acc])
            ));
        }
        out.push(format!(
            "{}{}{}",
            paint(vt, &[acc]),
            paint(&format!(" {} ", soft.repeat(body_w)), &[acc]),
            paint(vt, &[acc])
        ));
    }
    let mut lines_in: Vec<String> = vec![];
    if body.is_empty() {
        lines_in.push(String::new());
    } else {
        for para in body {
            lines_in.extend(wrap_plain(&strip_ansi(para), body_w));
        }
    }
    for ln in lines_in {
        out.push(format!(
            "{} {} {}",
            paint(vt, &[acc]),
            paint(&pad_vis(&ln, body_w), &[SILVER]),
            paint(vt, &[acc])
        ));
    }
    out.push(paint(&format!("{bl}{}{br}", hz.repeat(inner)), &[acc]));
    out.join("\n")
}

/// House signature footer: `─ ✦ evenweaker ✦ ──` centered, dim.
pub fn signature(width: usize) -> String {
    let core = "─ ✦ evenweaker ✦ ──";
    let w = width.clamp(8, 120);
    let pad = w.saturating_sub(vis_len(core)) / 2;
    format!("{}{}", " ".repeat(pad), paint(core, &[DIM, MUTED]))
}
