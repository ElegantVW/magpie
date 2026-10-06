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

fn llm_app() -> String {
    std::env::var("MAGPIE_LLM_APP").unwrap_or_else(|_| "magpie".into())
}

/// Resolve the inference endpoint through menagerie (the house control
/// plane): ensure the app slot, then read its URL. Falls back to
/// PIXIE_LLM_URL, then pixie's :8080. Returns (url, via_tag).
async fn llm_endpoint() -> (String, String) {
    let app = llm_app();
    // GPU: the house Vulkan server + offload travel with the ensure call,
    // so the magpie slot stays GPU-backed across evictions/restarts.
    // Opt out with MAGPIE_GPU=0 (falls back to default CPU binary).
    let gpu = std::env::var("MAGPIE_GPU").map(|v| v != "0").unwrap_or(true);
    let mut cmd = tokio::process::Command::new("menagerie");
    cmd.args(["ensure", &app]);
    if gpu {
        cmd.env("PIXIE_LLAMA_SERVER", format!("{}/.local/lib/llama-vulkan/llama-server-vk", std::env::var("HOME").unwrap_or_else(|_| "/home/evenweaker".into())));
        cmd.env("PIXIE_LLM_NGL", "99");
    }
    let out = cmd.output().await;
    if out.map(|o| o.status.success()).unwrap_or(false) {
        if let Ok(p) = tokio::process::Command::new("menagerie")
            .args(["path", &app])
            .output()
            .await
        {
            let txt = String::from_utf8_lossy(&p.stdout);
            for ln in txt.lines() {
                if let Some(url) = ln.strip_prefix("URL=") {
                    let url = url.trim().to_string();
                    if !url.is_empty() {
                        return (url, format!("llm:{app}"));
                    }
                }
            }
        }
    }
    // fallbacks keep old behavior working when menagerie is absent
    if let Ok(u) = std::env::var("PIXIE_LLM_URL") {
        if !u.trim().is_empty() {
            return (u, "llm:env".into());
        }
    }
    ("http://127.0.0.1:8080".into(), "llm:pixie-fallback".into())
}

const VALID_KINDS: &[&str] = &["spec", "fault", "review", "dimensions", "facts", "howto", "reference", "history", "other"];

/// One quick LLM task per entry via OpenAI chat endpoint with enforced
/// `response_format: json_object`. Returns (kind, bullets, specs, via).
async fn chat_once(client: &reqwest::Client, url: &str, body: &serde_json::Value) -> std::result::Result<String, String> {
    // NOTE: llama-server answers after full generation (no stream), so the
    // send() budget must cover CPU prompt eval + generation (~60-90s).
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(150),
        client.post(url).json(body).send(),
    )
    .await
    .map_err(|e| format!("send:{e}"))?
    .map_err(|e| format!("send:{e}"))?;
    tokio::time::timeout(std::time::Duration::from_secs(30), resp.text())
        .await
        .map_err(|e| format!("body:{e}"))?
        .map_err(|e| format!("body:{e}"))
}

async fn process_one(client: &reqwest::Client, endpoint: &str, via_tag: &str, chapters: &[String], base: &str, title: &str, text: &str) -> (String, Vec<String>, Vec<[String; 2]>, Vec<String>, String, String) {
    let excerpt: String = text.chars().take(5000).collect();
    let chlist = chapters.join(" | ");
    let sys = format!("You file web pages into a knowledge manual. Reply with ONLY a single JSON object, no other text. Quotes must be copied EXACTLY from the text, at most 90 characters each. Score relevance 0 for same-name-different-thing pages (a band, a surname, a music mode sharing the query word). File each page into exactly one chapter: {chlist}.");
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
    let url = format!("{base}/v1/chat/completions", base = endpoint.trim_end_matches('/'));
    let mut attempt = chat_once(client, &url, &body).await;
    if attempt.is_err() {
        // model may have been idle-evicted mid-run: summon its slot once, retry once
        let mut cmd = tokio::process::Command::new("menagerie");
        cmd.args(["ensure", &llm_app()]);
        if std::env::var("MAGPIE_GPU").map(|v| v != "0").unwrap_or(true) {
            cmd.env("PIXIE_LLAMA_SERVER", format!("{}/.local/lib/llama-vulkan/llama-server-vk", std::env::var("HOME").unwrap_or_else(|_| "/home/evenweaker".into())));
            cmd.env("PIXIE_LLM_NGL", "99");
        }
        let _ = cmd.output().await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        attempt = chat_once(client, &url, &body).await;
    }
    let txt = match attempt {
        Ok(t) => t,
        Err(e) => {
            let sec = chapters.last().cloned().unwrap_or_else(|| "Other".into());
            return with_via(heuristic(title, &excerpt), vec![], sec, &format!("heuristic:{e}").chars().take(48).collect::<String>());
        }
    };
    // tool contract first: choices[0].message.tool_calls[0].function.arguments
    // (real function call, not prose). This server build may instead emit
    // the call as content {"name":..,"arguments":{..}} — accept that too.
    // Content-JSON last. Heuristic never silent.
    let root: Option<serde_json::Value> =
        serde_json::from_str::<serde_json::Value>(&txt).ok();
    let tool_args = root
        .as_ref()
        .and_then(|v| {
            v["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .map(|s| s.to_string())
        })
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let content = root
        .as_ref()
        .and_then(|v| v["choices"][0]["message"]["content"].as_str().map(|s| s.to_string()))
        .unwrap_or_default();
    // unwrap this server's {"name","arguments"} envelope when present
    let enveloped = serde_json::from_str::<serde_json::Value>(content.trim())
        .ok()
        .and_then(|v| {
            if v.get("name").is_some() && v.get("arguments").is_some() {
                Some(v["arguments"].clone())
            } else {
                Some(v)
            }
        });
    let parsed: Option<(String, Vec<String>, Vec<[String; 2]>, Vec<String>, String, bool, i64)> = (|| {
        // prefer the tool call; then the envelope; fall back to raw content JSON
        let (v, tooled): (serde_json::Value, bool) = match tool_args {
            Some(t) => (t, true),
            None => (enveloped?, false),
        };
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
    let total = arr.len();
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
    let mut dropped_irrelevant = 0;
    // corroboration map: keeper index -> dup count (dups skip the LLM pass)
    let mut corrobor: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    // index map: seek.json page position -> entries position (keepers only)
    let mut keeper_at: Vec<Option<usize>> = vec![None; arr.len()];
    for (i, p) in arr.iter().enumerate() {
        if p.get("dup_of").and_then(|d| d.as_u64()).is_some() {
            continue;
        }
        keeper_at[i] = Some(entries.len());
        let url = p.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let title = p.get("title").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let text = p.get("text").and_then(|u| u.as_str()).unwrap_or("").to_string();
        if let Some(tx) = &prog {
            let _ = tx.send(format!("pixie {}/{} {}", entries.len() + 1, total, url.chars().take(50).collect::<String>()));
        }
        let (kind, bullets, specs, quotes, section, via) = process_one(&client, &endpoint, &via_tag, &chapters, &url, &title, &text).await;
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
    crate::pdf::render_manual(&query, &chapters, &mentries, &hosts, &dir.join("manual.pdf"))?;
    if let Some(tx) = &prog {
        let _ = tx.send("done".into());
    }
    Ok(dir)
}
