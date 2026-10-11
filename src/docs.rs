//! Document retrieval: when the crawl meets a catalog, datasheet, spreadsheet
//! or book file, it downloads and extracts it instead of ingesting mojibake.
//! Pure-Rust parsers only (pdf-extract, zip, calamine) — no system deps,
//! no daemons, no executables ever touched. Archives (zip/rar/tar) are
//! REFUSED outright: we extract text from document formats, never unpack
//! arbitrary archives (zip bombs live there).
//!
//! Budgets (house rule: TBs spare, politeness first):
//! - download cap 20MB per file (content-length gate + stream abort)
//! - extracted text cap 400K chars (uniform with IA book chunks downstream)
//! - second-hop admits ≤8 doc links per run (catalogs with 200 PDFs exist)

use std::io::{Cursor, Read};

/// Download ceiling per file (20MB). Above it: refuse as oversize.
pub const DOC_MAX_BYTES: u64 = 20 * 1024 * 1024;
/// Extracted text ceiling (chars). Uniform with book chunking downstream.
pub const DOC_MAX_CHARS: usize = 400_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocKind {
    Pdf,
    Epub,
    Docx,
    Xlsx,
    Xls,
    Ods,
    Csv,
    Txt,
}

/// Classify by URL extension (query/fragment stripped). Unknown → None
/// (fetched as a page, as before). Archives are never Some.
pub fn classify_url(url: &str) -> Option<DocKind> {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    let ext = base.rsplit('.').next().unwrap_or("").to_lowercase();
    // guard: extension must actually be trailing (rsplit on "a.b/c" gives "c")
    if !base.to_lowercase().ends_with(&format!(".{ext}")) || ext.contains('/') {
        return None;
    }
    match ext.as_str() {
        "pdf" => Some(DocKind::Pdf),
        "epub" => Some(DocKind::Epub),
        "docx" => Some(DocKind::Docx),
        "xlsx" => Some(DocKind::Xlsx),
        "xls" => Some(DocKind::Xls),
        "ods" => Some(DocKind::Ods),
        "csv" => Some(DocKind::Csv),
        "txt" | "text" | "md" | "markdown" => Some(DocKind::Txt),
        _ => None,
    }
}

/// Classify by response content-type (server truth beats URL guess).
pub fn classify_ctype(ctype: &str) -> Option<DocKind> {
    let c = ctype.to_lowercase();
    if c.contains("application/pdf") {
        Some(DocKind::Pdf)
    } else if c.contains("application/epub") {
        Some(DocKind::Epub)
    } else if c.contains("officedocument.wordprocessingml") {
        Some(DocKind::Docx)
    } else if c.contains("officedocument.spreadsheetml") {
        Some(DocKind::Xlsx)
    } else if c.contains("vnd.ms-excel") {
        Some(DocKind::Xls)
    } else if c.contains("vnd.oasis.opendocument.spreadsheet") {
        Some(DocKind::Ods)
    } else if c.contains("text/csv") {
        Some(DocKind::Csv)
    } else if c.contains("text/plain") {
        Some(DocKind::Txt)
    } else {
        None
    }
}

/// Display suffix for titles ("corsa-c-datasheet (PDF)").
pub fn kind_suffix(kind: DocKind) -> &'static str {
    match kind {
        DocKind::Pdf => "PDF",
        DocKind::Epub => "EPUB",
        DocKind::Docx => "DOCX",
        DocKind::Xlsx => "XLSX",
        DocKind::Xls => "XLS",
        DocKind::Ods => "ODS",
        DocKind::Csv => "CSV",
        DocKind::Txt => "TXT",
    }
}

/// Title from URL file stem (".../corsa-c-datasheet.pdf" → "corsa-c-datasheet (PDF)").
/// Bare-numeric stems (arXiv "2110", "2405") fall back to the hint when
/// given — a paper ID is not a title (the "2110 (PDF)" manual entry was).
pub fn title_for(url: &str, kind: DocKind) -> String {
    title_for_hint(url, kind, "")
}

pub fn title_for_hint(url: &str, kind: DocKind, hint: &str) -> String {
    let base = url.split(['?', '#']).next().unwrap_or(url);
    let stem = base
        .rsplit('/')
        .next()
        .unwrap_or("document")
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or("document");
    // numeric-only stems name nothing: prefer the caller's real title
    if !hint.is_empty() && stem.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 'v') {
        return format!("{} ({})", hint.chars().take(80).collect::<String>(), kind_suffix(kind));
    }
    let clean: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect::<String>()
        .chars()
        .take(80)
        .collect();
    format!("{} ({})", clean.trim_matches('-'), kind_suffix(kind))
}

/// Extract text from downloaded bytes. Magic bytes verified before parsing
/// (servers mislabel; a lying content-type must not choose the parser).
/// Output capped at DOC_MAX_CHARS (uniform with book chunks).
pub fn extract(kind: DocKind, bytes: &[u8]) -> Result<String, String> {
    if bytes.len() as u64 > DOC_MAX_BYTES {
        return Err(format!("oversize:{}b", bytes.len()));
    }
    let text = match kind {
        DocKind::Pdf => {
            if !bytes.starts_with(b"%PDF") {
                return Err("not-pdf-bytes".into());
            }
            pdf_extract::extract_text_from_mem(bytes).map_err(|e| format!("pdf:{e:?}"))?
        }
        DocKind::Epub => extract_epub(bytes)?,
        DocKind::Docx => extract_docx(bytes)?,
        DocKind::Xlsx | DocKind::Xls | DocKind::Ods => extract_sheet(kind, bytes)?,
        DocKind::Csv => extract_csv(bytes),
        DocKind::Txt => String::from_utf8_lossy(bytes).into_owned(),
    };
    let text: String = text.chars().take(DOC_MAX_CHARS).collect();
    // faithful conversion only: emptiness is the only refusal here.
    // Thin-content hygiene (<200 chars) lives at the crawl layer, where
    // the fetch context (URL, leg, budget) exists to judge it.
    if text.trim().is_empty() {
        return Err("doc-empty".into());
    }
    Ok(text)
}

fn zip_texts(bytes: &[u8], want: &[&str], cap_files: usize) -> Result<String, String> {
    if !bytes.starts_with(b"PK\x03\x04") && !bytes.starts_with(b"PK\x05\x06") && !bytes.starts_with(b"PK\x07\x08") {
        return Err("not-zip-bytes".into());
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("zip:{e}"))?;
    // bomb guard: refuse absurd member counts / sizes before reading
    if zip.len() > 500 {
        return Err("zip-too-many-members".into());
    }
    let mut names: Vec<String> = vec![];
    for i in 0..zip.len() {
        let f = zip.by_index(i).map_err(|e| format!("zip:{e}"))?;
        // declared size gate (before decompression spend)
        if f.size() > DOC_MAX_BYTES {
            return Err("zip-member-oversize".into());
        }
        let n = f.name().to_string();
        if want.iter().any(|w| {
            if w.starts_with('.') {
                n.ends_with(w)
            } else {
                n == *w
            }
        }) {
            names.push(n);
        }
    }
    names.sort();
    names.truncate(cap_files);
    if names.is_empty() {
        return Err("zip-no-content".into());
    }
    let mut out = String::new();
    for n in names {
        let mut f = zip.by_name(&n).map_err(|e| format!("zip:{e}"))?;
        let mut s = String::new();
        f.read_to_string(&mut s).map_err(|e| format!("zip:{e}"))?;
        out.push_str(&s);
        out.push('\n');
        if out.len() as u64 > DOC_MAX_BYTES {
            break;
        }
    }
    Ok(out)
}

/// EPUB: xhtml/html content docs, name-sorted (spine order is ideal;
/// name sort approximates it and never fails on odd packages).
fn extract_epub(bytes: &[u8]) -> Result<String, String> {
    let raw = zip_texts(bytes, &[".xhtml", ".html", ".htm"], 60)?;
    Ok(strip_xml_text(&raw))
}

/// DOCX: word/document.xml word-runs.
fn extract_docx(bytes: &[u8]) -> Result<String, String> {
    let raw = zip_texts(bytes, &["word/document.xml"], 1)?;
    // <w:t> runs carry the text (xml escaped); block ends break lines
    let mut out = raw.replace("</w:p>", "\n").replace("</w:tr>", "\n");
    out = strip_xml_text(&out);
    Ok(out)
}

fn strip_xml_text(s: &str) -> String {
    let re_tags = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    let t = re_tags.replace_all(s, " ");
    let t = t.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&").replace("&quot;", "\"").replace("&nbsp;", " ");
    let re_ws = regex::Regex::new(r"[ \t]+").unwrap();
    re_ws.replace_all(t.trim(), " ").to_string()
}

/// Spreadsheets via calamine: every sheet, capped rows/cols/cells.
/// Numbers, dates and bools render plainly (a spec table must survive).
fn extract_sheet(kind: DocKind, bytes: &[u8]) -> Result<String, String> {
    const MAX_ROWS: usize = 2000;
    const MAX_COLS: usize = 20;
    let mut out = String::new();
    // calamine reads from Read+Seek; Cursor<&[u8]> qualifies, no temp files
    macro_rules! dump {
        ($wb:expr) => {{
            use calamine::Reader;
            for (sheet, range) in $wb.worksheets() {
                out.push_str(&format!("\n[{sheet}]\n"));
                for (ri, row) in range.rows().enumerate() {
                    if ri >= MAX_ROWS {
                        break;
                    }
                    let cells: Vec<String> = row
                        .iter()
                        .take(MAX_COLS)
                        .map(|c| match c {
                            calamine::Data::String(s) => s.clone(),
                            calamine::Data::Float(f) => {
                                if f.fract() == 0.0 {
                                    format!("{}", *f as i64)
                                } else {
                                    format!("{f}")
                                }
                            }
                            calamine::Data::Int(i) => format!("{i}"),
                            calamine::Data::Bool(b) => format!("{b}"),
                            calamine::Data::DateTime(d) => format!("{}", d.as_f64()),
                            calamine::Data::DateTimeIso(s) => s.clone(),
                            calamine::Data::DurationIso(s) => s.clone(),
                            calamine::Data::Error(e) => format!("{e:?}"),
                            calamine::Data::Empty => String::new(),
                        })
                        .collect();
                    // skip fully-empty rows (sparse sheets pad forever)
                    if cells.iter().all(|c| c.trim().is_empty()) {
                        continue;
                    }
                    out.push_str(&cells.join(" | "));
                    out.push('\n');
                    if out.len() as u64 > DOC_MAX_BYTES {
                        break;
                    }
                }
            }
        }};
    }
    match kind {
        DocKind::Xlsx => {
            use calamine::Reader;
            let mut wb: calamine::Xlsx<_> =
                Reader::new(Cursor::new(bytes)).map_err(|e| format!("xlsx:{e}"))?;
            dump!(wb);
        }
        DocKind::Xls => {
            use calamine::Reader;
            let mut wb: calamine::Xls<_> =
                Reader::new(Cursor::new(bytes)).map_err(|e| format!("xls:{e}"))?;
            dump!(wb);
        }
        DocKind::Ods => {
            use calamine::Reader;
            let mut wb: calamine::Ods<_> =
                Reader::new(Cursor::new(bytes)).map_err(|e| format!("ods:{e}"))?;
            dump!(wb);
        }
        _ => return Err("not-a-sheet".into()),
    }
    Ok(out)
}

/// CSV: lines × cells, capped. Separator sniffed from the header (; wins
/// over , when more frequent — European sheets use semicolons).
fn extract_csv(bytes: &[u8]) -> String {
    const MAX_ROWS: usize = 2000;
    const MAX_COLS: usize = 20;
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.lines();
    let sep = match lines.clone().next() {
        Some(h) if h.matches(';').count() > h.matches(',').count() => ';',
        _ => ',',
    };
    let mut out = String::new();
    for (i, line) in lines.enumerate() {
        if i >= MAX_ROWS {
            break;
        }
        let cells: Vec<&str> = line.split(sep).take(MAX_COLS).collect();
        if cells.iter().all(|c| c.trim().is_empty()) {
            continue;
        }
        out.push_str(&cells.join(" | "));
        out.push('\n');
        if out.len() as u64 > DOC_MAX_BYTES {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn classify_urls() {
        assert_eq!(classify_url("https://x.example/a.PDF"), Some(DocKind::Pdf));
        assert_eq!(classify_url("https://x.example/a.pdf?page=2"), Some(DocKind::Pdf));
        assert_eq!(classify_url("https://x.example/sheet.XLSX#frag"), Some(DocKind::Xlsx));
        assert_eq!(classify_url("https://x.example/a.docx"), Some(DocKind::Docx));
        assert_eq!(classify_url("https://x.example/a.epub"), Some(DocKind::Epub));
        assert_eq!(classify_url("https://x.example/a.csv"), Some(DocKind::Csv));
        assert_eq!(classify_url("https://x.example/a.txt"), Some(DocKind::Txt));
        assert_eq!(classify_url("https://x.example/page"), None);
        assert_eq!(classify_url("https://x.example/archive.zip"), None);
        assert_eq!(classify_url("https://x.example/a.rar"), None);
        assert_eq!(classify_url("https://x.example/a.tar.gz"), None);
        // "a.b/c" must not read "c" as extension
        assert_eq!(classify_url("https://x.example/a.b/c"), None);
    }

    #[test]
    fn titles_from_stems() {
        assert_eq!(title_for("https://x.example/corsa-c-datasheet.pdf", DocKind::Pdf), "corsa-c-datasheet (PDF)");
        assert_eq!(title_for("https://x.example/s.xlsx?x=1", DocKind::Xlsx), "s (XLSX)");
    }

    #[test]
    fn minimal_pdf_extracts() {
        // assembled minimal PDF with a REAL xref table (byte offsets
        // computed, not guessed — pdf-extract demands honest xref)
        fn pdf_with_text(line: &str) -> Vec<u8> {
            let content = format!("BT /F1 18 Tf 100 700 Td ({line}) Tj ET\n");
            let objs: Vec<String> = vec![
                "<< /Type /Catalog /Pages 2 0 R >>".into(),
                "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".into(),
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".into(),
                format!("<< /Length {} >>\nstream\n{content}endstream", content.len()),
                "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".into(),
            ];
            let mut out = b"%PDF-1.4\n".to_vec();
            let mut offs = vec![0usize];
            for (i, body) in objs.iter().enumerate() {
                offs.push(out.len());
                out.extend_from_slice(format!("{} 0 obj\n{}\nendobj\n", i + 1, body).as_bytes());
            }
            let xref_at = out.len();
            out.extend_from_slice(format!("xref\n0 {}\n", objs.len() + 1).as_bytes());
            out.extend_from_slice(b"0000000000 65535 f \n");
            for o in offs.iter().skip(1) {
                out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
            }
            out.extend_from_slice(
                format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n", objs.len() + 1).as_bytes(),
            );
            out
        }
        let pdf = pdf_with_text("Hermetic wisdom quote");
        let t = extract(DocKind::Pdf, &pdf).expect("minimal pdf must parse");
        assert!(t.contains("Hermetic wisdom quote"), "got: {t}");
    }

    #[test]
    fn pdf_rejects_liars() {
        assert!(extract(DocKind::Pdf, b"<html>not a pdf</html>").is_err());
    }

    #[test]
    fn csv_semicolons_win() {
        let t = extract(DocKind::Csv, b"a;b;c\n1;2;3\n".as_slice()).unwrap();
        assert!(t.contains("1 | 2 | 3"), "got: {t}");
        let t2 = extract(DocKind::Csv, b"a,b,c\n4,5,6\n".as_slice()).unwrap();
        assert!(t2.contains("4 | 5 | 6"), "got: {t2}");
    }

    #[test]
    fn docx_minimal_extracts() {
        // minimal docx: content-types + word/document.xml with two runs
        let mut buf = Cursor::new(vec![]);
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            zip.start_file("[Content_Types].xml", zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(b"<Types/>".as_slice()).unwrap();
            zip.start_file("word/document.xml", zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(b"<w:document><w:body><w:p><w:r><w:t>Hello </w:t></w:r><w:r><w:t>magpie</w:t></w:r></w:p></w:body></w:document>".as_slice()).unwrap();
            zip.finish().unwrap();
        }
        let t = extract(DocKind::Docx, buf.get_ref()).expect("minimal docx must parse");
        assert!(t.contains("Hello") && t.contains("magpie"), "got: {t}");
    }

    #[test]
    fn xlsx_minimal_extracts() {
        // minimal xlsx with inline strings (no sharedStrings needed)
        let mut buf = Cursor::new(vec![]);
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let files = [
                ("[Content_Types].xml", "<Types/>"),
                ("_rels/.rels", "<Relationships><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"xl/workbook.xml\"/></Relationships>"),
                ("xl/workbook.xml", "<workbook xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"><sheets><sheet name=\"S1\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>"),
                ("xl/_rels/workbook.xml.rels", "<Relationships><Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet\" Target=\"worksheets/sheet1.xml\"/></Relationships>"),
                ("xl/worksheets/sheet1.xml", "<worksheet><sheetData><row><c><is><t>engine</t></is></c><c><is><t>1.3 CDTI</t></is></c></row><row><c><is><t>power</t></is></c><c><is><t>70 hp</t></is></c></row></sheetData></worksheet>"),
            ];
            for (n, c) in files {
                zip.start_file(n, zip::write::SimpleFileOptions::default()).unwrap();
                zip.write_all(c.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        let t = extract(DocKind::Xlsx, buf.get_ref()).expect("minimal xlsx must parse");
        assert!(t.contains("1.3 CDTI") && t.contains("70 hp"), "got: {t}");
    }

    #[test]
    fn epub_minimal_extracts() {
        let mut buf = Cursor::new(vec![]);
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            zip.start_file("mimetype", zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(b"application/epub+zip").unwrap();
            zip.start_file("OEBPS/ch1.xhtml", zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(b"<html><body><p>Corpus Hermeticum fragment</p></body></html>").unwrap();
            zip.finish().unwrap();
        }
        let t = extract(DocKind::Epub, buf.get_ref()).expect("minimal epub must parse");
        assert!(t.contains("Corpus Hermeticum fragment"), "got: {t}");
    }

    #[test]
    fn zip_bomb_guards_hold() {
        // 600 members > 500 cap
        let mut buf = Cursor::new(vec![]);
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            for i in 0..600 {
                zip.start_file(format!("OEBPS/p{i}.xhtml"), zip::write::SimpleFileOptions::default()).unwrap();
                zip.write_all(b"<p>x</p>").unwrap();
            }
            zip.finish().unwrap();
        }
        assert!(extract(DocKind::Epub, buf.get_ref()).is_err());
        // oversize input refused before parsing
        let big = vec![0u8; (DOC_MAX_BYTES + 1) as usize];
        assert!(extract(DocKind::Txt, &big).is_err());
    }
}
