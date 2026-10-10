//! The granary: DataFusion + Parquet lakehouse catalog over the hoard.
//! The amber (JSONL + seek dirs) stays source of truth; the granary is
//! derived and re-pressable — every table is hive-partitioned by subject
//! (db/granary/<table>/subject=<slug>/part-*.parquet) so compartments
//! prune for free and the loop's future floods stay queryable.
//! Feature-gated (`granary`, default OFF): 300 crates of compile.
//! Enable with `--features granary` (default features stay lean).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use datafusion::prelude::*;
use parquet::arrow::ArrowWriter;

pub fn granary_dir() -> PathBuf {
    crate::db::db_dir().join("granary")
}

fn str_col(vals: &[String]) -> Arc<StringArray> {
    Arc::new(StringArray::from(vals.to_vec()))
}

fn long_col(vals: &[i64]) -> Arc<Int64Array> {
    Arc::new(Int64Array::from(vals.to_vec()))
}

fn opt_long_col(vals: &[Option<i64>]) -> Arc<Int64Array> {
    Arc::new(Int64Array::from(vals.to_vec()))
}

fn write_table(table: &str, subject: &str, run: &str, schema: Arc<Schema>, batch: RecordBatch) -> Result<usize, String> {
    if batch.num_rows() == 0 {
        return Ok(0);
    }
    let dir = granary_dir()
        .join(table)
        .join(format!("subject={}", crate::store::slugify(subject)));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%f").to_string();
    let path = dir.join(format!("part-{run}-{stamp}.parquet"));
    let f = std::fs::File::create(&path).map_err(|e| e.to_string())?;
    let mut w = ArrowWriter::try_new(f, schema, None).map_err(|e| e.to_string())?;
    w.write(&batch).map_err(|e| e.to_string())?;
    w.close().map_err(|e| e.to_string())?;
    Ok(batch.num_rows())
}

fn schema_of(cols: &[(&str, DataType, bool)]) -> Arc<Schema> {
    Arc::new(Schema::new(
        cols.iter().map(|(n, t, nl)| Field::new(*n, t.clone(), *nl)).collect::<Vec<_>>(),
    ))
}

/// Press one finished run into the granary. Reads seek.json (+ fetch.jsonl,
/// processed.json, judgment.json, drops.json when present) and appends one
/// parquet file per non-empty table. This is the function the future loop
/// calls per cycle: search ends → press → word → new search.
pub fn press_run(dir: &Path) -> Result<Vec<(String, usize)>, String> {
    let run = dir
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "run".into());
    let raw = std::fs::read_to_string(dir.join("seek.json")).map_err(|e| e.to_string())?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let query = v.get("query").and_then(|q| q.as_str()).unwrap_or("seek").to_string();
    let subject = v
        .get("query_card")
        .and_then(|c| c.get("subject"))
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or(&query)
        .to_string();

    let mut pressed: Vec<(String, usize)> = vec![];

    // per-url language from the translation fan-out; default english
    let mut lang_of: std::collections::HashMap<String, String> = Default::default();
    if let Some(arr) = v.get("translations").and_then(|t| t.as_array()) {
        for t in arr {
            let a = t.as_array().cloned().unwrap_or_default();
            if a.len() >= 3 {
                if let (Some(l), Some(u)) = (a[0].as_str(), a[2].as_str()) {
                    lang_of.insert(u.to_string(), l.to_string());
                }
            }
        }
    }

    // pages: the big table (text included — TBs are the point)
    if let Some(pages) = v.get("pages").and_then(|p| p.as_array()) {
        let (mut url, mut title, mut text, mut chars, mut lang, mut qq, mut ss, mut rr) =
            (vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
        for p in pages {
            let u = p.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string();
            if u.is_empty() {
                continue;
            }
            url.push(u.clone());
            title.push(p.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string());
            text.push(p.get("text").and_then(|x| x.as_str()).unwrap_or("").to_string());
            chars.push(p.get("chars").and_then(|x| x.as_u64()).unwrap_or(0) as i64);
            lang.push(lang_of.get(&u).cloned().unwrap_or_else(|| "en".into()));
            qq.push(query.clone());
            ss.push(subject.clone());
            rr.push(run.clone());
        }
        let schema = schema_of(&[
            ("url", DataType::Utf8, false),
            ("title", DataType::Utf8, false),
            ("text", DataType::Utf8, false),
            ("chars", DataType::Int64, false),
            ("lang", DataType::Utf8, false),
            ("query", DataType::Utf8, false),
            ("subject", DataType::Utf8, false),
            ("run", DataType::Utf8, false),
        ]);
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![str_col(&url), str_col(&title), str_col(&text), long_col(&chars), str_col(&lang), str_col(&qq), str_col(&ss), str_col(&rr)],
        )
        .map_err(|e| e.to_string())?;
        pressed.push(("pages".into(), write_table("pages", &subject, &run, schema, batch)?));
    }

    // fetch ledger rows (fetch.jsonl per run; failures are data)
    {
        let (mut url, mut host, mut outcome, mut status, mut bytes, mut ms, mut attempts, mut note, mut qq, mut ss, mut rr) =
            (vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
        if let Ok(raw) = std::fs::read_to_string(dir.join("fetch.jsonl")) {
            for line in raw.lines().filter(|l| !l.trim().is_empty()) {
                if let Ok(r) = serde_json::from_str::<serde_json::Value>(line) {
                    let u = r.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    if u.is_empty() {
                        continue;
                    }
                    url.push(u);
                    host.push(r.get("host").and_then(|x| x.as_str()).unwrap_or("").to_string());
                    outcome.push(r.get("outcome").and_then(|x| x.as_str()).unwrap_or("").to_string());
                    status.push(r.get("status").and_then(|x| x.as_u64()).map(|s| s as i64));
                    bytes.push(r.get("bytes").and_then(|x| x.as_u64()).unwrap_or(0) as i64);
                    ms.push(r.get("ms").and_then(|x| x.as_u64()).unwrap_or(0) as i64);
                    attempts.push(r.get("attempts").and_then(|x| x.as_u64()).unwrap_or(0) as i64);
                    note.push(r.get("note").and_then(|x| x.as_str()).unwrap_or("").to_string());
                    qq.push(query.clone());
                    ss.push(subject.clone());
                    rr.push(run.clone());
                }
            }
        }
        let schema = schema_of(&[
            ("url", DataType::Utf8, false),
            ("host", DataType::Utf8, false),
            ("outcome", DataType::Utf8, false),
            ("status", DataType::Int64, true),
            ("bytes", DataType::Int64, false),
            ("ms", DataType::Int64, false),
            ("attempts", DataType::Int64, false),
            ("note", DataType::Utf8, false),
            ("query", DataType::Utf8, false),
            ("subject", DataType::Utf8, false),
            ("run", DataType::Utf8, false),
        ]);
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![str_col(&url), str_col(&host), str_col(&outcome), opt_long_col(&status), long_col(&bytes), long_col(&ms), long_col(&attempts), str_col(&note), str_col(&qq), str_col(&ss), str_col(&rr)],
        )
        .map_err(|e| e.to_string())?;
        pressed.push(("fetch".into(), write_table("fetch", &subject, &run, schema, batch)?));
    }

    // quarantine: refused works (seek.json) + refused chunks (drops.json)
    {
        let (mut title, mut url, mut leg, mut reason, mut qq, mut ss, mut rr) =
            (vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
        if let Some(arr) = v.get("quarantine").and_then(|q| q.as_array()) {
            for q in arr {
                title.push(q.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string());
                url.push(q.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string());
                leg.push(q.get("leg").and_then(|x| x.as_str()).unwrap_or("ia-books").to_string());
                reason.push(q.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string());
                qq.push(query.clone());
                ss.push(subject.clone());
                rr.push(run.clone());
            }
        }
        if let Ok(raw) = std::fs::read_to_string(dir.join("drops.json")) {
            if let Ok(arr) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(items) = arr.as_array() {
                    for q in items {
                        title.push(q.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string());
                        url.push(q.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string());
                        leg.push("process".into());
                        reason.push(q.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string());
                        qq.push(query.clone());
                        ss.push(subject.clone());
                        rr.push(run.clone());
                    }
                }
            }
        }
        let schema = schema_of(&[
            ("title", DataType::Utf8, false),
            ("url", DataType::Utf8, false),
            ("leg", DataType::Utf8, false),
            ("reason", DataType::Utf8, false),
            ("query", DataType::Utf8, false),
            ("subject", DataType::Utf8, false),
            ("run", DataType::Utf8, false),
        ]);
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![str_col(&title), str_col(&url), str_col(&leg), str_col(&reason), str_col(&qq), str_col(&ss), str_col(&rr)],
        )
        .map_err(|e| e.to_string())?;
        pressed.push(("quarantine".into(), write_table("quarantine", &subject, &run, schema, batch)?));
    }

    // entries: the manual's kept claims (processed.json)
    {
        let (mut url, mut title, mut kind, mut section, mut via, mut relevance, mut facts, mut qq, mut ss, mut rr) =
            (vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
        if let Ok(raw) = std::fs::read_to_string(dir.join("processed.json")) {
            if let Ok(p) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(arr) = p.get("entries").and_then(|e| e.as_array()) {
                    for e in arr {
                        url.push(e.get("url").and_then(|x| x.as_str()).unwrap_or("").to_string());
                        title.push(e.get("title").and_then(|x| x.as_str()).unwrap_or("").to_string());
                        kind.push(e.get("kind").and_then(|x| x.as_str()).unwrap_or("").to_string());
                        section.push(e.get("section").and_then(|x| x.as_str()).unwrap_or("").to_string());
                        via.push(e.get("via").and_then(|x| x.as_str()).unwrap_or("").to_string());
                        relevance.push(e.get("relevance").and_then(|x| x.as_i64()).unwrap_or(-1));
                        facts.push(
                            serde_json::json!({"bullets": e.get("bullets").cloned().unwrap_or_default(), "specs": e.get("specs").cloned().unwrap_or_default()}).to_string(),
                        );
                        qq.push(query.clone());
                        ss.push(subject.clone());
                        rr.push(run.clone());
                    }
                }
            }
        }
        let schema = schema_of(&[
            ("url", DataType::Utf8, false),
            ("title", DataType::Utf8, false),
            ("kind", DataType::Utf8, false),
            ("section", DataType::Utf8, false),
            ("via", DataType::Utf8, false),
            ("relevance", DataType::Int64, false),
            ("facts", DataType::Utf8, false),
            ("query", DataType::Utf8, false),
            ("subject", DataType::Utf8, false),
            ("run", DataType::Utf8, false),
        ]);
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![str_col(&url), str_col(&title), str_col(&kind), str_col(&section), str_col(&via), long_col(&relevance), str_col(&facts), str_col(&qq), str_col(&ss), str_col(&rr)],
        )
        .map_err(|e| e.to_string())?;
        pressed.push(("entries".into(), write_table("entries", &subject, &run, schema, batch)?));
    }

    // verdicts: the judge's per-claim calls (judgment.json)
    {
        let (mut qq, mut ss, mut rr, mut overall, mut claim, mut verdict, mut note) =
            (vec![], vec![], vec![], vec![], vec![], vec![], vec![]);
        if let Ok(raw) = std::fs::read_to_string(dir.join("judgment.json")) {
            if let Ok(j) = serde_json::from_str::<serde_json::Value>(&raw) {
                let ov = j.get("overall").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if let Some(arr) = j.get("verdicts").and_then(|x| x.as_array()) {
                    for vd in arr {
                        qq.push(query.clone());
                        ss.push(subject.clone());
                        rr.push(run.clone());
                        overall.push(ov.clone());
                        claim.push(vd.get("claim").and_then(|x| x.as_str()).unwrap_or("").to_string());
                        verdict.push(vd.get("verdict").and_then(|x| x.as_str()).unwrap_or("").to_string());
                        note.push(vd.get("note").and_then(|x| x.as_str()).unwrap_or("").to_string());
                    }
                }
            }
        }
        let schema = schema_of(&[
            ("query", DataType::Utf8, false),
            ("subject", DataType::Utf8, false),
            ("run", DataType::Utf8, false),
            ("overall", DataType::Utf8, false),
            ("claim", DataType::Utf8, false),
            ("verdict", DataType::Utf8, false),
            ("note", DataType::Utf8, false),
        ]);
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![str_col(&qq), str_col(&ss), str_col(&rr), str_col(&overall), str_col(&claim), str_col(&verdict), str_col(&note)],
        )
        .map_err(|e| e.to_string())?;
        pressed.push(("verdicts".into(), write_table("verdicts", &subject, &run, schema, batch)?));
    }

    // runs: one row per press (the loop's own ledger)
    {
        let schema = schema_of(&[
            ("query", DataType::Utf8, false),
            ("subject", DataType::Utf8, false),
            ("run", DataType::Utf8, false),
            ("created", DataType::Utf8, false),
            ("pages", DataType::Int64, false),
            ("entries", DataType::Int64, false),
            ("refused", DataType::Int64, false),
        ]);
        let pages_n = pressed.iter().find(|(t, _)| t == "pages").map(|(_, n)| *n as i64).unwrap_or(0);
        let entries_n = pressed.iter().find(|(t, _)| t == "entries").map(|(_, n)| *n as i64).unwrap_or(0);
        let refused_n = pressed.iter().find(|(t, _)| t == "quarantine").map(|(_, n)| *n as i64).unwrap_or(0);
        let created = v.get("created").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                str_col(&[query.clone()]),
                str_col(&[subject.clone()]),
                str_col(&[run.clone()]),
                str_col(&[created]),
                long_col(&[pages_n]),
                long_col(&[entries_n]),
                long_col(&[refused_n]),
            ],
        )
        .map_err(|e| e.to_string())?;
        pressed.push(("runs".into(), write_table("runs", &subject, &run, schema, batch)?));
    }

    Ok(pressed)
}

/// Query the granary with SQL. Tables register only when their dirs exist
/// (a fresh nest answers some queries with empty, never with errors).
/// Returns pretty-printed rows (capped) plus the total row count.
pub async fn sql(query: &str, cap: usize) -> Result<(String, usize), String> {
    let ctx = SessionContext::new();
    for table in ["pages", "fetch", "quarantine", "entries", "verdicts", "runs"] {
        let dir = granary_dir().join(table);
        if dir.is_dir() {
            ctx.register_parquet(table, dir.to_string_lossy().as_ref(), ParquetReadOptions::default())
                .await
                .map_err(|e| format!("register {table}: {e}"))?;
        }
    }
    let df = ctx.sql(query).await.map_err(|e| format!("sql: {e}"))?;
    let batches = df.collect().await.map_err(|e| format!("collect: {e}"))?;
    let total: usize = batches.iter().map(|b| b.num_rows()).sum();
    let shown: Vec<RecordBatch> = {
        let mut acc = vec![];
        let mut left = cap;
        for b in batches {
            if left == 0 {
                break;
            }
            let n = b.num_rows().min(left);
            acc.push(b.slice(0, n));
            left -= n;
        }
        acc
    };
    let pretty = datafusion::arrow::util::pretty::pretty_format_batches(&shown).map_err(|e| e.to_string())?;
    Ok((pretty.to_string(), total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_seek(dir: &Path) {
        let doc = serde_json::json!({
            "query": "granary-test",
            "depth": "quick",
            "created": "2026-10-11T00:00:00Z",
            "query_card": {"subject": "Granary Test", "meaning": "a test", "aliases": ["gtest"], "instance": ""},
            "legs": [["engines", 1]],
            "expansion": [],
            "translations": [["de", "Getreide-Test", "https://de.example/1"]],
            "pages": [
                {"url": "https://en.example/1", "title": "First", "text": "alpha beta gamma", "chars": 16, "dup_of": null},
                {"url": "https://de.example/1", "title": "Zweite", "text": "eins zwei drei", "chars": 14, "dup_of": null}
            ],
            "hosts": ["en.example", "de.example"],
            "warnings": [],
            "fetch_summary": [],
            "quarantine": [{"title": "Bad", "url": "https://bad.example", "leg": "ia-books", "reason": "off-subject"}]
        });
        std::fs::write(dir.join("seek.json"), serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        std::fs::write(
            dir.join("fetch.jsonl"),
            "{\"url\":\"https://en.example/1\",\"host\":\"en.example\",\"outcome\":\"ok\",\"status\":200,\"bytes\":100,\"ms\":50,\"attempts\":1,\"note\":\"16c\"}\n{\"url\":\"https://bad.example\",\"host\":\"bad.example\",\"outcome\":\"fail\",\"status\":null,\"bytes\":0,\"ms\":9000,\"attempts\":2,\"note\":\"net:timeout\"}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("processed.json"),
            "{\"query\":\"granary-test\",\"entries\":[{\"url\":\"https://en.example/1\",\"title\":\"First\",\"kind\":\"facts\",\"section\":\"Facts\",\"via\":\"llm:test:json\",\"bullets\":[\"alpha fact\"],\"specs\":[],\"quotes\":[],\"relevance\":90,\"claimed_by\":0}],\"dropped_irrelevant\":1}",
        )
        .unwrap();
        std::fs::write(
            dir.join("judgment.json"),
            "{\"query\":\"granary-test\",\"overall\":\"APPROVED\",\"comment\":\"fine\",\"via\":\"llm:test:judge\",\"verdicts\":[{\"claim\":\"alpha fact\",\"verdict\":\"supported\",\"note\":\"in text\"}],\"searches\":1}",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn granary_presses_and_answers() {
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("MAGPIE_DB", tmp.path().join("db"));
        let run = tmp.path().join("run1");
        std::fs::create_dir_all(&run).unwrap();
        sample_seek(&run);
        let pressed = press_run(&run).unwrap();
        let got: std::collections::HashMap<_, _> = pressed.into_iter().collect();
        assert_eq!(got.get("pages"), Some(&2));
        assert_eq!(got.get("fetch"), Some(&2));
        assert_eq!(got.get("quarantine"), Some(&1));
        assert_eq!(got.get("entries"), Some(&1));
        assert_eq!(got.get("verdicts"), Some(&1));
        assert_eq!(got.get("runs"), Some(&1));
        // hive partition pruning by subject
        let (out, total) = sql("SELECT subject, COUNT(*) AS n FROM pages GROUP BY subject", 20).await.unwrap();
        assert_eq!(total, 1);
        assert!(out.contains("Granary Test"), "rows carry subject: {out}");
        // failures are data: the rinse finds them
        let (out, total) = sql("SELECT url FROM fetch WHERE outcome = 'fail'", 20).await.unwrap();
        assert_eq!(total, 1);
        assert!(out.contains("bad.example"), "failed fetch queryable: {out}");
        // language rode along from translations
        let (_, total) = sql("SELECT url FROM pages WHERE lang = 'de'", 20).await.unwrap();
        assert_eq!(total, 1);
        std::env::remove_var("MAGPIE_DB");
    }
}
