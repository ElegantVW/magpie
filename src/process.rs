use crate::error::{MagpieError, Result};

#[derive(Debug, serde::Deserialize)]
struct SeekPage {
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, serde::Serialize, Clone)]
struct Entry {
    url: String,
    title: String,
    kind: String,
    via: String,
    bullets: Vec<String>,
    specs: Vec<[String; 2]>,
    /// verbatim spans (≤90 chars) for the bibliography
    quotes: Vec<String>,
    /// outline chapter this entry was filed into
    section: String,
    /// corroboration: how many dup pages repeat this keeper
    claimed_by: usize,
}

use crate::llm::{llm_app, llm_endpoint};

const VALID_KINDS: &[&str] = &["spec", "fault", "review", "dimensions", "facts", "howto", "reference", "history", "other"];

async fn process_one(client: &reqwest::Client, endpoint: &str, via_tag: &str, chapters: &[String], meaning: &str, query: &str, base: &str, title: &str, text: &str) -> (String, Vec<String>, Vec<[String; 2]>, Vec<String>, String, String) {
    let excerpt: String = text.chars().take(5000).collect();
    let chlist = chapters.join(" | ");
    let sys = format!("You file web pages into a knowledge manual on '{query}' ({meaning}). Reply with ONLY a single JSON object, no other text. Quotes must be copied EXACTLY from the text, at most 90 characters each. Relevance scores whether the page is ABOUT the subject itself — never the chapter, never derivation: something NAMED AFTER or DERIVED FROM the subject (a poetry movement named for obscurity, a band called Portugal, a surname) scores below 40 even when the chapter seems to fit. File each page into exactly one chapter: {chlist}.");
    let user = format!(
        "{{\"task\": \"classify\", \"title\": \"{title}\", \"url\": \"{base}\", \"text\": \"{excerpt}\", \"want\": {{\"kind\": \"spec|fault|review|dimensions|facts|howto|reference|history|other\", \"section\": \"one of: {chlist}\", \"bullets\": [\"up to 10 facts\"], \"specs\": [[\"key\", \"value\"] up to 12], \"quotes\": [\"up to 3 verbatim spans\"]}}}}"
    );
    let body = serde_json::json!({
        "messages": [{"role": "system", "content": sys}, {"role": "user", "content": user}],
        "temperature": 0.1,
        "max_tokens": 700,
        "response_format": {"type": "json_object"},
        "tools": [{
            "type": "function",
            "function": {
                "name": "classify_entry",
                "description": "File one crawled page into the vehicle manual",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "enum": ["spec", "fault", "review", "dimensions", "facts", "howto", "reference", "history", "other"]},
                        "relevance": {"type": "integer", "minimum": 0, "maximum": 100},
                        "bullets": {"type": "array", "items": {"type": "string"}, "maxItems": 10},
                        "specs": {"type": "array", "items": {"type": "array", "items": {"type": "string"}, "minItems": 2, "maxItems": 2}, "maxItems": 12},
                        "quotes": {"type": "array", "items": {"type": "string"}, "maxItems": 3},
                        "section": {"type": "string"}
                    },
                    "required": ["kind", "bullets", "specs", "quotes", "relevance", "section"]
                }
            }
        }],
        "tool_choice": {"type": "function", "function": {"name": "classify_entry"}},
    });
    let mut attempt = crate::llm::chat_json(client, endpoint, &sys, &user, 700, 0.1).await;
    if attempt.is_none() {
        // model may have been idle-evicted mid-run: summon its slot once, retry once
        let mut cmd = tokio::process::Command::new("menagerie");
        cmd.args(["ensure", &crate::llm::llm_app()]);
        if std::env::var("MAGPIE_GPU").map(|v| v != "0").unwrap_or(true) {
            cmd.env("PIXIE_LLAMA_SERVER", format!("{}/.local/lib/llama-vulkan/llama-server-vk", std::env::var("HOME").unwrap_or_else(|_| "/home/evenweaker".into())));
            cmd.env("PIXIE_LLM_NGL", "99");
        }
        let _ = cmd.output().await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        attempt = crate::llm::chat_json(client, endpoint, &sys, &user, 700, 0.1).await;
    }
    // tool contract first (real function call), envelope/content JSON after.
    // Heuristic never silent.
    let (v, tooled) = match attempt {
        Some((v, tooled)) => (v, tooled),
        None => {
            let sec = chapters.last().cloned().unwrap_or_else(|| "Other".into());
            return with_via(heuristic(title, &excerpt), vec![], sec, "heuristic:no-llm");
        }
    };
    let parsed: Option<(String, Vec<String>, Vec<[String; 2]>, Vec<String>, String, bool, i64)> = (|| {
        // v resolved by llm::chat_json (tool contract → envelope → content)
        let raw_kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("other").to_lowercase();
        let kind = if VALID_KINDS.contains(&raw_kind.as_str()) { raw_kind } else { "other".into() };
        // relevance gate: the model scores fit to the query; <40 never
        // reaches the manual (Mixolydian-mode class of drift)
        let relevance: i64 = v.get("relevance").and_then(|r| r.as_i64()).unwrap_or(50);
        let bullets: Vec<String> = v
            .get("bullets")
            .and_then(|b| b.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.chars().take(200).collect())).take(10).collect())
            .unwrap_or_default();
        let specs: Vec<[String; 2]> = v
            .get("specs")
            .and_then(|b| b.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        if let Some(s) = x.as_str() {
                            // lenient: flat strings become spec rows
                            Some(["spec".to_string(), s.chars().take(88).collect()])
                        } else {
                            Some([
                                x.get(0)?.as_str()?.chars().take(28).collect(),
                                x.get(1)?.as_str()?.chars().take(60).collect(),
                            ])
                        }
                    })
                    .take(12)
                    .collect()
            })
            .unwrap_or_default();
        // verbatim spans for the bibliography: capped, de-emptied
        let quotes: Vec<String> = v
            .get("quotes")
            .and_then(|b| b.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str())
                    .map(|s| s.trim().chars().take(90).collect::<String>())
                    .filter(|s| !s.is_empty())
                    .take(3)
                    .collect()
            })
            .unwrap_or_default();
        // chapter assignment, validated against the outline; unknown falls
        // back to the last chapter (always the misc bucket)
        let raw_sec = v.get("section").and_then(|s| s.as_str()).unwrap_or("").to_string();
        let section = chapters
            .iter()
            .find(|c| c.to_lowercase() == raw_sec.to_lowercase())
            .cloned()
            .unwrap_or_else(|| chapters.last().cloned().unwrap_or_else(|| "Other".into()));
        if bullets.is_empty() && specs.is_empty() && quotes.is_empty() {
            return None;
        }
        Some((kind, bullets, specs, quotes, section, tooled, relevance))
    })();
    match parsed {
        Some((k, b, s, q, sec, true, r)) if r >= 40 => (k, b, s, q, sec, format!("{via_tag}:tool")),
        Some((k, b, s, q, sec, false, r)) if r >= 40 => (k, b, s, q, sec, format!("{via_tag}:json")),
        Some(_) => return with_via(heuristic(title, &excerpt), vec![], chapters.last().cloned().unwrap_or_else(|| "Other".into()), "heuristic:irrelevant"),
        None => {
            let (k, b, s) = heuristic(title, &excerpt);
            (k, b, s, vec![], chapters.last().cloned().unwrap_or_else(|| "Other".into()), "heuristic:parse".into())
        }
    }
}

fn with_via(h: (String, Vec<String>, Vec<[String; 2]>), quotes: Vec<String>, section: String, via: &str) -> (String, Vec<String>, Vec<[String; 2]>, Vec<String>, String, String) {
    (h.0, h.1, h.2, quotes, section, via.into())
}

fn heuristic(title: &str, text: &str) -> (String, Vec<String>, Vec<[String; 2]>) {
    let t = format!("{} {}", title, text.chars().take(400).collect::<String>()).to_lowercase();
    let kind = if t.contains("fault") || t.contains("problem") || t.contains("recall") || t.contains("common") {
        "fault"
    } else if t.contains("spec") || t.contains("ps") || t.contains("hp") || t.contains("nm") || t.contains("kw") {
        "spec"
    } else if t.contains("review") || t.contains("drive") {
        "review"
    } else if t.contains("dimension") || t.contains("length") || t.contains("wheelbase") || t.contains("boot") {
        "dimensions"
    } else {
        "other"
    };
    let mut bullets = vec![];
    for line in text.split('\n').filter(|l| l.trim().len() > 40).take(3) {
        bullets.push(line.trim().chars().take(160).collect());
    }
    // harvest spec-looking rows so manuals carry tables even offline
    let mut specs = vec![];
    for line in text.split('\n') {
        let l = line.trim();
        if l.len() < 12 || l.len() > 140 {
            continue;
        }
        let ll = l.to_lowercase();
        let has_num = l.chars().any(|c| c.is_ascii_digit());
        let has_unit = ["ps", "hp", "kw", "nm", "mm", "kg", "l/100", "g/km", "litre", "litres", "seconds", "mph", "km/h"]
            .iter()
            .any(|u| ll.contains(u));
        if has_num && has_unit {
            let kv: Vec<&str> = l.splitn(2, ':').collect();
            if kv.len() == 2 {
                specs.push([kv[0].trim().chars().take(28).collect(), kv[1].trim().chars().take(60).collect()]);
            } else {
                specs.push(["spec".to_string(), l.chars().take(88).collect()]);
            }
            if specs.len() >= 6 {
                break;
            }
        }
    }
    (kind.into(), bullets, specs)
}

pub async fn run_process(dir: std::path::PathBuf, prog: Option<tokio::sync::mpsc::UnboundedSender<String>>) -> Result<std::path::PathBuf> {
    let raw = std::fs::read_to_string(dir.join("seek.json")).map_err(|e| MagpieError::Io(e.to_string()))?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| MagpieError::Parse(e.to_string()))?;
    let query = v.get("query").and_then(|q| q.as_str()).unwrap_or("seek").to_string();
    let arr = v.get("pages").and_then(|p| p.as_array()).cloned().unwrap_or_default();
    // producer outline: subject domain from the wikidata synthetic page,
    // chapters from the domain table. The manual renders chapters in order
    // with explicit gap flags where coverage failed.
    let instance: String = arr
        .iter()
        .find(|p| p.get("url").and_then(|u| u.as_str()).map(|u| u.contains("wikidata.org")).unwrap_or(false))
        .and_then(|p| p.get("text").and_then(|t| t.as_str()))
        .unwrap_or("")
        .lines()
        .find_map(|l| l.strip_prefix("instance of:"))
        .unwrap_or("")
        .to_string();
    let domain = crate::crawl::subject_domain(&query, &instance);
    let chapters = crate::crawl::chapters(&domain);
    // meaning line from the stored query card (falls back to the query);
    // the relevance prompt judges against meaning, never spelling
    let meaning: String = v
        .get("query_card")
        .and_then(|c| c.get("meaning"))
        .and_then(|m| m.as_str())
        .filter(|m| !m.is_empty())
        .unwrap_or(&query)
        .chars()
        .take(300)
        .collect();
    // route inference through menagerie (house control plane): ensure the
    // magpie slot, resolve its URL. No raw-port assumptions.
    let app = llm_app();
    if let Some(tx) = &prog {
        let _ = tx.send(format!("ensure {app} model…"));
    }
    let (endpoint, via_tag) = llm_endpoint().await;
    if let Some(tx) = &prog {
        let _ = tx.send(format!("model: {endpoint}"));
    }
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().map_err(|e| MagpieError::Network(e.to_string()))?;
    let mut entries = vec![];
    // volume guard: cap keepers so hundred-chunk book pulls stay tractable.
    // Round-robin across hosts (breadth first), densest first within host.
    const MAX_KEEPERS: usize = 80;
    let mut keeper_idx: Vec<usize> = vec![];
    {
        let qtokens: Vec<String> = query
            .to_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|t| t.len() > 2)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .map(|t| t.to_string())
            .collect();
        // per-host buckets, densest first
        let mut by_host: std::collections::HashMap<String, Vec<(usize, usize)>> = Default::default();
        for (i, p) in arr.iter().enumerate() {
            if p.get("dup_of").and_then(|d| d.as_u64()).is_some() {
                continue;
            }
            let t = p.get("text").and_then(|t| t.as_str()).unwrap_or("").to_lowercase();
            let hits: usize = qtokens.iter().map(|tok| t.matches(tok).count()).sum();
            let len = p.get("text").and_then(|t| t.as_str()).map(|t| t.chars().count().max(1)).unwrap_or(1);
            let score = hits.saturating_mul(1000) / len;
            let host = p.get("url").and_then(|u| u.as_str()).unwrap_or("").split('/').nth(2).unwrap_or("").to_string();
            by_host.entry(host).or_default().push((score, i));
        }
        for v in by_host.values_mut() {
            v.sort_by(|a, b| b.0.cmp(&a.0));
        }
        let mut hosts: Vec<String> = by_host.keys().cloned().collect();
        hosts.sort();
        let mut round = 0;
        loop {
            let mut added = false;
            for h in hosts.iter() {
                if keeper_idx.len() >= MAX_KEEPERS {
                    break;
                }
                if let Some((_, i)) = by_host.get(h).and_then(|v| v.get(round).copied()) {
                    keeper_idx.push(i);
                    added = true;
                }
            }
            if !added || keeper_idx.len() >= MAX_KEEPERS {
                break;
            }
            round += 1;
        }
    }
    let mut dropped_irrelevant = 0;
    // corroboration map: keeper index -> dup count (dups skip the LLM pass)
    let mut corrobor: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    // index map: seek.json page position -> entries position (keepers only)
    let mut keeper_at: Vec<Option<usize>> = vec![None; arr.len()];
    let kept_total = keeper_idx.len();
    for (n, i) in keeper_idx.iter().copied().enumerate() {
        let p = &arr[i];
        keeper_at[i] = Some(entries.len());
        let url = p.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let title = p.get("title").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let text = p.get("text").and_then(|u| u.as_str()).unwrap_or("").to_string();
        if let Some(tx) = &prog {
            let _ = tx.send(format!("pixie {}/{} {}", n + 1, kept_total, url.chars().take(50).collect::<String>()));
        }
        let (kind, bullets, specs, quotes, section, via) = process_one(&client, &endpoint, &via_tag, &chapters, &meaning, &query, &url, &title, &text).await;
        // namesake filter: parenthetical disambiguators that never belong
        // in a subject manual (bands, surnames, albums share query words)
        let tl = title.to_lowercase();
        if tl.contains("(band)")
            || tl.contains("(surname)")
            || tl.contains("(given name)")
            || tl.contains("(disambiguation)")
            || tl.contains("(album)")
            || tl.contains("(song)")
            || tl.contains("(mode)")
        {
            if let Some(tx) = &prog {
                let _ = tx.send(format!("drop namesake: {}", title.chars().take(50).collect::<String>()));
            }
            keeper_at[i] = None;
            dropped_irrelevant += 1;
            continue;
        }
        // relevance gate: model-scored <40 never reaches the manual
        if via == "heuristic:irrelevant" {
            if let Some(tx) = &prog {
                let _ = tx.send(format!("drop irrelevant: {}", title.chars().take(50).collect::<String>()));
            }
            keeper_at[i] = None;
            dropped_irrelevant += 1;
            continue;
        }
        entries.push(Entry { url, title, kind, via, bullets, specs, quotes, section, claimed_by: 0 });
    }
    // second pass: attribute dups to their keepers (entries index space)
    for p in arr.iter() {
        if let Some(k) = p.get("dup_of").and_then(|d| d.as_u64()).and_then(|k| keeper_at.get(k as usize).copied().flatten()) {
            *corrobor.entry(k).or_insert(0) += 1;
        }
    }
    for (ei, e) in entries.iter_mut().enumerate() {
        e.claimed_by = *corrobor.get(&ei).unwrap_or(&0);
    }
    let out = serde_json::json!({
        "query": query,
        "title": format!("{query} — manual"),
        "domain": domain,
        "chapters": chapters,
        "entries": entries,
        "dropped_irrelevant": dropped_irrelevant,
    });
    std::fs::write(dir.join("processed.json"), serde_json::to_string_pretty(&out).unwrap()).map_err(|e| MagpieError::Io(e.to_string()))?;
    // synthesis pass: one clearly-marked generated intro per covered
    // chapter (facts stay verbatim in entries; intros are connective tissue)
    let mut intros: std::collections::HashMap<String, (String, String)> = Default::default();
    {
        let client2 = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().map_err(|e| MagpieError::Network(e.to_string()))?;
        for ch in chapters.iter() {
            let facts: Vec<String> = entries
                .iter()
                .filter(|e| &e.section == ch)
                .flat_map(|e| e.bullets.iter().take(2).cloned())
                .take(12)
                .collect();
            if facts.len() < 2 {
                continue;
            }
            if let Some(tx) = &prog {
                let _ = tx.send(format!("synthesizing {ch}…"));
            }
            let sys = "Write a short chapter introduction. Use ONLY the facts given. Reply ONLY JSON.";
            let user = format!(
                "{{\"manual\": \"{query}\", \"chapter\": \"{ch}\", \"facts\": {:?}, \"want\": {{\"intro\": \"150-250 word overview, no new facts\"}}}}",
                facts
            );
            let body = serde_json::json!({
                "messages": [{"role": "system", "content": sys}, {"role": "user", "content": user}],
                "temperature": 0.3,
                "max_tokens": 400,
                "response_format": {"type": "json_object"},
            });
            let url = format!("{base}/v1/chat/completions", base = endpoint.trim_end_matches('/'));
            let mut text = String::new();
            if let Ok(Ok(r)) = tokio::time::timeout(std::time::Duration::from_secs(150), client2.post(&url).json(&body).send()).await {
                if let Ok(Ok(t)) = tokio::time::timeout(std::time::Duration::from_secs(30), r.text()).await {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        if let Some(s) = v["choices"][0]["message"]["content"].as_str() {
                            if let Ok(j) = serde_json::from_str::<serde_json::Value>(s) {
                                if let Some(i) = j.get("intro").and_then(|x| x.as_str()) {
                                    text = i.chars().take(1500).collect();
                                }
                            }
                        }
                    }
                }
            }
            if !text.is_empty() {
                intros.insert(ch.clone(), (text, format!("{via_tag}:synth")));
            }
        }
    }
    let out = serde_json::json!({
        "query": query,
        "title": format!("{query} — manual"),
        "domain": domain,
        "chapters": chapters,
        "entries": entries,
        "intros": intros.iter().map(|(k, (t, v))| (k.clone(), serde_json::json!({"text": t, "via": v}))).collect::<serde_json::Map<String, serde_json::Value>>(),
        "dropped_irrelevant": dropped_irrelevant,
    });
    std::fs::write(dir.join("processed.json"), serde_json::to_string_pretty(&out).unwrap()).map_err(|e| MagpieError::Io(e.to_string()))?;
    if let Some(tx) = &prog {
        let _ = tx.send("rendering manual…".into());
    }
    // structured manual from processed entries (grouped sections + TOC + sources)
    let mentries: Vec<crate::pdf::ManualEntry> = entries
        .iter()
        .map(|e| crate::pdf::ManualEntry {
            kind: e.kind.clone(),
            title: e.title.clone(),
            url: e.url.clone(),
            via: e.via.clone(),
            claimed_by: e.claimed_by,
            section: e.section.clone(),
            quotes: e.quotes.clone(),
            bullets: e.bullets.clone(),
            specs: e.specs.clone(),
        })
        .collect();
    let hosts: Vec<String> = {
        let mut h: Vec<String> = entries.iter().filter_map(|p| url::Url::parse(&p.url).ok()).map(|u| u.host_str().unwrap_or("").to_string()).collect();
        h.sort();
        h.dedup();
        h
    };
    crate::pdf::render_manual(&query, &chapters, &intros, &mentries, &hosts, &dir.join("manual.pdf"))?;
    if let Some(tx) = &prog {
        let _ = tx.send("done".into());
    }
    Ok(dir)
}
