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
pub async fn chat_once(client: &reqwest::Client, url: &str, body: &serde_json::Value) -> std::result::Result<String, String> {
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
    let txt = chat_once(client, &url, &body).await.ok()?;
    extract_json(&txt)
}
