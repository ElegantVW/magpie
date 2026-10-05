use crate::crawl::Page;
use crate::error::{MagpieError, Result};
use std::fs::File;
use std::io::{BufWriter, Write};

fn esc(s: &str) -> String {
    s.replace('\\', "\\\\").replace('(', "\\(").replace(')', "\\)")
}

fn wrap(s: &str, width: usize) -> Vec<String> {
    // width in chars (not bytes) — multibyte safe
    let mut lines = vec![];
    let mut cur = String::new();
    let mut cur_n: usize = 0;
    for w in s.split_whitespace() {
        let wn: usize = w.chars().count();
        if cur_n + wn + 1 > width && !cur.is_empty() {
            lines.push(cur.clone());
            cur.clear();
            cur_n = 0;
        }
        if !cur.is_empty() {
            cur.push(' ');
            cur_n += 1;
        }
        cur.push_str(w);
        cur_n += wn;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// One entry of a processed manual.
pub struct ManualEntry {
    pub kind: String,
    pub title: String,
    pub url: String,
    pub bullets: Vec<String>,
    pub specs: Vec<[String; 2]>,
}

struct Line {
    text: String,
    size: f32,
    bold: bool,
    units: usize, // vertical budget
}

fn h1(doc: &mut Vec<Line>, sec_no: &mut usize, title: &str) {
    *sec_no += 1;
    doc.push(Line { text: String::new(), size: 8.0, bold: false, units: 1 });
    doc.push(Line { text: format!("{}. {}", sec_no, title), size: 13.0, bold: true, units: 3 });
}

fn h2(doc: &mut Vec<Line>, text: &str) {
    doc.push(Line { text: text.to_string(), size: 10.0, bold: true, units: 2 });
}

fn body(doc: &mut Vec<Line>, text: &str) {
    for ln in wrap(text, 95) {
        doc.push(Line { text: ln, size: 8.5, bold: false, units: 1 });
    }
}

fn bullets(doc: &mut Vec<Line>, items: &[String]) {
    for b in items {
        for (i, ln) in wrap(b, 92).iter().enumerate() {
            let t = if i == 0 { format!("• {ln}") } else { format!("  {ln}") };
            doc.push(Line { text: t, size: 8.5, bold: false, units: 1 });
        }
    }
}

fn spec_table(doc: &mut Vec<Line>, specs: &[[String; 2]]) {
    for [k, v] in specs {
        let row = format!("{:<28} {}", truncate(k, 28), truncate(v, 60));
        for ln in wrap(&row, 95) {
            doc.push(Line { text: ln, size: 8.0, bold: false, units: 1 });
        }
    }
}

fn callout(doc: &mut Vec<Line>, label: &str, text: &str) {
    let bar = "+-- ".to_string() + label + &"-".repeat(90usize.saturating_sub(label.len() + 5)) + "+";
    doc.push(Line { text: bar, size: 8.0, bold: true, units: 1 });
    for ln in wrap(text, 90) {
        doc.push(Line { text: format!("| {ln}"), size: 8.0, bold: false, units: 1 });
    }
    doc.push(Line { text: "+".to_string() + &"-".repeat(93) + "+", size: 8.0, bold: true, units: 1 });
}

/// Structured manual: cover + TOC + grouped sections + sources.
/// Groups follow the Corsa-manual order: spec, fault, review, dimensions, other.
pub fn render_manual(query: &str, entries: &[ManualEntry], hosts: &[String], out: &std::path::Path) -> Result<()> {
    let order = ["spec", "fault", "review", "dimensions", "other"];
    let titles = [
        "Specifications",
        "Faults & reliability",
        "Reviews & driving",
        "Dimensions & practicality",
        "Other sources",
    ];
    let grouped: Vec<(usize, Vec<&ManualEntry>)> = order
        .iter()
        .enumerate()
        .map(|(i, k)| (i, entries.iter().filter(|e| e.kind == *k).collect()))
        .collect();

    // Pass 1: build content lines, record section start line index.
    let mut sec_no = 0usize;
    let mut doc: Vec<Line> = vec![];
    let mut sec_starts: Vec<(String, usize)> = vec![];
    for (i, group) in grouped.iter() {
        if group.is_empty() {
            continue;
        }
        sec_starts.push((titles[*i].to_string(), doc.len()));
        h1(&mut doc, &mut sec_no, titles[*i]);
        for (n, e) in group.iter().enumerate() {
            h2(&mut doc, &format!("[{}] {}", n + 1, truncate(&e.title, 85)));
            body(&mut doc, &format!("Source: {}", e.url));
            if !e.bullets.is_empty() {
                bullets(&mut doc, &e.bullets);
            }
            if !e.specs.is_empty() {
                spec_table(&mut doc, &e.specs);
            }
        }
        if titles[*i].starts_with("Faults") {
            callout(&mut doc, "WARNING", "Confirm torques, clearances and fluid specs against workshop data for your VIN before work.");
        }
    }
    if entries.is_empty() {
        h1(&mut doc, &mut sec_no, "No entries");
        body(&mut doc, "The pixie pass produced no entries. See seek.json for raw pages.");
    }
    // Sources section
    sec_starts.push(("Sources".into(), doc.len()));
    h1(&mut doc, &mut sec_no, "Sources");
    for (n, e) in entries.iter().enumerate() {
        body(&mut doc, &format!("{}. [{}] {} — {}", n + 1, e.kind, truncate(&e.title, 70), e.url));
    }

    // Pass 2: paginate content (50 units/page), map sections to pages.
    const UNITS: usize = 50;
    let mut content_pages: Vec<Vec<&Line>> = vec![];
    let mut cur: Vec<&Line> = vec![];
    let mut used = 0usize;
    // line index -> content page number (1-based, excluding cover+TOC)
    let mut line_page: Vec<usize> = vec![0; doc.len()];
    for (idx, ln) in doc.iter().enumerate() {
        if used + ln.units > UNITS && !cur.is_empty() {
            content_pages.push(cur);
            cur = vec![];
            used = 0;
        }
        line_page[idx] = content_pages.len() + 1; // 1-based within content
        cur.push(ln);
        used += ln.units;
    }
    if !cur.is_empty() {
        content_pages.push(cur);
    }

    // TOC page lines (content page numbers offset by cover+TOC = +2)
    let mut toc: Vec<Line> = vec![];
    for (name, start) in &sec_starts {
        let pg = line_page[*start] + 2;
        toc.push(Line { text: format!("{:<60} p.{pg}", truncate(name, 60)), size: 9.0, bold: false, units: 1 });
    }
    if toc.is_empty() {
        toc.push(Line { text: "(empty)".into(), size: 9.0, bold: false, units: 1 });
    }

    // Emit: cover stream, TOC stream, then content streams.
    let mut streams: Vec<String> = vec![];
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let mut cover = String::new();
    cover.push_str("BT /F1 18 Tf 50 800 Td (magpie manual) Tj ET\n");
    cover.push_str(&format!("BT /F1 13 Tf 50 776 Td ({}) Tj ET\n", esc(&truncate(query, 80))));
    cover.push_str(&format!("BT /F1 9 Tf 50 758 Td ({} entries, {} hosts, {}) Tj ET\n", entries.len(), hosts.len(), date));
    cover.push_str(&format!("BT /F1 8 Tf 50 742 Td ({}) Tj ET\n", esc(&truncate(&hosts.join(", "), 110))));
    cover.push_str("BT /F1 8 Tf 50 716 Td (Grouped spec / fault / review / dimensions. Verify critical data by VIN.) Tj ET\n");
    streams.push(cover);
    let mut tocs = String::new();
    tocs.push_str("BT /F1 13 Tf 50 800 Td (Contents) Tj ET\n");
    let mut y = 780i32;
    for ln in &toc {
        tocs.push_str(&format!("BT /F1 9 Tf 50 {} Td ({}) Tj ET\n", y, esc(&truncate(&ln.text, 110))));
        y -= 14;
    }
    streams.push(tocs);
    for page in &content_pages {
        let mut s = String::new();
        let mut y = 800i32;
        for ln in page {
            let op = if ln.bold { "BT /F1 11 Tf" } else { &format!("BT /F1 {} Tf", ln.size) };
            // h1 already bold 13 via op override below
            let size_op = if ln.units >= 3 { "BT /F1 13 Tf" } else if ln.bold { "BT /F1 10 Tf" } else { &format!("BT /F1 {} Tf", ln.size) };
            let _ = op;
            s.push_str(&format!("{size_op} 50 {y} Td ({}) Tj ET\n", esc(&truncate(&ln.text, 115))));
            y -= if ln.units >= 3 { 20 } else if ln.bold { 16 } else { 13 };
            if y < 60 {
                break;
            }
        }
        streams.push(s);
    }

    emit_pdf(&streams, &format!("magpie manual — {query}"), out)
}

/// Minimal PDF writer (Helvetica, A4). No external PDF crate — OS-friendly.
pub fn render_pdf(query: &str, pages: &[Page], hosts: &[String], out: &std::path::Path) -> Result<()> {
    // Quick dump used by `seek` (no pixie pass yet): title + excerpts.
    let mut streams: Vec<String> = vec![];
    {
        let mut cover = String::new();
        cover.push_str("BT /F1 18 Tf 50 800 Td (magpie seek) Tj ET\n");
        cover.push_str(&format!("BT /F1 13 Tf 50 778 Td ({}) Tj ET\n", esc(&truncate(query, 80))));
        cover.push_str(&format!(
            "BT /F1 9 Tf 50 760 Td ({} pages, {} hosts. Local minimal-PDF build.) Tj ET\n",
            pages.len(),
            hosts.len()
        ));
        cover.push_str(&format!("BT /F1 8 Tf 50 746 Td ({}) Tj ET\n", esc(&truncate(&hosts.join(", "), 110))));
        cover.push_str("BT /F1 8 Tf 50 730 Td (Raw crawl dump. Run `magpie process DIR` for the structured manual.) Tj ET\n");
        streams.push(cover);

        for p in pages {
            let mut s = String::new();
            let title = if p.title.is_empty() { &p.url } else { &p.title };
            s.push_str(&format!("BT /F1 11 Tf 50 800 Td ({}) Tj ET\n", esc(&truncate(title, 90))));
            s.push_str(&format!("BT /F1 7 Tf 50 786 Td ({}) Tj ET\n", esc(&truncate(&p.url, 110))));
            let mut y = 768i32;
            let ex: String = p.text.chars().take(1800).collect();
            for line in wrap(&ex, 95).into_iter().take(38) {
                s.push_str(&format!("BT /F1 8 Tf 50 {} Td ({}) Tj ET\n", y, esc(&truncate(&line, 120))));
                y -= 14;
                if y < 60 {
                    break;
                }
            }
            streams.push(s);
        }
    }

    emit_pdf(&streams, &format!("magpie seek — {query}"), out)
}

fn emit_pdf(streams: &[String], title: &str, out: &std::path::Path) -> Result<()> {
    let font_obj = 3usize;
    let mut page_objs: Vec<(usize, usize)> = vec![];
    let mut next_id = 4usize;
    let total_pages = streams.len().max(1);
    for _ in 0..total_pages {
        let pid = next_id;
        next_id += 1;
        let cid = next_id;
        next_id += 1;
        page_objs.push((pid, cid));
    }

    let _ = title;
    let mut pdf: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets: Vec<usize> = vec![0];
    let mut emit = |id: usize, body: &[u8]| {
        offsets.resize(id + 1, 0);
        offsets[id] = pdf.len();
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    };

    let kids: String = page_objs
        .iter()
        .map(|(pid, _)| format!("{pid} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    emit(1, b"<< /Type /Catalog /Pages 2 0 R >>");
    emit(2, format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", total_pages).as_bytes());
    emit(font_obj, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");

    // footer helper draws title + page number at bottom of each stream
    for (i, ((pid, cid), stream)) in page_objs.iter().zip(streams.iter()).enumerate() {
        let page_body = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 {font_obj} 0 R >> >> /Contents {cid} 0 R >>"
        );
        emit(*pid, page_body.as_bytes());
        let with_footer = format!("{stream}BT /F1 7 Tf 50 30 Td (p.{} of {}) Tj ET\n", i + 1, total_pages);
        let mut cb = format!("<< /Length {} >>\nstream\n", with_footer.len()).into_bytes();
        cb.extend_from_slice(with_footer.as_bytes());
        cb.extend_from_slice(b"\nendstream");
        emit(*cid, &cb);
    }

    let xref_pos = pdf.len();
    pdf.extend_from_slice(format!("xref\n0 {}\n", next_id).as_bytes());
    pdf.extend_from_slice(b"0000000000 65535 f \n");
    for id in 1..next_id {
        let off = if id < offsets.len() { offsets[id] } else { 0 };
        pdf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    pdf.extend_from_slice(
        format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_pos}\n%%EOF", next_id).as_bytes(),
    );

    let f = File::create(out).map_err(|e| MagpieError::Io(e.to_string()))?;
    let mut w = BufWriter::new(f);
    w.write_all(&pdf).map_err(|e| MagpieError::Io(e.to_string()))?;
    Ok(())
}
