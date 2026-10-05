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
        let wn = w.chars().count();
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

/// Minimal PDF writer (Helvetica, A4). No external PDF crate — OS-friendly.
pub fn render_pdf(query: &str, pages: &[Page], hosts: &[String], out: &std::path::Path) -> Result<()> {
    let mut objects: Vec<Vec<u8>> = vec![];
    // 1 catalog, 2 pages, 3 font, then page+content pairs
    let font_obj = 3usize;

    let mut page_objs: Vec<(usize, usize)> = vec![];
    let mut next_id = 4usize;

    // Build page streams first
    let mut streams: Vec<Vec<u8>> = vec![];
    {
        // cover + one stream per source page (chunked if needed)
        let mut cover = String::new();
        cover.push_str("BT /F1 18 Tf 50 800 Td (magpie seek) Tj ET\n");
        cover.push_str(&format!("BT /F1 13 Tf 50 778 Td ({}) Tj ET\n", esc(&truncate(query, 80))));
        cover.push_str(&format!(
            "BT /F1 9 Tf 50 760 Td ({} pages, {} hosts. Local minimal-PDF build.) Tj ET\n",
            pages.len(),
            hosts.len()
        ));
        cover.push_str(&format!("BT /F1 8 Tf 50 746 Td ({}) Tj ET\n", esc(&truncate(&hosts.join(", "), 110))));
        streams.push(cover.into_bytes());

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
            streams.push(s.into_bytes());
        }
    }

    // page objects: cover + one per source
    let total_pages = streams.len();
    for _ in 0..total_pages {
        let pid = next_id;
        next_id += 1;
        let cid = next_id;
        next_id += 1;
        page_objs.push((pid, cid));
    }

    // header
    let mut pdf: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets: Vec<usize> = vec![0];
    let mut emit = |id: usize, body: &[u8]| {
        offsets.resize(id + 1, 0);
        offsets[id] = pdf.len();
        pdf.extend_from_slice(format!("{id} 0 obj\n").as_bytes());
        pdf.extend_from_slice(body);
        pdf.extend_from_slice(b"\nendobj\n");
    };

    // catalog (1), pages (2), font (3)
    let kids: String = page_objs
        .iter()
        .map(|(pid, _)| format!("{pid} 0 R"))
        .collect::<Vec<_>>()
        .join(" ");
    emit(1, format!("<< /Type /Catalog /Pages 2 0 R >>").as_bytes());
    emit(2, format!("<< /Type /Pages /Kids [{kids}] /Count {} >>", total_pages).as_bytes());
    emit(font_obj, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>");

    for ((pid, cid), stream) in page_objs.iter().zip(streams.iter()) {
        let page_body = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 595 842] /Resources << /Font << /F1 {font_obj} 0 R >> >> /Contents {cid} 0 R >>"
        );
        emit(*pid, page_body.as_bytes());
        let mut cb = format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes();
        cb.extend_from_slice(stream);
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

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
