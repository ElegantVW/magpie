//! Shared local-LLM plumbing: menagerie-routed endpoint + JSON-enforced calls.
//! Used by the query-expansion pass (plan time) and the per-entry pass
//! (process time). No cloud, no keys — house control plane only.

pub fn llm_app() -> String {
    std::env::var("MAGPIE_LLM_APP").unwrap_or_else(|_| "magpie".into())
}

/// Resolve the inference endpoint through menagerie (the house control
/// plane): ensure the app slot, then read its URL. Falls back to
/// PIXIE_LLM_URL, then pixie's :8080. Returns (url, via_tag).
/// Backend (cpu/gpu) is configured per-creature in the steward
/// (`menagerie backend magpie gpu`) — magpie passes no GPU env of its own.
pub async fn llm_endpoint() -> (String, String) {
    let app = llm_app();
    let mut cmd = tokio::process::Command::new("menagerie");
    cmd.args(["ensure", &app]);
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
/// then raw content JSON. Unwraps a top-level "want" object: small models
/// echo the user message's want-block instead of the tool schema
/// ({"want": {"kind":..,"relevance":25,..}} — balanced, misplaced, and
/// once silently dropped 16 pages into heuristic fallbacks).
/// A legitimate answer never carries top-level "want" (not in the schema),
// so the unwrap cannot misfire on good output.
pub fn extract_json(txt: &str) -> Option<(serde_json::Value, bool)> {
    let root: serde_json::Value = serde_json::from_str(txt).ok()?;
    if let Some(s) = root["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"].as_str() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
            return Some((unwrap_want(v), true));
        }
    }
    let content = root["choices"][0]["message"]["content"].as_str().unwrap_or_default();
    let v: serde_json::Value = serde_json::from_str(content.trim()).ok()?;
    if v.get("name").is_some() && v.get("arguments").is_some() {
        return Some((unwrap_want(v["arguments"].clone()), true));
    }
    Some((unwrap_want(v), false))
}

/// Descend through task-echo wrappers ("want", and nothing else).
fn unwrap_want(v: serde_json::Value) -> serde_json::Value {
    const KEYS: &[&str] = &["kind", "bullets", "specs", "quotes", "relevance", "section"];
    let mut cur = v;
    loop {
        let has_inner = cur.get("want").map(|w| w.is_object()).unwrap_or(false);
        let has_own = KEYS.iter().any(|k| cur.get(k).is_some());
        if has_inner && !has_own {
            cur = cur["want"].clone();
        } else {
            return cur;
        }
    }
}

/// What one enforced-JSON round trip yields: the parsed value, how it
/// arrived, and the envelope forensics (finish reason, token counts, raw
/// model text). The forensics travel even on failure paths — truncation
/// diagnosis needs them exactly when parsing fails.
pub struct ChatOut {
    pub v: serde_json::Value,
    pub tooled: bool,
    pub finish: String,
    pub comp_tokens: u64,
    pub raw: String,
}

/// Server context window magpie is served with (menagerie registers every
/// app at ctx=8192). Prompts are clamped to fit it — a request over the
/// window comes back with an EMPTY completion, not an error, which is how
/// 98 "Context size has been exceeded" events silently emptied chunks.
const CTX_TOKENS: usize = 8192;
/// ~3.6 chars/token for the Qwen3 family; deliberately pessimistic so the
/// clamp errs toward a shorter prompt rather than a silent overflow.
const CHARS_PER_TOKEN: usize = 3;

/// Byte index `i` snapped down to a UTF-8 char boundary — slicing text at an
/// arbitrary byte panics on multi-byte content (em-dashes, CJK, accented
/// place names all appear in these chunks).
fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Convenience: system + user + enforced JSON object, parsed or None.
pub fn fit_user(sys: &str, user: &str, max_tokens: u32) -> String {
    // clamp the user turn to fit: window - reply budget - system - slack.
    // head+tail (never head-only: chapter headings and closing specs live
    // at the end of a chunk and the model needs them).
    let sys_tok = sys.len() / CHARS_PER_TOKEN + 16;
    let room = CTX_TOKENS
        .saturating_sub(max_tokens as usize + sys_tok + 64)
        .max(256);
    let budget = room * CHARS_PER_TOKEN;
    if user.len() <= budget {
        return user.to_string();
    }
    let head = budget * 3 / 5;
    let tail = budget - head;
    let cut = floor_char_boundary(user, head);
    let tstart = floor_char_boundary(user, user.len() - tail);
    format!(
        "{}\n[...{} chars elided: prompt fitted to the {}-token window...]\n{}",
        &user[..cut],
        user.len() - tail - cut,
        CTX_TOKENS,
        &user[tstart..]
    )
}

pub async fn chat_json(
    client: &reqwest::Client,
    endpoint: &str,
    sys: &str,
    user: &str,
    max_tokens: u32,
    temperature: f32,
) -> Option<ChatOut> {
    let user = &fit_user(sys, user, max_tokens);
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
    let root: serde_json::Value = serde_json::from_str(&txt).unwrap_or(serde_json::Value::Null);
    let finish = root["choices"][0]
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("")
        .to_string();
    let comp_tokens = root["usage"].get("completion_tokens").and_then(|n| n.as_u64()).unwrap_or(0);
    let raw = root["choices"][0]["message"]
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string();
    let mk = |v: serde_json::Value, tooled: bool| ChatOut {
        v,
        tooled,
        finish: finish.clone(),
        comp_tokens,
        raw: raw.clone(),
    };
    match extract_json(&txt) {
        Some((v, tooled)) => Some(mk(v, tooled)),
        // small models truncate mid-object when the token budget runs out;
        // salvage the balanced prefix rather than losing the whole answer.
        // Unsalvageable content logs its shape (finish/tokens/head): a
        // no-response row must say whether the model said nothing or said
        // it wrong (that blind spot once hid 3 timeouts among 16 rows).
        None => {
            if raw.trim().is_empty() {
                eprintln!("llm: empty model content (finish={finish} toks={comp_tokens})");
                return None;
            }
            match salvage_json(&raw) {
                Some(v) => Some(mk(v, false)),
                None => {
                    eprintln!(
                        "llm: unparseable model content (finish={finish} toks={comp_tokens}): {}",
                        raw.chars().take(160).collect::<String>()
                    );
                    None
                }
            }
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

#[cfg(test)]
mod extract_tests {
    use super::extract_json;

    fn envelope(content: &str) -> String {
        serde_json::json!({"choices": [{"message": {"content": content}}]}).to_string()
    }

    #[test]
    fn unwraps_want_echo() {
        // the de Mahieu failure shape: balanced, misplaced, score inside
        let out = extract_json(&envelope(r#"{"want": {"kind": "facts", "relevance": 25, "section": "Facts", "bullets": ["x"], "specs": [], "quotes": []}}"#))
            .expect("must parse");
        assert_eq!(out.0.get("relevance").and_then(|r| r.as_i64()), Some(25));
        assert_eq!(out.0.get("kind").and_then(|k| k.as_str()), Some("facts"));
    }

    #[test]
    fn legit_answers_untouched() {
        // no top-level want: returned as-is (both envelope and raw paths)
        let good = r#"{"kind": "history", "relevance": 95, "section": "History", "bullets": [], "specs": [], "quotes": []}"#;
        let out = extract_json(&envelope(good)).expect("must parse");
        assert_eq!(out.0.get("relevance").and_then(|r| r.as_i64()), Some(95));
        // a hypothetical answer that BOTH scores and (oddly) carries want:
        // own keys win, no descend
        let both = r#"{"kind": "facts", "relevance": 80, "want": {"kind": "other"}}"#;
        let out2 = extract_json(&envelope(both)).expect("must parse");
        assert_eq!(out2.0.get("kind").and_then(|k| k.as_str()), Some("facts"));
    }
}

#[cfg(test)]
mod ctx_tests {
    use super::{floor_char_boundary, CHARS_PER_TOKEN, CTX_TOKENS};

    #[test]
    fn boundary_snaps_to_char_start() {
        let s = "aé€b"; // 1+2+3+1 bytes
        for i in 0..=s.len() {
            assert!(s.is_char_boundary(floor_char_boundary(s, i)), "byte {i}");
        }
        assert_eq!(floor_char_boundary("abc", 99), 3);
    }

    #[test]
    fn budget_leaves_room_for_the_reply() {
        // worst case: system at 4096 chars, reply 1024 tokens
        let sys_tok = 4096 / CHARS_PER_TOKEN + 16;
        let room = CTX_TOKENS.saturating_sub(1024 + sys_tok + 64);
        let total_tok = sys_tok + room + 1024;
        assert!(total_tok < CTX_TOKENS, "{total_tok} must fit {CTX_TOKENS}");
        assert!(room >= 256, "floor keeps a usable prompt");
    }
}

#[cfg(test)]
mod fit_tests {
    use super::fit_user;

    #[test]
    fn short_prompts_pass_through_untouched() {
        assert_eq!(fit_user("sys", "hello", 400), "hello");
    }

    #[test]
    fn giant_facts_blob_is_fitted_and_marked() {
        let blob = "x".repeat(200_000);
        let out = fit_user("s", &blob, 400);
        assert!(out.len() < 30_000, "{}", out.len());
        assert!(out.contains("elided"), "the model must know it lost text");
        assert!(out.starts_with('x') && out.ends_with('x'));
    }

    #[test]
    fn multibyte_tail_slice_is_valid_utf8() {
        let blob = "é".repeat(100_000); // 2 bytes per char
        let out = fit_user("s", &blob, 400);
        assert!(out.is_char_boundary(out.len()));
    }
}
