//! Judge pass: fact-checks a finished manual against fresh web searches,
//! then delivers a text verdict. APPROVED appends a judge's appendix to
//! the PDF (comment + method); REDO leaves the PDF untouched and reports
//! reasons as text (no render cost for a rejection).

use crate::error::{MagpieError, Result};

struct Claim {
    text: String,
    source: String,
}

/// Top claims, spec/fault kinds first (checkable facts beat prose).
fn top_claims(entries: &[serde_json::Value]) -> Vec<Claim> {
    let mut ordered: Vec<&serde_json::Value> = vec![];
    for kind in ["spec", "fault", "facts", "review", "dimensions", "howto", "reference", "history", "other"] {
        for e in entries.iter().filter(|e| e.get("kind").and_then(|k| k.as_str()) == Some(kind)) {
            ordered.push(e);
        }
    }
    let mut out = vec![];
    let mut seen_sig: std::collections::HashSet<String> = Default::default();
    for e in ordered {
        let source = e.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let mut facts: Vec<String> = vec![];
        if let Some(arr) = e.get("bullets").and_then(|b| b.as_array()) {
            for b in arr.iter().take(2) {
                if let Some(s) = b.as_str() {
                    facts.push(s.to_string());
                }
            }
        }
        if let Some(arr) = e.get("specs").and_then(|b| b.as_array()) {
            for s in arr.iter().take(2) {
                if let (Some(k), Some(v)) = (s.get(0).and_then(|x| x.as_str()), s.get(1).and_then(|x| x.as_str())) {
                    facts.push(format!("{k}: {v}"));
                }
            }
        }
        for f in facts {
            if f.chars().count() > 25 {
                // specs often restate the bullet above them; one slot per fact
                let sig: String = f.to_lowercase().chars().filter(|c| c.is_alphanumeric()).take(48).collect();
                if !seen_sig.insert(sig) {
                    continue;
                }
                out.push(Claim { text: f.chars().take(160).collect(), source: source.clone() });
            }
            if out.len() >= 10 {
                return out;
            }
        }
    }
    out
}

pub async fn run_judge(dir: std::path::PathBuf, prog: Option<tokio::sync::mpsc::UnboundedSender<String>>) -> Result<std::path::PathBuf> {
    let send = |m: &str| {
        if let Some(tx) = &prog {
            let _ = tx.send(m.to_string());
        }
    };
    let raw = std::fs::read_to_string(dir.join("processed.json")).map_err(|e| MagpieError::Io(e.to_string()))?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| MagpieError::Parse(e.to_string()))?;
    let query = v.get("query").and_then(|q| q.as_str()).unwrap_or("seek").to_string();
    let entries = v.get("entries").and_then(|e| e.as_array()).cloned().unwrap_or_default();
    // unscored manuals are not judged: fact-checking heuristic fallback
    // bullets wastes inference on content the producer pass never scored.
    // (A slow-day timeout once filled a whole manual with heuristic:no-llm.)
    {
        let h = entries
            .iter()
            .filter(|e| e.get("via").and_then(|x| x.as_str()).map(|s| s.starts_with("heuristic")).unwrap_or(false))
            .count();
        if !entries.is_empty() && h * 2 > entries.len() {
            let comment = format!(
                "producer pass failed: {h}/{} entries unscored (heuristic fallback) — fix inference health, reprocess, then re-judge",
                entries.len()
            );
            let record = serde_json::json!({
                "query": query,
                "overall": "REDO",
                "comment": comment,
                "via": "heuristic:unscored",
                "verdicts": [],
                "searches": 0,
            });
            std::fs::write(dir.join("judgment.json"), serde_json::to_string_pretty(&record).unwrap()).map_err(|e| MagpieError::Io(e.to_string()))?;
            std::fs::write(dir.join("judgment.txt"), format!("JUDGMENT: REDO (heuristic:unscored)\n{comment}\n")).map_err(|e| MagpieError::Io(e.to_string()))?;
            crate::db::append_subject(
                &query,
                "quarantine",
                serde_json::json!({"title": format!("{query} — manual"), "leg": "judge", "reason": format!("REDO unscored: {comment}")}),
            );
            crate::db::manifest_stage(
                &dir,
                "judge",
                serde_json::json!({"overall": "REDO", "via": "heuristic:unscored", "searches": 0}),
            );
            send("judge: REDO (unscored manual, no searches spent)");
            return Ok(dir);
        }
    }
    let claims = top_claims(&entries);
    if claims.is_empty() {
        return Err(MagpieError::Parse("no checkable claims".into()));
    }
    // ≥10 verification searches of our own (one+ per claim, distinct queries)
    send(&format!("judge: verifying {} claims…", claims.len()));
    let mut pairs = vec![];
    for (i, c) in claims.iter().enumerate() {
        let q: String = c.text.chars().take(80).collect();
        if let Some(tx) = &prog {
            let _ = tx.send(format!("judge search {}/{} {}", i + 1, claims.len(), q.chars().take(40).collect::<String>()));
        }
        let mut evidence = vec![];
        for u in crate::crawl::verify_search(&c.text).await {
            if evidence.len() >= 2 {
                break;
            }
            if let Some(p) = crate::crawl::fetch_snippet(&u).await {
                evidence.push(format!("{}: {}", u, p));
            }
        }
        pairs.push(serde_json::json!({"claim": c.text, "manual_source": c.source, "evidence": evidence}));
    }
    // one judge call over all pairs
    let (endpoint, via_tag) = crate::llm::llm_endpoint().await;
    // no client-level timeout: chat_once governs (600s send / 60s body).
    // The deliberation payload is ~12KB of evidence; 30s was too tight.
    let client = reqwest::Client::new();
    let sys = "You are the judge of a researched manual. For each claim, verdict supported|contested|unverified from the evidence. Then overall APPROVED or REDO. Reply ONLY JSON.";
    let user = serde_json::json!({
        "manual": query,
        "pairs": pairs,
        "want": {"verdicts": [{"claim": "", "verdict": "supported|contested|unverified", "note": "under 90 chars"}], "overall": "APPROVED|REDO", "comment": "why approved and how fact-checked, or why it must be redone"}
    })
    .to_string();
    send("judge: deliberating…");
    let mut judgment = serde_json::json!({"overall": "REDO", "comment": "judge unreachable", "verdicts": []});
    let mut judge_via = "heuristic:judge-down";
    // generous budget: 10 pairs of evidence in, 10 terse verdicts out.
    // 600 tokens truncated mid-JSON (parse then fails → unreachable).
    // ctx 8192 fits ~5K prompt + 1500 generation.
    if let Some(o) = crate::llm::chat_json(&client, &endpoint, sys, &user, 1500, 0.1).await {
        let _ = o.tooled;
        judgment = o.v;
        judge_via = Box::leak(format!("{via_tag}:judge").into_boxed_str());
    }
    let overall = judgment.get("overall").and_then(|o| o.as_str()).unwrap_or("REDO").to_string();
    let comment = judgment.get("comment").and_then(|c| c.as_str()).unwrap_or("").to_string();
    let record = serde_json::json!({
        "query": query,
        "overall": overall,
        "comment": comment,
        "via": judge_via,
        "verdicts": judgment.get("verdicts").cloned().unwrap_or(serde_json::Value::Null),
        "searches": pairs.len(),
    });
    std::fs::write(dir.join("judgment.json"), serde_json::to_string_pretty(&record).unwrap()).map_err(|e| MagpieError::Io(e.to_string()))?;
    // human text form: no PDF cost for reading the verdict
    let mut txt = format!("JUDGMENT: {} ({})\n{}\n\n", overall, judge_via, comment);
    if let Some(arr) = record.get("verdicts").and_then(|x| x.as_array()) {
        for vd in arr {
            txt.push_str(&format!(
                "- [{}] {}\n  {}\n",
                vd.get("verdict").and_then(|x| x.as_str()).unwrap_or("?"),
                vd.get("claim").and_then(|x| x.as_str()).unwrap_or(""),
                vd.get("note").and_then(|x| x.as_str()).unwrap_or("")
            ));
        }
    }
    std::fs::write(dir.join("judgment.txt"), txt).map_err(|e| MagpieError::Io(e.to_string()))?;
    send(&format!("judge: {overall}"));
    if overall == "APPROVED" {
        // appendix page: the judge's comment + method, inside the document
        if let Some(tx) = &prog {
            let _ = tx.send("judge: appending verdict…".into());
        }
        append_verdict(&dir, &query, &comment)?;
    } else {
        // REDO quarantines the manual itself: refused output with reasons,
        // filed under the subject like any refused work
        crate::db::append_subject(
            &query,
            "quarantine",
            serde_json::json!({"title": format!("{query} — manual"), "leg": "judge", "reason": format!("REDO: {}", comment.chars().take(200).collect::<String>())}),
        );
    }
    // manifest stage judge: the chain is complete
    // (claim → entry → page → fetch → run → verdict)
    crate::db::manifest_stage(
        &dir,
        "judge",
        serde_json::json!({
            "overall": overall,
            "judgment.json": crate::db::sha_file(&dir.join("judgment.json")),
            "manual.pdf": crate::db::sha_file(&dir.join("manual.pdf")),
            "searches": pairs.len(),
        }),
    );
    Ok(dir)
}

/// Appendix: re-render is overkill — append one page via the manual writer.
/// Reuses render path with an appendix section.
fn append_verdict(dir: &std::path::Path, query: &str, comment: &str) -> Result<()> {
    let raw = std::fs::read_to_string(dir.join("processed.json")).map_err(|e| MagpieError::Io(e.to_string()))?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| MagpieError::Parse(e.to_string()))?;
    let chapters: Vec<String> = v
        .get("chapters")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_else(|| vec!["Facts".into()]);
    let mut mentries: Vec<crate::pdf::ManualEntry> = vec![];
    if let Some(arr) = v.get("entries").and_then(|e| e.as_array()) {
        for e in arr {
            mentries.push(crate::pdf::ManualEntry {
                kind: e.get("kind").and_then(|k| k.as_str()).unwrap_or("other").into(),
                title: e.get("title").and_then(|k| k.as_str()).unwrap_or("").into(),
                url: e.get("url").and_then(|k| k.as_str()).unwrap_or("").into(),
                via: e.get("via").and_then(|k| k.as_str()).unwrap_or("").into(),
                claimed_by: e.get("claimed_by").and_then(|k| k.as_u64()).unwrap_or(0) as usize,
                section: e.get("section").and_then(|k| k.as_str()).unwrap_or("").into(),
                // carry content through: the appendix is a re-render, the
                // manual must come out identical plus the verdict page
                quotes: e
                    .get("quotes")
                    .and_then(|q| q.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default(),
                bullets: e
                    .get("bullets")
                    .and_then(|b| b.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default(),
                specs: e
                    .get("specs")
                    .and_then(|s| s.as_array())
                    .map(|a| {
                        a.iter()
                            .filter(|p| p.as_array().map(|x| x.len() == 2).unwrap_or(false))
                            .map(|p| [p[0].as_str().unwrap_or("").to_string(), p[1].as_str().unwrap_or("").to_string()])
                            .collect()
                    })
                    .unwrap_or_default(),
            });
        }
    }
    let hosts: Vec<String> = {
        let mut h: Vec<String> = mentries.iter().filter_map(|p| url::Url::parse(&p.url).ok()).map(|u| u.host_str().unwrap_or("").to_string()).collect();
        h.sort();
        h.dedup();
        h
    };
    // chapter intros are generated at process time; re-render must carry
    // them or the manual loses its synthesis pass
    let intros: std::collections::HashMap<String, (String, String)> = v
        .get("intros")
        .and_then(|i| i.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| {
                    let t = val.get("text").and_then(|t| t.as_str())?.to_string();
                    let via = val.get("via").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    Some((k.clone(), (t, via)))
                })
                .collect()
        })
        .unwrap_or_default();
    // verdict entry lives in its own appendix chapter
    let mut chapters2 = chapters.clone();
    chapters2.push("Judge's verdict: APPROVED".into());
    mentries.push(crate::pdf::ManualEntry {
        kind: "verdict".into(),
        title: "Why this manual was approved".into(),
        url: String::new(),
        via: "llm:judge".into(),
        claimed_by: 0,
        section: "Judge's verdict: APPROVED".into(),
        quotes: vec![],
        bullets: vec![comment.chars().take(1200).collect()],
        specs: vec![],
    });
    crate::pdf::render_manual(query, &chapters2, &intros, &mentries, &hosts, &dir.join("manual.pdf"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::append_verdict;

    #[test]
    fn appendix_carries_entry_content() {
        let dir = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({
            "query": "fidelity",
            "chapters": ["Facts", "History"],
            "intros": {"Facts": {"text": "UNIQUEINTRO synthesis paragraph.", "via": "llm:magpie:synth"}},
            "entries": [
                {
                    "kind": "spec", "title": "Fixture entry", "url": "https://fixture.example/1",
                    "via": "llm:magpie:json", "claimed_by": 2, "section": "Facts",
                    "quotes": ["UNIQUEQUOTE verbatim span."],
                    "bullets": ["UNIQUEBULLET distinctive checkable fact."],
                    "specs": [["uniquekey", "uniquevalue"]]
                }
            ]
        });
        std::fs::write(
            dir.path().join("processed.json"),
            serde_json::to_string_pretty(&doc).unwrap(),
        )
        .unwrap();

        append_verdict(dir.path(), "fidelity", "UNIQUECOMMENT the judge's method.").unwrap();

        let pdf = dir.path().join("manual.pdf");
        assert!(pdf.is_file(), "appendix render produced no pdf");
        let out = std::process::Command::new("pdftotext")
            .arg(&pdf)
            .arg("-")
            .output()
            .unwrap();
        let txt = String::from_utf8_lossy(&out.stdout);
        for must in ["UNIQUEQUOTE", "UNIQUEBULLET", "uniquekey", "uniquevalue", "UNIQUEINTRO", "UNIQUECOMMENT", "Judge"] {
            assert!(txt.contains(must), "re-render lost {must:?}:\n{txt}");
        }
    }
}

#[cfg(test)]
mod gate_tests {
    use super::run_judge;

    #[tokio::test]
    async fn unscored_manual_redos_without_searches() {
        let dir = tempfile::tempdir().unwrap();
        let doc = serde_json::json!({
            "query": "slowday",
            "entries": [
                {"kind": "other", "title": "A", "url": "https://a.example", "via": "heuristic:no-llm",
                 "bullets": ["x"], "specs": [], "quotes": [], "section": "Facts", "claimed_by": 0, "relevance": -1},
                {"kind": "other", "title": "B", "url": "https://b.example", "via": "heuristic:no-llm",
                 "bullets": ["y"], "specs": [], "quotes": [], "section": "Facts", "claimed_by": 0, "relevance": -1},
                {"kind": "facts", "title": "C", "url": "https://c.example", "via": "llm:test:json",
                 "bullets": ["z fact with enough length to check"], "specs": [], "quotes": [], "section": "Facts", "claimed_by": 0, "relevance": 90}
            ]
        });
        std::fs::write(dir.path().join("processed.json"), serde_json::to_string_pretty(&doc).unwrap()).unwrap();
        let out = run_judge(dir.path().to_path_buf(), None).await.unwrap();
        assert_eq!(out, dir.path().to_path_buf());
        let rec: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("judgment.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(rec.get("overall").and_then(|o| o.as_str()), Some("REDO"));
        assert_eq!(rec.get("via").and_then(|x| x.as_str()), Some("heuristic:unscored"));
        assert_eq!(rec.get("searches").and_then(|s| s.as_u64()), Some(0), "no searches spent on unscored");
    }
}
