use crate::error::{MagpieError, Result};

#[derive(Debug, serde::Deserialize)]
struct SeekPage {
    url: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    text: String,
}

#[derive(Debug, serde::Serialize)]
struct Entry {
    url: String,
    title: String,
    kind: String,
    bullets: Vec<String>,
    specs: Vec<[String; 2]>,
}

fn llm_url() -> String {
    std::env::var("PIXIE_LLM_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into())
}

/// One quick LLM task per entry, sequential. Falls back to heuristic when LLM down.
async fn process_one(client: &reqwest::Client, base: &str, title: &str, text: &str) -> (String, Vec<String>, Vec<[String; 2]>) {
    let excerpt: String = text.chars().take(1200).collect();
    let prompt = format!(
        "Classify this page for a vehicle manual. Reply ONLY JSON {{\"kind\":\"spec|fault|review|dimensions|other\",\"bullets\":[\"...\",\"...\",\"...\"],\"specs\":[[\"k\",\"v\"]]}} max 3 bullets, max 6 specs.\nTitle: {title}\nURL: {base}\nText:\n{excerpt}"
    );
    let body = serde_json::json!({"prompt": prompt, "n_predict": 192, "temperature": 0.1, "cache_prompt": true});
    let url = format!("{base}/completion", base = llm_url().trim_end_matches('/'));
    let txt = match tokio::time::timeout(
        std::time::Duration::from_secs(25),
        client.post(&url).json(&body).send(),
    )
    .await
    {
        Ok(Ok(r)) => match tokio::time::timeout(std::time::Duration::from_secs(20), r.text()).await {
            Ok(Ok(t)) => t,
            _ => String::new(),
        },
        _ => String::new(),
    };
    // llama /completion returns {"content":"..."} — extract first {...}
    if let Some(s) = txt.find('{') {
        if let Some(e) = txt.rfind('}') {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt[s..=e]) {
                let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("other").to_string();
                let bullets: Vec<String> = v
                    .get("bullets")
                    .and_then(|b| b.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).take(3).collect())
                    .unwrap_or_default();
                let specs: Vec<[String; 2]> = v
                    .get("specs")
                    .and_then(|b| b.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| {
                                let k = x.get(0)?.as_str()?.to_string();
                                let v = x.get(1)?.as_str()?.to_string();
                                Some([k, v])
                            })
                            .take(6)
                            .collect()
                    })
                    .unwrap_or_default();
                if !bullets.is_empty() || !specs.is_empty() {
                    return (kind, bullets, specs);
                }
            }
        }
    }
    heuristic(title, &excerpt)
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
    (kind.into(), bullets, vec![])
}

pub async fn run_process(dir: std::path::PathBuf, prog: Option<tokio::sync::mpsc::UnboundedSender<String>>) -> Result<std::path::PathBuf> {
    let raw = std::fs::read_to_string(dir.join("seek.json")).map_err(|e| MagpieError::Io(e.to_string()))?;
    let v: serde_json::Value = serde_json::from_str(&raw).map_err(|e| MagpieError::Parse(e.to_string()))?;
    let query = v.get("query").and_then(|q| q.as_str()).unwrap_or("seek").to_string();
    let arr = v.get("pages").and_then(|p| p.as_array()).cloned().unwrap_or_default();
    let total = arr.len();
    let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).build().map_err(|e| MagpieError::Network(e.to_string()))?;
    let mut entries = vec![];
    for (i, p) in arr.iter().enumerate() {
        let url = p.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let title = p.get("title").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let text = p.get("text").and_then(|u| u.as_str()).unwrap_or("").to_string();
        if let Some(tx) = &prog {
            let _ = tx.send(format!("pixie {}/{} {}", i + 1, total, url.chars().take(50).collect::<String>()));
        }
        let (kind, bullets, specs) = process_one(&client, &url, &title, &text).await;
        entries.push(Entry { url, title, kind, bullets, specs });
    }
    let out = serde_json::json!({
        "query": query,
        "title": format!("{query} — manual"),
        "entries": entries,
    });
    std::fs::write(dir.join("processed.json"), serde_json::to_string_pretty(&out).unwrap()).map_err(|e| MagpieError::Io(e.to_string()))?;
    if let Some(tx) = &prog {
        let _ = tx.send("rendering PDF…".into());
    }
    // render structured manual from processed entries
    let pages: Vec<crate::crawl::Page> = out["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| crate::crawl::Page {
            url: e["url"].as_str().unwrap_or("").into(),
            title: format!("[{}] {}", e["kind"].as_str().unwrap_or("other"), e["title"].as_str().unwrap_or("")),
            text: e["bullets"].as_array().map(|b| b.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join("\n")).unwrap_or_default(),
            chars: 0,
        })
        .collect();
    let hosts: Vec<String> = {
        let mut h: Vec<String> = pages.iter().filter_map(|p| url::Url::parse(&p.url).ok()).map(|u| u.host_str().unwrap_or("").to_string()).collect();
        h.sort();
        h.dedup();
        h
    };
    crate::pdf::render_pdf(&query, &pages, &hosts, &dir.join("manual.pdf"))?;
    if let Some(tx) = &prog {
        let _ = tx.send("done".into());
    }
    Ok(dir)
}
