//! Shared local-LLM plumbing: menagerie-routed endpoint + JSON-enforced calls.
//! Used by the query-expansion pass (plan time) and the per-entry pass
//! (process time). No cloud, no keys — house control plane only.

pub fn llm_app() -> String {
    std::env::var("MAGPIE_LLM_APP").unwrap_or_else(|_| "magpie".into())
}

/// Resolve the inference endpoint through menagerie (the house control
/// plane): ensure the app slot, then read its URL. Falls back to
/// PIXIE_LLM_URL, then pixie's :8080. Returns (url, via_tag).
pub async fn llm_endpoint() -> (String, String) {
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

/// One quick LLM round-trip. llama-server answers after full generation
/// (no stream), so budgets must cover CPU prompt eval + generation.
/// Floor math (measured): 700 output tokens at a bad-day 3 tok/s ≈ 233s
/// + prompt eval. A 150s budget once failed EVERY keeper on a slow day
/// (4 tok/s → 175s > 150s → whole manual silently heuristic). Budgets are
/// generous on purpose: too-short is catastrophic, too-long is waiting.
pub async fn chat_once(client: &reqwest::Client, url: &str, body: &serde_json::Value) -> std::result::Result<String, String> {
    let resp = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        client.post(url).json(body).send(),
    )
    .await
    .map_err(|e| format!("send:{e}"))?
    .map_err(|e| format!("send:{e}"))?;
    tokio::time::timeout(std::time::Duration::from_secs(60), resp.text())
        .await
        .map_err(|e| format!("body:{e}"))?
        .map_err(|e| format!("body:{e}"))
}

/// Extract the model's JSON from a chat response: tool_calls first,
/// then the {"name","arguments"} envelope this server build emits,
/// then raw content JSON.
pub fn extract_json(txt: &str) -> Option<(serde_json::Value, bool)> {
    let root: serde_json::Value = serde_json::from_str(txt).ok()?;
    if let Some(s) = root["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"].as_str() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
            return Some((v, true));
        }
    }
    let content = root["choices"][0]["message"]["content"].as_str().unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(content.trim()).ok()?;
    if v.get("name").is_some() && v.get("arguments").is_some() {
        return Some((v["arguments"].clone(), true));
    }
    Some((v, false))
}

/// Convenience: system + user + enforced JSON object, parsed or None.
pub async fn chat_json(
    client: &reqwest::Client,
    endpoint: &str,
    sys: &str,
    user: &str,
    max_tokens: u32,
    temperature: f32,
) -> Option<(serde_json::Value, bool)> {
    let body = serde_json::json!({
        "messages": [{"role": "system", "content": sys}, {"role": "user", "content": user}],
        "temperature": temperature,
        "max_tokens": max_tokens,
        "response_format": {"type": "json_object"},
    });
    let url = format!("{base}/v1/chat/completions", base = endpoint.trim_end_matches('/'));
    // failures surface on stderr: a silent LLM path once filled a whole
    // manual with heuristic fallbacks before anyone noticed
    let txt = match chat_once(client, &url, &body).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("llm: chat failed: {}", e.chars().take(160).collect::<String>());
            return None;
        }
    };
    match extract_json(&txt) {
        Some(pair) => Some(pair),
        // small models truncate mid-object when the token budget runs out;
        // salvage the balanced prefix rather than losing the whole answer
        None => {
            let content = serde_json::from_str::<serde_json::Value>(&txt)
                .ok()
                .and_then(|v| v["choices"][0]["message"]["content"].as_str().map(|s| s.to_string()))?;
            salvage_json(&content).map(|v| (v, false))
        }
    }
}

/// Close a truncated JSON object: keep the deepest complete prefix (a
/// value that finished, or a nested structure that closed) and append
/// whatever closers the bracket stack still owes. Candidates are tried
/// deepest-first, so the smallest loss wins.
pub fn salvage_json(s: &str) -> Option<serde_json::Value> {
    let start = s.find('{')?;
    let bytes: Vec<char> = s[start..].chars().collect();
    let mut stack: Vec<char> = vec![];
    let mut in_str = false;
    let mut esc = false;
    let mut token = false; // inside a number/true/false/null run
    let mut prev_sig: Option<char> = None; // last significant char outside strings
    let mut str_open: Option<char> = None; // char before a string's opening quote
    // (chars kept, bracket stack snapshot at that point)
    let mut cands: Vec<(usize, Vec<char>)> = vec![];
    for (i, &ch) in bytes.iter().enumerate() {
        if in_str {
            if esc {
                esc = false;
            } else if ch == '\\' {
                esc = true;
            } else if ch == '"' {
                in_str = false;
                token = false;
                // a finished string is a value only when a ':' opened it —
                // otherwise it is a key and the prefix would be unparseable
                if str_open == Some(':') {
                    cands.push((i + 1, stack.clone()));
                }
            }
            continue;
        }
        match ch {
            '"' => {
                in_str = true;
                str_open = prev_sig;
                token = false;
            }
            '{' | '[' => {
                stack.push(ch);
                token = false;
            }
            '}' | ']' => {
                stack.pop();
                token = false;
                if stack.is_empty() {
                    let body: String = bytes[..=i].iter().collect();
                    return serde_json::from_str(&body).ok();
                }
                cands.push((i + 1, stack.clone()));
            }
            c if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' || c.is_ascii_alphabetic() => {
                token = true; // number/const run still going
            }
            _ => {
                if token {
                    cands.push((i, stack.clone()));
                    token = false;
                }
            }
        }
        if !ch.is_whitespace() {
            prev_sig = Some(ch);
        }
    }
    if token {
        cands.push((bytes.len(), stack.clone()));
    }
    for (cut, snap) in cands.iter().rev().take(200) {
        let mut body: String = bytes[..*cut].iter().collect();
        for opener in snap.iter().rev() {
            body.push(if *opener == '{' { '}' } else { ']' });
        }
        if let Ok(v) = serde_json::from_str(&body) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::salvage_json;

    #[test]
    fn salvages_truncated_json() {
        let cases: &[(&str, &str)] = &[
            // cut mid string value inside a nested array
            (r#"{"verdicts":[{"claim":"foo","verdict":"sup"#, "verdicts"),
            // cut mid key at root: the half-written key drops, members survive
            (r#"{"a":1,"b":[1,2],"over"#, r#""b":[1,2]"#),
            // cut mid key right after a nested object closed
            (r#"{"verdicts":[{"claim":"foo"}],"comm"#, "verdicts"),
            // cut mid number
            (r#"{"n":12"#, "n"),
            // cut right after a colon (value never started)
            (r#"{"a":"x","b":"#, "a"),
            // balanced input parses unchanged
            (r#"{"ok":true}"#, "ok"),
            // prose preamble before the object
            (r#"Sure! {"a":1"#, "a"),
        ];
        for (input, must_have) in cases {
            let v = salvage_json(input).unwrap_or_else(|| panic!("no salvage for {input:?}"));
            assert!(
                v.to_string().contains(must_have),
                "{input:?} salvaged to {v} (want {must_have:?})"
            );
        }
    }

    #[test]
    fn garbage_stays_none() {
        assert!(salvage_json("no json here").is_none());
        assert!(salvage_json("{").is_none());
    }
}
