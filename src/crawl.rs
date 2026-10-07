use crate::error::{MagpieError, Result};
use regex::Regex;
use std::collections::HashSet;
use std::time::Duration;

const UA: &str = "duck/3.0 (terminal search; +https://github.com/jarun/ddgr)";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Page {
    pub url: String,
    pub title: String,
    pub text: String,
    pub chars: usize,
    /// Knowledge layer: Some(keeper_index) when this page near-duplicates
    /// another (locale mirror, syndication). Keepers carry the facts;
    /// dups corroborate them. Pixie pass skips dups (saves compute).
    #[serde(default)]
    pub dup_of: Option<usize>,
}

fn client() -> reqwest::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .user_agent(UA)
                .timeout(Duration::from_secs(12))
                .build()
                .expect("client")
        })
        .clone()
}

/// Query card: what the query MEANS, before any searching.
/// Wikidata (label + description + aliases + instance) and Wiktionary
/// (definitions per sense) — both keyless. Every downstream gate and the
/// relevance scorer work from this card, not from raw spelling.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct QueryCard {
    pub subject: String,
    pub meaning: String,
    pub aliases: Vec<String>,
    pub instance: String,
}

pub async fn query_card(query: &str) -> QueryCard {
    let c = client();
    let mut card = QueryCard { subject: query.to_string(), ..Default::default() };
    // wikidata entity: label, description, aliases, instance
    if let Ok(s) = c
        .get(&format!(
            "https://www.wikidata.org/w/api.php?action=wbsearchentities&search={}&language=en&format=json&limit=1",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        ))
        .send()
        .await
    {
        if let Ok(t) = s.text().await {
            if let Some(id) = Regex::new(r#""id":"(Q\d+)""#)
                .unwrap()
                .captures(&t)
                .map(|c| c[1].to_string())
            {
                let url = format!("https://www.wikidata.org/w/api.php?action=wbgetentities&ids={id}&props=labels%7Cdescriptions%7Caliases%7Cclaims&languages=en&format=json");
                if let Ok(r) = c.get(&url).send().await {
                    if let Ok(t) = r.text().await {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                            if let Some(e) = v.get("entities").and_then(|e| e.get(&id)) {
                                let pick = |o: Option<&str>| o.unwrap_or("").to_string();
                                card.subject = pick(e.get("labels").and_then(|l| l.get("en")).and_then(|l| l.get("value")).and_then(|x| x.as_str()));
                                if card.subject.is_empty() {
                                    card.subject = query.to_string();
                                }
                                card.meaning = pick(e.get("descriptions").and_then(|l| l.get("en")).and_then(|l| l.get("value")).and_then(|x| x.as_str()));
                                if let Some(arr) = e.get("aliases").and_then(|a| a.get("en")).and_then(|a| a.as_array()) {
                                    for a in arr.iter().take(8) {
                                        if let Some(s) = a.get("value").and_then(|x| x.as_str()) {
                                            if !s.is_empty() {
                                                card.aliases.push(s.to_string());
                                            }
                                        }
                                    }
                                }
                                // instance line for domain detection
                                if let Some(arr) = e.get("claims").and_then(|c| c.get("P31")).and_then(|s| s.as_array()) {
                                    let mut inst = vec![];
                                    for st in arr.iter().take(3) {
                                        if let Some(q) = st.get("mainsnak").and_then(|m| m.get("datavalue")).and_then(|d| d.get("value")).and_then(|x| x.get("id")).and_then(|x| x.as_str()) {
                                            inst.push(q.to_string());
                                        }
                                    }
                                    card.instance = inst.join(",");
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // wiktionary: definitions per sense for single-word queries
    if query.split_whitespace().count() == 1 {
        let url = format!(
            "https://en.wiktionary.org/w/api.php?action=query&prop=extracts&explaintext=1&titles={}&format=json",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        );
        if let Ok(r) = c.get(&url).send().await {
            if let Ok(t) = r.text().await {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                    if let Some(pages) = v.get("query").and_then(|q| q.get("pages")).and_then(|p| p.as_object()) {
                        for (_, p) in pages {
                            if let Some(x) = p.get("extract").and_then(|e| e.as_str()) {
                                // definitions only: drop ==section== headers and
                                // stray separators leaking from the extract
                                let def: String = x
                                    .split('\n')
                                    .map(|l| l.trim())
                                    .filter(|l| {
                                        !l.is_empty()
                                            && !(l.starts_with("==") && l.ends_with("=="))
                                            && *l != "/"
                                    })
                                    .take(4)
                                    .collect::<Vec<_>>()
                                    .join(" / ")
                                    .chars()
                                    .take(400)
                                    .collect();
                                if !def.is_empty() {
                                    if !card.meaning.is_empty() {
                                        card.meaning.push_str(" | ");
                                    }
                                    card.meaning.push_str(&def);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    card
}

fn clean_ws(s: &str) -> String {
    let re = Regex::new(r"\s+").unwrap();
    re.replace_all(s.trim(), " ").to_string()
}

fn strip_tags(html: &str) -> String {
    // readability-lite: drop chrome elements wholesale (nav, menus,
    // footers pollute every page and dilute both Jaccard and LLM context),
    // then keep only dense blocks. (No backrefs: the regex crate forbids
    // them, so one pattern per element.)
    let mut t = html.to_string();
    for tag in ["header", "nav", "footer", "aside", "form", "menu", "noscript", "svg", "iframe"] {
        let pat = format!(r"(?is)<{tag}[^>]*>.*?</{tag}>");
        t = Regex::new(&pat).unwrap().replace_all(&t, " ").to_string();
    }
    let re_script = Regex::new(r"(?is)<script.*?</script>").unwrap();
    let re_style = Regex::new(r"(?is)<style.*?</style>").unwrap();
    let re_block = Regex::new(r"(?is)</?(p|div|br|tr|li|h1|h2|h3|h4|table|article|section)[^>]*>").unwrap();
    let t = re_script.replace_all(&t, " ");
    let t = re_style.replace_all(&t, " ");
    let t = re_block.replace_all(&t, "\n");
    let re_tags = Regex::new(r"(?s)<[^>]+>").unwrap();
    let t = re_tags.replace_all(&t, " ");
    // keep newlines, collapse spaces per line
    let decoded = html_escape::decode_html_entities(&t);
    let ws = Regex::new(r"[ \t]+").unwrap();
    decoded
        .split('\n')
        .map(|l| ws.replace_all(l.trim(), " ").to_string())
        .filter(|l| {
            if l.is_empty() {
                return false;
            }
            let words = l.split_whitespace().count();
            // dense prose passes; short headings pass unless menu-like
            words >= 8 || (l.len() < 120 && words >= 3 && !l.contains('|'))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn safe_truncate(s: &str, n_chars: usize) -> String {
    if s.chars().count() <= n_chars {
        return s.to_string();
    }
    s.chars().take(n_chars).collect()
}

pub fn extract_text(html: &str) -> (String, String) {
    let re_title = Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap();
    let title = re_title
        .captures(html)
        .map(|c| clean_ws(&html_escape::decode_html_entities(&c[1])))
        .unwrap_or_default();
    let t = strip_tags(html);
    (safe_truncate(&title, 300), safe_truncate(&t, 12000))
}

async fn fetch_one(url: &str) -> Result<Page> {
    let c = client();
    let r = c
        .get(url)
        .send()
        .await
        .map_err(|e| MagpieError::Network(e.to_string()))?;
    if !r.status().is_success() {
        return Err(MagpieError::Network(format!("{} {}", r.status(), url)));
    }
    let html = r
        .text()
        .await
        .map_err(|e| MagpieError::Network(e.to_string()))?;
    let (title, text) = extract_text(&html);
    let chars = text.chars().count();
    Ok(Page {
        url: url.to_string(),
        title,
        text: text.clone(),
        chars,
        dup_of: None,
    })
}

fn extract_hrefs(html: &str) -> Vec<String> {
    let re = Regex::new(r#"(?i)href\s*=\s*"([^"]+)""#).unwrap();
    re.captures_iter(html)
        .map(|c| c[1].to_string())
        .collect()
}

fn unwrap_ddg(href: &str) -> String {
    // //duckduckgo.com/l/?uddg=<enc>&rut=...  -> real target
    if href.contains("duckduckgo.com") && href.contains("uddg=") {
        if let Some(pos) = href.find("uddg=") {
            let enc = &href[pos + 5..];
            let end = enc.find('&').unwrap_or(enc.len());
            return percent_decode(&enc[..end]);
        }
    }
    href.to_string()
}

async fn ddg_search(query: &str, n: usize) -> Vec<String> {
    ddg_search_tries(query, n, 3, 20).await
}

async fn ddg_search_tries(query: &str, n: usize, tries: usize, wait_s: u64) -> Vec<String> {
    // POST like python magpie: q, kl=us-en, df=, s=0.
    // Retried with backoff: throttled engines recover if out-waited,
    // and runtime no longer matters more than coverage.
    for attempt in 0..tries.max(1) {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(wait_s)).await;
        }
        let out = ddg_once(query, n).await;
        if !out.is_empty() || attempt + 1 >= tries.max(1) {
            return out;
        }
    }
    vec![]
}

async fn ddg_once(query: &str, n: usize) -> Vec<String> {
    // POST like python magpie: q, kl=us-en, df=, s=0
    let c = client();
    let params = [("q", query), ("kl", "us-en"), ("df", ""), ("s", "0")];
    let html = match c.post("https://html.duckduckgo.com/html/").form(&params).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    if html.to_lowercase().contains("anomaly") && !html.contains("result__a") {
        return vec![];
    }
    let re = Regex::new(r#"class="result__a"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap();
    let mut out = vec![];
    for cap in re.captures_iter(&html) {
        let u = unwrap_ddg(&cap[1]);
        if u.starts_with("http") {
            out.push(u);
        }
        if out.len() >= n {
            break;
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    // percent-decode to bytes first, then UTF-8 lossy (char-boundary safe)
    let bytes = s.as_bytes();
    let mut buf: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                buf.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            buf.push(b' ');
        } else {
            buf.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&buf).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

async fn marginalia_search(query: &str, n: usize) -> Vec<String> {
    marginalia_tries(query, n, 3, 20).await
}

async fn marginalia_tries(query: &str, n: usize, tries: usize, wait_s: u64) -> Vec<String> {
    for attempt in 0..tries.max(1) {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(wait_s)).await;
        }
        let out = marginalia_once(query, n).await;
        if !out.is_empty() || attempt + 1 >= tries.max(1) {
            return out;
        }
    }
    vec![]
}

async fn marginalia_once(query: &str, n: usize) -> Vec<String> {
    // old text-friendly UI like python magpie (new UI is JS-only)
    let c = client();
    let url = format!(
        "https://old-search.marginalia.nu/search?query={}&offset=0",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    let html = match c.get(&url).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    // sections first, else whole page
    let re_sec = Regex::new(r#"(?is)<section[^>]*class="[^"]*search-result[^"]*"[^>]*>(.*?)</section>"#).unwrap();
    let blocks: Vec<String> = {
        let v: Vec<String> = re_sec.captures_iter(&html).map(|c| c[1].to_string()).collect();
        if v.is_empty() { vec![html.clone()] } else { v }
    };
    let re_a = Regex::new(r#"(?i)<a[^>]+href="(https?://[^"]+)"[^>]*>(.*?)</a>"#).unwrap();
    let re_tag = Regex::new(r"(?s)<[^>]+>").unwrap();
    let mut out = vec![];
    for b in blocks {
        for cap in re_a.captures_iter(&b) {
            let h = cap[1].to_string();
            if h.contains("marginalia") {
                continue;
            }
            let t = clean_ws(&re_tag.replace_all(&cap[2], " "));
            if t.is_empty() || t.starts_with("http") {
                continue;
            }
            out.push(h);
            break;
        }
        if out.len() >= n {
            break;
        }
    }
    // fallback: any http links
    if out.is_empty() {
        for h in extract_hrefs(&html) {
            if h.starts_with("http") && !h.contains("marginalia") {
                out.push(h);
            }
            if out.len() >= n {
                break;
            }
        }
    }
    out.truncate(n);
    out
}

/// Marque aliases: the same car hides under other badges
/// (Opel↔Vauxhall, Chevrolet↔Holden). Fan-out finds those pages.
fn alias_queries(query: &str) -> Vec<String> {
    const PAIRS: &[(&str, &str)] = &[
        ("opel", "vauxhall"),
        ("vauxhall", "opel"),
        ("chevrolet", "holden"),
        ("holden", "chevrolet"),
    ];
    let lq = query.to_lowercase();
    let mut out = vec![];
    for (a, b) in PAIRS {
        if lq.contains(a) {
            out.push(lq.replacen(a, b, 1));
        }
    }
    out
}

/// Producer outline, step 1 — subject domain from the query + the
/// wikidata instance line. Deterministic and offline-first; no LLM needed.
pub fn subject_domain(query: &str, instance_of: &str) -> String {
    let lq = query.to_lowercase();
    let inst = instance_of.to_lowercase();
    // vehicle-machine: engine codes, units, years with model tokens
    let tokens: Vec<&str> = lq
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let techy = tokens.iter().any(|t| {
        t.chars().any(|c| c.is_ascii_digit()) && t.len() <= 6
            || ["tdci", "cdti", "hdi", "jtd", "tdi", "ps", "hp", "kw", "cc", "4x4", "awd", "fwd", "v6", "v8"].contains(t)
    });
    for place in ["country", "sovereign state", "city", "island", "capital", "region", "province", "municipality", "district"] {
        if inst.contains(place) {
            return "place".into();
        }
    }
    if techy {
        return "vehicle".into();
    }
    for v in ["car model", "automobile", "motor vehicle", "aircraft", "ship ", "locomotive", "engine"] {
        if inst.contains(v) {
            return "vehicle".into();
        }
    }
    if inst.is_empty() {
        return "other".into();
    }
    "field".into()
}

/// Producer outline, step 2 — chapters per domain. The manual renders
/// these in order with explicit gap flags where coverage failed.
pub fn chapters(domain: &str) -> Vec<String> {
    match domain {
        "place" => ["History", "Geography", "Cities & regions", "Cuisine & wine", "Music & arts", "Economy"]
            .iter().map(|s| s.to_string()).collect(),
        "vehicle" => ["Specifications", "Faults & reliability", "Reviews & driving", "Dimensions & practicality"]
            .iter().map(|s| s.to_string()).collect(),
        "field" => ["Canon & books", "Techniques", "History", "Key figures"]
            .iter().map(|s| s.to_string()).collect(),
        _ => ["Facts", "Reference"].iter().map(|s| s.to_string()).collect(),
    }
}

/// Chapter queries fanned out as real searches (replaces the fixed
/// specs/problems pair with domain-aware coverage).
pub fn chapter_queries(query: &str, domain: &str) -> Vec<String> {
    let parts: Vec<&str> = match domain {
        "place" => vec!["history", "cuisine food", "music culture", "geography cities"],
        "vehicle" => vec!["specifications", "problems faults", "review"],
        "field" => vec!["techniques", "books canon", "history"],
        _ => vec!["specifications", "problems"],
    };
    parts.iter().map(|c| format!("{query} {c}")).collect()
}

/// Internet Archive books: public-domain/open full text only.
/// advancedsearch → metadata (confirm `_djvu.txt` exists) → download.
/// Lending-library and restricted items never qualify — listed, never fetched.
async fn ia_books(query: &str, prog: &Option<tokio::sync::mpsc::UnboundedSender<String>>, max_books: usize) -> Vec<Page> {
    let send = |m: &str| {
        if let Some(tx) = prog {
            let _ = tx.send(m.to_string());
        }
    };
    send("search: archive.org books…");
    let c = client();
    let q = format!(
        "https://archive.org/advancedsearch.php?q={}+AND+mediatype%3Atexts&fl%5B%5D=identifier&fl%5B%5D=title&fl%5B%5D=year&rows=12&output=json",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    let txt = match c.get(&q).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let v: serde_json::Value = match serde_json::from_str(&txt) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let docs = v["response"]["docs"].as_array().cloned().unwrap_or_default();
    let mut out = vec![];
    for d in docs {
        if out.len() >= max_books.max(1) {
            break;
        }
        let id = d.get("identifier").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let title = d.get("title").and_then(|x| x.as_str()).unwrap_or(&id).to_string();
        if id.is_empty() {
            continue;
        }
        // metadata: only proceed with a real full-text file present.
        // NOTE: newer items name files by title, not identifier —
        // resolve the exact _djvu.txt from the file list, never guess.
        let meta = match c.get(&format!("https://archive.org/metadata/{id}")).send().await {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(_) => continue,
        };
        let txt_name: Option<String> = (|| {
            let v: serde_json::Value = serde_json::from_str(&meta).ok()?;
            let mut cands: Vec<String> = vec![];
            for f in v.get("files")?.as_array()? {
                let n = f.get("name")?.as_str()?;
                if n.ends_with("_djvu.txt") {
                    cands.push(n.to_string());
                }
            }
            // prefer the identifier-stemmed file, else first candidate
            cands
                .iter()
                .find(|n| n.starts_with(&id))
                .cloned()
                .or_else(|| cands.into_iter().next())
        })();
        let txt_name = match txt_name {
            Some(n) => n,
            None => continue,
        };
        let body = match c.get(&format!("https://archive.org/download/{id}/{txt_name}")).send().await {
            Ok(r) => {
                // restricted/lending items answer 403 with an error page —
                // never ingest those as book text
                if !r.status().is_success() {
                    continue;
                }
                r.text().await.unwrap_or_default()
            }
            Err(_) => continue,
        };
        if body.contains("Page not found on Internet Archive") {
            continue;
        }
        // full text, not a sample: books are chunked downstream, and
        // runtime no longer matters more than coverage (cap ~400KB)
        let text: String = body.chars().take(400_000).collect();
        if text.chars().count() < 2000 {
            continue;
        }
        // content-density gate: the book must actually be ABOUT the query
        // (title matches lie — cf. poetry volumes). Stemmed: "hermeticism"
        // must match "hermetic philosophy" (Kybalion class of misses).
        let stem = |t: &str| -> String {
            let mut s = t.to_string();
            for suf in ["icism", "ism", "ists", "ist", "tion", "ing", "ed", "es", "s"] {
                if s.len() > suf.len() + 3 && s.ends_with(suf) {
                    s.truncate(s.len() - suf.len());
                    break;
                }
            }
            s
        };
        let lt = text.to_lowercase();
        let toks: Vec<String> = query
            .to_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|t| t.len() > 2)
            .map(|t| t.to_string())
            .collect();
        let stems: Vec<String> = toks.iter().map(|t| stem(t)).collect();
        let hits: usize = toks
            .iter()
            .chain(stems.iter())
            .map(|t| lt.matches(&t[..]).count())
            .sum();
        // title-or-lead gate: at least one query token in the title or
        // the first 1500 chars (front matter). Passing mentions deep in
        // a Texas file or a Hong Kong newspaper never qualify.
        // A strong title match halves the bar, so non-English books
        // (Portuguese cookbooks) survive on few English tokens.
        let head: String = text.chars().take(1500).collect::<String>().to_lowercase();
        let titled = title.to_lowercase();
        let all: Vec<&String> = toks.iter().chain(stems.iter()).collect();
        let titled_hit = all.iter().any(|t| titled.contains(&t[..]) || head.contains(&t[..]));
        if !titled_hit {
            continue;
        }
        let title_strong = all.iter().any(|t| titled.contains(&t[..]));
        if hits < if title_strong { 2 } else { 4 } {
            continue;
        }
        let chars = text.chars().count();
        out.push(Page {
            url: format!("https://archive.org/details/{id}"),
            title: format!("{title} — Internet Archive"),
            text,
            chars,
            dup_of: None,
        });
        send(&format!("search: archive.org got {title}"));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    out
}

/// Wikisource: full text of public-domain works (same canon pool as IA).
async fn wikisource_search(query: &str) -> Vec<Page> {
    let c = client();
    let s = match c
        .get(&format!(
            "https://en.wikisource.org/w/api.php?action=query&list=search&srsearch={}&format=json&srlimit=4",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        ))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let v: serde_json::Value = serde_json::from_str(&s).unwrap_or(serde_json::Value::Null);
    let hits = v["query"]["search"].as_array().cloned().unwrap_or_default();
    let mut out = vec![];
    for h in hits {
        let title = h.get("title").and_then(|t| t.as_str()).unwrap_or("").to_string();
        if title.is_empty() {
            continue;
        }
        let t = match c
            .get(&format!(
                "https://en.wikisource.org/w/api.php?action=query&prop=extracts&explaintext=1&titles={}&format=json",
                url::form_urlencoded::byte_serialize(title.as_bytes()).collect::<String>()
            ))
            .send()
            .await
        {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(_) => continue,
        };
        let v: serde_json::Value = serde_json::from_str(&t).unwrap_or(serde_json::Value::Null);
        let mut text = String::new();
        if let Some(pages) = v["query"]["pages"].as_object() {
            for (_, p) in pages {
                if let Some(x) = p.get("extract").and_then(|e| e.as_str()) {
                    text.push_str(x);
                }
            }
        }
        let text: String = text.chars().take(200_000).collect();
        if text.chars().count() < 500 {
            continue;
        }
        // Wikisource extracts arrive whole; chunking happens downstream
        let chars = text.chars().count();
        out.push(Page {
            url: format!(
                "https://en.wikisource.org/wiki/{}",
                title.replace(' ', "_")
            ),
            title: format!("{title} — Wikisource"),
            text,
            chars,
            dup_of: None,
        });
        if out.len() >= 2 {
            break;
        }
    }
    out
}

/// TheCocktailDB: structured drink canon (ingredients/measures/method).
/// Fires only when it actually has the drink (null otherwise) — cheap probe.
async fn cocktaildb(query: &str) -> Vec<Page> {
    let c = client();
    let mut tries = vec![query.to_string()];
    let longest: Option<String> = query
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 3)
        .max_by_key(|t| t.len())
        .map(|t| t.to_string());
    if let Some(l) = longest {
        if l.to_lowercase() != query.to_lowercase() {
            tries.push(l);
        }
    }
    for q in tries {
        let url = format!(
            "https://www.thecocktaildb.com/api/json/v1/1/search.php?s={}",
            url::form_urlencoded::byte_serialize(q.as_bytes()).collect::<String>()
        );
        let txt = match c.get(&url).send().await {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(_) => continue,
        };
        let v: serde_json::Value = match serde_json::from_str(&txt) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let drinks = v.get("drinks").and_then(|d| d.as_array()).cloned().unwrap_or_default();
        if drinks.is_empty() {
            continue;
        }
        let mut out = vec![];
        for d in drinks.iter().take(4) {
            let name = d.get("strDrink").and_then(|x| x.as_str()).unwrap_or("drink").to_string();
            let mut lines = vec![format!("{name} (TheCocktailDB)")];
            for k in ["strCategory", "strGlass", "strInstructions"] {
                if let Some(x) = d.get(k).and_then(|x| x.as_str()) {
                    if !x.is_empty() {
                        lines.push(format!("{}: {}", k.trim_start_matches("str"), x.chars().take(400).collect::<String>()));
                    }
                }
            }
            for i in 1..=8 {
                let ing = d.get(&format!("strIngredient{i}")).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
                let mea = d.get(&format!("strMeasure{i}")).and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
                if ing.is_empty() {
                    continue;
                }
                lines.push(format!("- {} {}", ing, mea.trim_end()));
            }
            let text = lines.join("\n");
            let chars = text.chars().count();
            out.push(Page {
                url: format!("https://www.thecocktaildb.com/drink/{name}"),
                title: format!("{name} — CocktailDB"),
                text,
                chars,
                dup_of: None,
            });
        }
        if !out.is_empty() {
            return out;
        }
    }
    vec![]
}

/// Open Library: edition metadata for the bibliography (author/year/publisher).
async fn openlibrary(query: &str) -> Vec<Page> {
    let c = client();
    let txt = match c
        .get(&format!(
            "https://openlibrary.org/search.json?q={}&limit=4&fields=title,author_name,first_publish_year,publisher,key",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        ))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or(serde_json::Value::Null);
    let docs = v.get("docs").and_then(|d| d.as_array()).cloned().unwrap_or_default();
    if docs.is_empty() {
        return vec![];
    }
    let mut lines = vec![format!("Open Library editions for “{query}” (bibliography):")];
    for d in docs.iter().take(4) {
        let t = d.get("title").and_then(|x| x.as_str()).unwrap_or("?");
        let a = d
            .get("author_name")
            .and_then(|x| x.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))
            .unwrap_or_else(|| "?".into());
        let y = d.get("first_publish_year").map(|x| x.to_string()).unwrap_or_else(|| "?".into());
        let k = d.get("key").and_then(|x| x.as_str()).unwrap_or("");
        lines.push(format!("- {t} — {a} ({y}) https://openlibrary.org{k}"));
    }
    let text = lines.join("\n");
    let chars = text.chars().count();
    vec![Page {
        url: format!("https://openlibrary.org/search?q={query}"),
        title: format!("Open Library: {query}"),
        text,
        chars,
        dup_of: None,
    }]
}

/// Wikidata entity traversal: top entities for the query contribute their
/// article sitelinks as seed URLs (finds "Cocktail" from "mixology").
async fn wiki_related(query: &str) -> Vec<String> {
    let c = client();
    let s = match c
        .get(&format!(
            "https://www.wikidata.org/w/api.php?action=wbsearchentities&search={}&language=en&format=json&limit=5",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        ))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let ids: Vec<String> = Regex::new(r#""id":"(Q\d+)""#)
        .unwrap()
        .captures_iter(&s)
        .map(|c| c[1].to_string())
        .take(5)
        .collect();
    if ids.is_empty() {
        return vec![];
    }
    let e = match c
        .get(&format!(
            "https://www.wikidata.org/w/api.php?action=wbgetentities&ids={}&props=sitelinks&sitefilter=enwiki&format=json",
            ids.join("|")
        ))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let v: serde_json::Value = serde_json::from_str(&e).unwrap_or(serde_json::Value::Null);
    let mut out = vec![];
    if let Some(ents) = v.get("entities").and_then(|e| e.as_object()) {
        for ent in ents.values() {
            if let Some(title) = ent
                .get("sitelinks")
                .and_then(|s| s.get("enwiki"))
                .and_then(|s| s.get("title"))
                .and_then(|t| t.as_str())
            {
                out.push(format!("https://en.wikipedia.org/wiki/{}", title.replace(' ', "_")));
                if out.len() >= 4 {
                    break;
                }
            }
        }
    }
    out
}

fn wikipedia_api(query: &str) -> String {
    format!(
        "https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&format=json&srlimit=5",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    )
}

/// Wikidata: structured facts via API (search → claims → label resolve).
/// One synthetic Page, always reachable, no key. Tries progressively
/// shorter queries ("opel corsa 1.3 cdti" → "opel corsa") since entity
/// search needs coarse terms.
async fn wiki_data(query: &str) -> Option<(Page, String)> {
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut tries = vec![query.to_string()];
    if words.len() > 3 {
        tries.push(words[..3].join(" "));
    }
    if words.len() > 2 {
        tries.push(words[..2].join(" "));
    }
    for q in tries {
        if let Some(p) = wiki_entity(&q).await {
            return Some(p);
        }
    }
    None
}

async fn wiki_entity(query: &str) -> Option<(Page, String)> {
    let c = client();
    let search = format!(
        "https://www.wikidata.org/w/api.php?action=wbsearchentities&search={}&language=en&format=json&limit=3",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    let s = c.get(&search).send().await.ok()?.text().await.ok()?;
    let id = Regex::new(r#""id":"(Q\d+)""#)
        .unwrap()
        .captures_iter(&s)
        .map(|c| c[1].to_string())
        .next()?;
    let ent = c
        .get(&format!("https://www.wikidata.org/w/api.php?action=wbgetentities&ids={id}&props=labels%7Cdescriptions%7Cclaims&languages=en&format=json"))
        .send()
        .await
        .ok()?
        .text()
        .await
        .ok()?;
    let v: serde_json::Value = serde_json::from_str(&ent).ok()?;
    let e = v.get("entities")?.get(&id)?;
    let label = e.get("labels")?.get("en")?.get("value").and_then(|x| x.as_str()).unwrap_or(&id).to_string();
    let desc = e.get("descriptions")?.get("en")?.get("value").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let claims = e.get("claims")?.as_object()?;
    // resolve value entity labels in one call
    let mut need: Vec<String> = vec![];
    for stmts in claims.values().take(20) {
        for st in stmts.as_array().cloned().unwrap_or_default().iter().take(3) {
            if let Some(q) = st.get("mainsnak").and_then(|m| m.get("datavalue")).and_then(|d| d.get("value")).and_then(|x| x.get("id")).and_then(|x| x.as_str()) {
                if !need.contains(&q.to_string()) {
                    need.push(q.to_string());
                }
            }
        }
        if need.len() >= 40 {
            break;
        }
    }
    let mut labels: std::collections::HashMap<String, String> = Default::default();
    if !need.is_empty() {
        let ids = need.join("|");
        // one retry: this call flaked silently before, starving both the
        // text and the instance line of labels
        for _ in 0..2 {
            if let Ok(t) = c
                .get(&format!("https://www.wikidata.org/w/api.php?action=wbgetentities&ids={ids}&props=labels&languages=en&format=json"))
                .send()
                .await
            {
                if let Ok(t) = t.text().await {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        if let Some(ents) = v.get("entities").and_then(|e| e.as_object()) {
                            for (k, ent) in ents {
                                if let Some(l) = ent.get("labels").and_then(|l| l.get("en")).and_then(|l| l.get("value")).and_then(|x| x.as_str()) {
                                    labels.insert(k.clone(), l.to_string());
                                }
                            }
                        }
                    }
                }
            }
            if !labels.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
    // curated property map; opaque identifier props are dropped silently
    // (a human manual never shows Freebase IDs or catalog numbers)
    let prop_name = |p: &str| -> Option<String> {
        Some(
            match p {
                "P31" => "instance of",
                "P176" => "manufacturer",
                "P571" => "inception",
                "P580" => "start time",
                "P582" => "end time",
                "P856" => "official website",
                "P1082" => "population",
                "P2046" => "area",
                "P37" => "official language",
                "P36" => "capital",
                "P38" => "currency",
                "P30" => "continent",
                "P35" => "head of state",
                "P6" => "head of government",
                "P297" => "ISO code",
                "P463" => "member of",
                "P17" => "country",
                "P495" => "country of origin",
                "P112" => "founded by",
                _ => return None,
            }
            .to_string(),
        )
    };
    // static QID floor: domain-critical entities resolve even when the
    // labels call flakes (offline-proof; never print raw QIDs to humans)
    let qid_floor = |q: &str| -> Option<String> {
        match q {
            "Q6256" => Some("country".into()),
            "Q3624078" => Some("sovereign state".into()),
            "Q515" => Some("city".into()),
            _ => None,
        }
    };
    let mut lines = vec![format!("{label} ({desc}) [Wikidata {id}]")];
    // curated properties first (instance-of feeds domain detection),
    // then anything else mapped; opaque IDs never print
    let mut ordered: Vec<(&String, &serde_json::Value)> = vec![];
    for (prop, stmts) in claims.iter() {
        if prop_name(prop).is_some() {
            // P31 (instance of) always leads — downstream domain detection
            if prop == "P31" {
                ordered.insert(0, (prop, stmts));
            } else {
                ordered.push((prop, stmts));
            }
        }
    }
    for (prop, stmts) in ordered.iter().take(18) {
        let mut vals = vec![];
        for st in stmts.as_array().cloned().unwrap_or_default().iter().take(3) {
            let dv = st.get("mainsnak").and_then(|m| m.get("datavalue"));
            let s = match dv.and_then(|d| d.get("type").and_then(|t| t.as_str())) {
                Some("wikibase-entityid") => dv
                    .and_then(|d| d.get("value")).and_then(|x| x.get("id")).and_then(|x| x.as_str())
                    .and_then(|q| labels.get(q).cloned().or_else(|| qid_floor(q)))
                    .unwrap_or_default(),
                Some("time") => dv.and_then(|d| d.get("value")).and_then(|x| x.get("time")).and_then(|x| x.as_str()).unwrap_or("").trim_start_matches('+').split('T').next().unwrap_or("").to_string(),
                Some("quantity") => dv.and_then(|d| d.get("value")).and_then(|x| x.get("amount")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
                Some("string") | Some("url") => dv.and_then(|d| d.get("value")).and_then(|x| x.as_str()).unwrap_or("").to_string(),
                _ => continue,
            };
            if !s.is_empty() {
                vals.push(s);
            }
        }
        if !vals.is_empty() {
            if let Some(name) = prop_name(prop) {
                lines.push(format!("{name}: {}", vals.join(", ")));
            }
        }
    }
    let text = lines.join("\n");
    if text.chars().count() < 80 {
        return None;
    }
    let chars = text.chars().count();
    // instance-of text feeds subject-domain detection downstream
    let instance = claims
        .get("P31")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|st| {
                    st.get("mainsnak")?.get("datavalue")?.get("value")?.get("id")?.as_str()
                        .and_then(|q| labels.get(q).cloned().or_else(|| qid_floor(q)))
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    Some((
        Page { url: format!("https://www.wikidata.org/wiki/{id}"), title: format!("{label} — Wikidata"), text: text.clone(), chars, dup_of: None },
        instance,
    ))
}

/// Sitemap enumeration: /sitemap.xml (+ index children), keep locs
/// matching query tokens. Zero search-engine dependence.
async fn sitemap_urls(host: &str, tokens: &[String], limit: usize) -> Vec<String> {
    let c = client();
    let root = format!("https://{host}/sitemap.xml");
    let xml = match c.get(&root).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let loc_re = Regex::new(r"<loc>([^<]+)</loc>").unwrap();
    let mut locs: Vec<String> = loc_re.captures_iter(&xml).map(|c| c[1].to_string()).collect();
    // sitemapindex: recurse into children (cap 4)
    if xml.contains("sitemapindex") {
        let mut deep = vec![];
        for child in locs.iter().take(4) {
            if let Ok(r) = c.get(child).send().await {
                if let Ok(t) = r.text().await {
                    deep.extend(loc_re.captures_iter(&t).map(|c| c[1].to_string()));
                }
            }
        }
        locs = deep;
    }
    locs.into_iter()
        .filter(|u| {
            let lu = u.to_lowercase();
            (lu.ends_with(".html") || lu.ends_with(".htm") || lu.ends_with('/') || !lu.split('/').last().unwrap_or("").contains('.'))
                && tokens.iter().any(|t| lu.contains(t.as_str()))
        })
        .take(limit)
        .collect()
}

/// Common Crawl: index API → WARC byte-range fetch → gunzip → extract.
/// Bulk web without search engines. `prefix` like "ultimatespecs.com/car-specs/Opel/".
async fn common_crawl(query: &str, prefix: &str, limit: usize) -> Vec<Page> {
    let c = client();
    let idx = format!(
        "https://index.commoncrawl.org/CC-MAIN-2025-30-index?url={}%2F*&output=json&filter=status%3A200&limit=40",
        prefix.trim_end_matches('/')
    );
    let list = match c.get(&idx).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let mut out = vec![];
    for line in list.lines() {
        if out.len() >= limit {
            break;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let url = v.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
        if !model_gate(query, &url) {
            continue;
        }
        let (filename, offset, length) = match (
            v.get("filename").and_then(|x| x.as_str()),
            v.get("offset").and_then(|x| x.as_str()),
            v.get("length").and_then(|x| x.as_str()),
        ) {
            (Some(f), Some(o), Some(l)) => (f.to_string(), o.to_string(), l.to_string()),
            _ => continue,
        };
        let (offset, length): (usize, usize) = match (offset.parse(), length.parse()) {
            (Ok(o), Ok(l)) => (o, l),
            _ => continue,
        };
        if length == 0 || length > 4_000_000 {
            continue;
        }
        let warc_url = format!("https://data.commoncrawl.org/{filename}");
        let bytes = match c
            .get(&warc_url)
            .header("Range", format!("bytes={}-{}", offset, offset + length - 1))
            .send()
            .await
        {
            Ok(r) => r.bytes().await.unwrap_or_default(),
            Err(_) => continue,
        };
        // CC records are standalone gzip members: decompress, skip WARC + HTTP headers
        let mut dec = flate2::read::GzDecoder::new(&bytes[..]);
        let mut raw = Vec::new();
        use std::io::Read;
        if dec.read_to_end(&mut raw).is_err() {
            continue;
        }
        let text = String::from_utf8_lossy(&raw);
        let parts: Vec<&str> = text.split("\r\n\r\n").collect();
        let html = parts.last().unwrap_or(&"");
        if html.len() < 1000 {
            continue;
        }
        let (mut title, body) = extract_text(html);
        if body.chars().count() < 200 {
            continue;
        }
        if title.is_empty() {
            title = url.clone();
        }
        let chars = body.chars().count();
        out.push(Page { url, title, text: body, chars, dup_of: None });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    out
}

/// Relevance gate: a URL must carry the query's model token (longest
/// alpha token, e.g. `corsa`, `transit`) plus at least one more query
/// token. Kills single-token drift (a Mondeo page matching only
/// year+fuel, a Liebao page matching only the year).
fn model_tokens(query: &str) -> Vec<String> {
    let mut alphas: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() >= 4 && t.chars().any(|c| c.is_ascii_alphabetic()))
        .map(|t| t.to_string())
        .collect();
    alphas.sort_by_key(|t| std::cmp::Reverse(t.len()));
    alphas.dedup();
    alphas
}

fn model_gate(query: &str, url: &str) -> bool {
    let tokens: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 2)
        .map(|t| t.to_string())
        .collect();
    let lu = url.to_lowercase();
    let hits = tokens.iter().filter(|t| lu.contains(t.as_str())).count();
    if hits < 2 {
        return false;
    }
    let core = model_tokens(query);
    if core.is_empty() {
        return true;
    }
    // longest token wins; ties all count as core
    let maxlen = core.iter().map(|t| t.len()).max().unwrap_or(0);
    core.iter().filter(|t| t.len() == maxlen).any(|t| lu.contains(t.as_str()))
}

/// Wikipedia family: en/de/pl/it opensearch (proven car-spec value:
///
/// German datasheets, Polish Multijet pages). Always reachable.
async fn wiki_family(query: &str) -> Vec<String> {
    let mut out = vec![];
    for lang in ["en", "de", "pl", "it"] {
        let url = format!(
            "https://{lang}.wikipedia.org/w/api.php?action=opensearch&search={}&limit=4&format=json",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        );
        if let Ok(p) = fetch_one(&url).await {
            for cap in Regex::new(r#"https?://[a-z]+\.wikipedia\.org/wiki/[^\s"\\]+"#)
                .unwrap()
                .captures_iter(&p.text)
            {
                out.push(cap[0].to_string());
                if out.len() >= 8 {
                    return out;
                }
            }
        }
    }
    out
}

/// Vertical top-up: `site:`-scoped queries against hosts where spec
/// tables actually live, through the DDG leg.
async fn site_scoped(query: &str, prog: &Option<tokio::sync::mpsc::UnboundedSender<String>>) -> Vec<String> {
    const VERTICALS: &[&str] = &[
        "ultimatespecs.com",
        "cars-data.com",
        "parkers.co.uk",
        "rac.co.uk",
        "honestjohn.co.uk",
        "adac.de",
    ];
    let mut out = vec![];
    for host in VERTICALS {
        let q = format!("{query} site:{host}");
        let mut hits = ddg_search_tries(&q, 2, 2, 10).await;
        // keep only hits actually on the vertical (DDG may ignore site:)
        hits.retain(|u| u.to_lowercase().contains(host));
        if !hits.is_empty() {
            if let Some(tx) = prog {
                let _ = tx.send(format!("search: {host} got {}", hits.len()));
            }
            out.extend(hits);
        }
        if out.len() >= 8 {
            break;
        }
    }
    out
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CrawlPlan {
    pub urls: Vec<String>,
    /// content already fetched from bulk legs (WARC, structured APIs)
    #[serde(default)]
    pub prefetched: Vec<Page>,
    /// per-leg yield for the log: (leg, urls-or-pages contributed)
    #[serde(default)]
    pub legs: Vec<(String, usize)>,
    /// what the query means (drives gates + relevance downstream)
    pub query_card: QueryCard,
}

/// Build thorough URL list: DDG + Marginalia + Wikipedia titles.
pub async fn plan_urls(
    query: &str,
    depth: &str,
    max_pages: usize,
    prog: Option<tokio::sync::mpsc::UnboundedSender<String>>,
) -> CrawlPlan {
    let send = |m: &str| {
        if let Some(tx) = &prog {
            let _ = tx.send(m.to_string());
        }
    };
    let (per_engine, wiki_n) = match depth {
        "quick" => (4, 2),
        "extreme" => (16, 5),
        _ => (10, 3),
    };
    let mut urls: Vec<String> = vec![];
    let mut prefetched: Vec<Page> = vec![];
    let mut seen = HashSet::new();
    let mut legs: Vec<(String, usize)> = vec![];

    // query card FIRST: meaning + aliases drive every gate below.
    // Understanding precedes searching.
    send("understand: query card…");
    let card = query_card(query).await;
    if card.meaning.is_empty() && card.aliases.is_empty() {
        send("understand: no card (offline?) — raw spelling only");
    } else {
        send(&format!(
            "understand: {} — {} [aliases: {}]",
            card.subject,
            card.meaning.chars().take(100).collect::<String>(),
            card.aliases.join(", ").chars().take(80).collect::<String>()
        ));
    }
    // expanded lexicon: query terms + aliases feed the gates
    // (cross-badge misses die here, not in the manual)
    let gate_q = format!("{} {}", query, card.aliases.join(" "));

    send("search: duckduckgo…");
    let ddg = match tokio::time::timeout(std::time::Duration::from_secs(20), ddg_search(query, per_engine)).await {
        Ok(v) => v,
        Err(_) => {
            send("search: duckduckgo timed out (20s), continuing…");
            vec![]
        }
    };
    send(&format!("search: duckduckgo got {}", ddg.len()));
    send("search: marginalia…");
    let mar = match tokio::time::timeout(std::time::Duration::from_secs(20), marginalia_search(query, per_engine)).await {
        Ok(v) => v,
        Err(_) => {
            send("search: marginalia timed out (20s), continuing…");
            vec![]
        }
    };
    send(&format!("search: marginalia got {}", mar.len()));

    let mut raw: Vec<String> = ddg.into_iter().chain(mar).collect();
    // relevance gate on the EXPANDED lexicon (wikipedia/verticals exempt)
    let gated: Vec<String> = raw.drain(..).filter(|u| !spam_url(u) && model_gate(&gate_q, u)).collect();
    legs.push(("engines".into(), gated.len()));
    let chosen: Vec<String> = if gated.len() >= 3 {
        gated
    } else {
        send("search: model gate relaxed (<3 gated hits)");
        let mut backfill: Vec<String> = gated;
        for u in raw {
            if backfill.len() >= 3 || seen.contains(&canon_url(&u)) {
                continue;
            }
            if spam_url(&u) {
                continue;
            }
            backfill.push(u);
        }
        backfill
    };
    for u in chosen {
        if !seen.insert(canon_url(&u)) {
            continue;
        }
        urls.push(u);
        if urls.len() >= max_pages {
            break;
        }
    }
    if urls.len() < max_pages {
        send("search: wikipedia (en/de/pl/it)…");
        // flood guard: encyclopedia share capped so one exempt leg can't
        // become the whole corpus on single-token queries (see mixology)
        let wiki_cap = (max_pages * 4 / 10).max(2);
        let mut n = 0;
        for u in wiki_family(query).await {
            if spam_url(&u) {
                continue;
            }
            if !seen.insert(canon_url(&u)) {
                continue;
            }
            urls.push(u);
            n += 1;
            if urls.len() >= max_pages || n >= wiki_cap {
                break;
            }
        }
        send(&format!("search: wikipedia got {n}"));
        legs.push(("wikipedia".into(), n));
        // legacy en API as backup when opensearch yields little
        if n < 2 {
            match tokio::time::timeout(std::time::Duration::from_secs(15), fetch_one(&wikipedia_api(query))).await {
                Ok(Ok(p)) => {
                    for cap in Regex::new(r#""title":"([^"]+)""#)
                        .unwrap()
                        .captures_iter(&p.text)
                        .take(wiki_n)
                    {
                        let t = cap[1].replace(' ', "_");
                        let u = format!("https://en.wikipedia.org/wiki/{t}");
                        if !seen.insert(canon_url(&u)) {
                            continue;
                        }
                        urls.push(u);
                        n += 1;
                        if urls.len() >= max_pages {
                            break;
                        }
                    }
                    send(&format!("search: wikipedia got {n}"));
                }
                _ => send("search: wikipedia backup failed, continuing…"),
            }
        }
    }
    // structured facts FIRST: cheap, reliable, and the instance line
    // drives subject-domain detection for everything below (chapters,
    // chapter books, variants)
    send("search: wikidata…");
    let wmark = prefetched.len();
    let mut instance_of = String::new();
    if let Some((p, inst)) = wiki_data(query).await {
        if !spam_url(&p.url) && seen.insert(canon_url(&p.url)) {
            instance_of = inst;
            prefetched.push(p);
            send("search: wikidata got 1");
        }
    }
    let domain = subject_domain(query, &instance_of);
    send(&format!("outline: subject is {domain}"));
    legs.push(("wikidata".into(), prefetched.len() - wmark));
    if urls.len() < max_pages {
        send("search: verticals (site:)…");
        let mark = urls.len();
        for u in site_scoped(query, &prog).await {
            if spam_url(&u) {
                continue;
            }
            // verticals are pre-scoped by site: but engines may ignore the
            // operator — still gate on the expanded lexicon
            if !model_gate(&gate_q, &u) {
                continue;
            }
            if !seen.insert(canon_url(&u)) {
                continue;
            }
            urls.push(u);
            if urls.len() >= max_pages {
                break;
            }
        }
        legs.push(("site-verticals".into(), urls.len() - mark));
    }
    // book + structured-drink legs: no search engine, no key.
    // IA/Wikisource/OpenLibrary are field-agnostic (sciences, art,
    // history); CocktailDB fires only when it has the drink.
    // Books run per chapter too (a cookbook for Cuisine, not more history).
    {
        let mark = prefetched.len();
        send("search: books (archive.org)…");
        for p in ia_books(query, &prog, 3).await {
            if spam_url(&p.url) || !seen.insert(canon_url(&p.url)) {
                continue;
            }
            prefetched.push(p);
        }
        for ch in chapters(&domain) {
            let first = ch.split_whitespace().next().unwrap_or("");
            if first.is_empty() {
                continue;
            }
            let bq = format!("{query} {first}");
            send(&format!("search: books ({bq})…"));
            for p in ia_books(&bq, &prog, 1).await {
                if spam_url(&p.url) || !seen.insert(canon_url(&p.url)) {
                    continue;
                }
                prefetched.push(p);
                break;
            }
        }
        send("search: wikisource…");
        for p in wikisource_search(query).await {
            if spam_url(&p.url) || !seen.insert(canon_url(&p.url)) {
                continue;
            }
            prefetched.push(p);
        }
        for p in cocktaildb(query).await {
            if spam_url(&p.url) || !seen.insert(canon_url(&p.url)) {
                continue;
            }
            prefetched.push(p);
        }
        // Open Library is bibliography, not knowledge: only worth a slot
        // when real books were actually found above
        let found_books = prefetched
            .iter()
            .filter(|p| p.url.contains("archive.org") || p.url.contains("wikisource.org"))
            .count();
        if found_books > 0 {
            send("search: openlibrary…");
            for p in openlibrary(query).await {
                if spam_url(&p.url) || !seen.insert(canon_url(&p.url)) {
                    continue;
                }
                prefetched.push(p);
                break;
            }
        }
        legs.push(("books".into(), prefetched.len() - mark));
    }
    // entity traversal: top wikidata entities seed their articles
    // ("mixology" -> Cocktail, Bartender, List of cocktails…)
    if urls.len() < max_pages + 4 {
        send("search: wikidata related…");
        let mark = urls.len();
        for u in wiki_related(query).await {
            if spam_url(&u) || !seen.insert(canon_url(&u)) {
                continue;
            }
            urls.push(u);
            if urls.len() >= max_pages + 4 {
                break;
            }
        }
        legs.push(("related".into(), urls.len() - mark));
    }
    // bulk legs: sitemaps + Common Crawl on hosts already seen (no search engine)
    if urls.len() < max_pages {
        let mark = urls.len();
        let pmark = prefetched.len();
        use std::collections::HashSet as _Hs2;
        let mut hosts: Vec<String> = vec![];
        let mut _hs = _Hs2::new();
        for u in urls.iter() {
            if let Ok(p) = url::Url::parse(u) {
                let h = p.host_str().unwrap_or("").to_string();
                if !h.is_empty() && !h.contains("wikipedia.org") && _hs.insert(h.clone()) {
                    hosts.push(h);
                }
            }
            if hosts.len() >= 4 {
                break;
            }
        }
        let tokens: Vec<String> = query
            .to_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|t| t.len() > 2)
            .map(|t| t.to_string())
            .collect();
        for h in hosts.iter().take(3) {
            if urls.len() >= max_pages {
                break;
            }
            send(&format!("search: sitemap {h}…"));
            for u in sitemap_urls(h, &tokens, 3).await {
                if spam_url(&u) || !seen.insert(canon_url(&u)) {
                    continue;
                }
                urls.push(u);
                if urls.len() >= max_pages {
                    break;
                }
            }
        }
        send("search: commoncrawl…");
        for h in hosts.iter().take(2) {
            let prefix = h.trim_start_matches("www.").to_string() + "/";
            for p in common_crawl(query, &prefix, 2).await {
                if spam_url(&p.url) || !seen.insert(canon_url(&p.url)) {
                    continue;
                }
                prefetched.push(p);
            }
        }
        legs.push(("bulk".into(), (urls.len() - mark) + (prefetched.len() - pmark)));
    }
    // (wikidata + domain already resolved above, before books)
    if depth != "quick" && urls.len() < max_pages + 4 {
        let mut variants = chapter_queries(query, &domain);
        for a in alias_queries(query) {
            variants.push(a);
        }
        // per-chapter ring merge: each chapter gets its fair share first,
        // so one loud chapter can't eat the whole budget (Portugal's
        // History drowning Cuisine). Collect per variant, then interleave.
        let mut buckets: Vec<Vec<String>> = vec![];
        for variant in variants {
            send(&format!("search: variant {variant}…"));
            let extra = match tokio::time::timeout(std::time::Duration::from_secs(60), ddg_search_tries(&variant, 3, 2, 10)).await {
                Ok(v) => v,
                Err(_) => {
                    send("search: variant timed out, continuing…");
                    vec![]
                }
            };
            let mut bucket = vec![];
            // per-chapter budget grows with depth: quick 3, deep 6, extreme 10
            let bucket_cap = match depth {
                "quick" => 3,
                "extreme" => 10,
                _ => 6,
            };
            for u in extra {
                if spam_url(&u) {
                    continue;
                }
                if !model_gate(&variant, &u) && !model_gate(&gate_q, &u) {
                    continue;
                }
                if !seen.insert(canon_url(&u)) {
                    continue;
                }
                bucket.push(u);
                if bucket.len() >= bucket_cap {
                    break;
                }
            }
            buckets.push(bucket);
        }
        // round-robin: ch1u1, ch2u1, … ch1u2, ch2u2, …
        let mut round = 0;
        loop {
            let mut added = false;
            for b in buckets.iter() {
                if round < b.len() && urls.len() < max_pages + 4 {
                    urls.push(b[round].clone());
                    added = true;
                }
            }
            if !added {
                break;
            }
            round += 1;
        }
        legs.push(("variants".into(), buckets.iter().map(|b| b.len()).sum()));
    }
    // global encyclopedia cap: exempt legs (family, sitelinks, harvest)
    // can otherwise flood single-token queries with surnames and bands
    let wiki_cap = (max_pages * 4 / 10).max(2);
    let mut wiki_kept = 0;
    urls.retain(|u| {
        if u.contains("wikipedia.org") {
            wiki_kept += 1;
            return wiki_kept <= wiki_cap;
        }
        true
    });
    urls.truncate(max_pages);
    CrawlPlan { urls, prefetched, legs, query_card: card }
}

/// robots.txt, cached per host: best-effort Disallow check.
/// House citizenship: magpie is privacy-first AND polite.
async fn robots_allows(url: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<String>>>> = OnceLock::new();
    let host = match url::Url::parse(url).ok().and_then(|p| p.host_str().map(|h| h.to_string())) {
        Some(h) => h,
        None => return true,
    };
    let path = url::Url::parse(url).map(|p| p.path().to_string()).unwrap_or_else(|_| "/".into());
    {
        let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
        if let Ok(c) = cache.lock() {
            if let Some(rules) = c.get(&host) {
                return !rules.iter().any(|d: &String| path.starts_with(d));
            }
        }
    }
    // fetch once per host (short timeout; failure = allow)
    let body = {
        let c = client();
        match tokio::time::timeout(std::time::Duration::from_secs(8), c.get(&format!("https://{host}/robots.txt")).send()).await {
            Ok(Ok(r)) => r.text().await.unwrap_or_default(),
            _ => String::new(),
        }
    };
    let mut in_wildcard = false;
    let mut rules: Vec<String> = vec![];
    for line in body.lines() {
        let l = line.trim();
        if l.to_lowercase().starts_with("user-agent:") {
            let ua = l[11..].trim();
            in_wildcard = ua == "*" || ua.to_lowercase().contains("magpie");
        } else if in_wildcard && l.to_lowercase().starts_with("disallow:") {
            let d = l[9..].trim().to_string();
            if !d.is_empty() {
                rules.push(d);
            }
        }
    }
    let ok = !rules.iter().any(|d| path.starts_with(d));
    if let Ok(mut c) = CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock() {
        c.insert(host, rules);
    }
    ok
}

pub async fn crawl_urls(urls: &[String], prog: Option<tokio::sync::mpsc::UnboundedSender<String>>) -> Vec<Page> {
    use std::sync::Arc;
    use tokio::sync::Semaphore;
    // 3 concurrent fetches, 250ms launch pacing: fast but polite.
    // (Was serial 400ms; time no longer matters more than coverage,
    // but citizenship still does.)
    let total = urls.len();
    let sem = Arc::new(Semaphore::new(3));
    let mut handles = vec![];
    for (i, u) in urls.iter().cloned().enumerate() {
        let prog = prog.clone();
        let sem = sem.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire_owned().await;
            if let Some(tx) = &prog {
                let host = url::Url::parse(&u)
                    .map(|p| p.host_str().unwrap_or("").to_string())
                    .unwrap_or_else(|_| u.chars().take(40).collect());
                let _ = tx.send(format!("fetch {}/{} {}", i + 1, total, host));
            }
            if !robots_allows(&u).await {
                if let Some(tx) = &prog {
                    let _ = tx.send(format!("skip {}/{} (robots)", i + 1, total));
                }
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            match fetch_one(&u).await {
                Ok(p) => {
                    if p.text.chars().count() < 200 {
                        if let Some(tx) = &prog {
                            let _ = tx.send(format!("skip {}/{} (thin)", i + 1, total));
                        }
                        return None;
                    }
                    if let Some(tx) = &prog {
                        let _ = tx.send(format!("got {}/{} {}c", i + 1, total, p.chars));
                    }
                    Some((i, p))
                }
                Err(_) => {
                    if let Some(tx) = &prog {
                        let _ = tx.send(format!("fail {}/{}", i + 1, total));
                    }
                    None
                }
            }
        }));
    }
    let mut out: Vec<(usize, Page)> = vec![];
    for h in handles {
        if let Ok(Some(p)) = h.await {
            out.push(p);
        }
    }
    // restore plan order (progress may arrive out of order; order matters
    // for dup_of indices and ring-budget expectations)
    out.sort_by_key(|(i, _)| *i);
    out.into_iter().map(|(_, p)| p).collect()
}

fn host_of(u: &str) -> String {
    url::Url::parse(u).map(|p| p.host_str().unwrap_or("").to_string()).unwrap_or_default()
}

fn spam_host(h: &str) -> bool {
    let h = h.to_lowercase();
    h.contains("adtargeting.io")
        || h.contains("gumtree.com")
        || h.contains("alchetron.com")
        || h.contains("creativecommons.org")
        || h.contains("facebook.com")
        || h.contains("instagram.com")
        // dealer lots / lead-gen: specs never live here
        || h.contains("cinch.co.uk")
        || h.contains("unbeatablecar.com")
        || h.contains("deniskinanehonda")
        || h.contains("choosemycar.com")
        || h.contains("autotrader")
        || h.contains("ebay.")
        || h.contains("pistonheads.com")
        // classifieds / marketplaces in other locales
        || h.contains("otomoto.pl")
        || h.contains("mobile.de")
        || h.contains("autoscout24")
        || h.contains("olx.")
}

fn spam_url(u: &str) -> bool {
    let lu = u.to_lowercase();
    if spam_host(&host_of(u)) {
        return true;
    }
    // share / intent / social widgets carry no manual content
    lu.contains("/intent/")
        || lu.contains("intent/tweet")
        || lu.contains("/sharer")
        || lu.contains("whatsapp")
        || lu.contains("facebook.com/sharer")
        || lu.contains("twitter.com/intent")
        || lu.contains("mailto:")
        // for-sale / classified paths, even on legit hosts (parkers, rac)
        || lu.contains("/for-sale/")
        || lu.contains("/srpsearch")
        || lu.contains("/cars-vans-motorbikes/")
        || lu.contains("/used-cars/")
        || lu.contains("/vans-for-sale")
        || lu.contains("usedvfs-")
}

/// Canonical dedupe key: mirror networks collapse even across TLDs
/// (www.auto-abc.eu == es.auto-abc.eu == www.auto-abc.lv when paths match;
/// auto-data.net/en/X == auto-data.net/bg/X).
fn canon_url(u: &str) -> String {
    let parsed = match url::Url::parse(u) {
        Ok(p) => p,
        Err(_) => return u.to_lowercase(),
    };
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    // site name: second-level label (auto-abc.eu and auto-abc.lv -> auto-abc)
    let parts: Vec<&str> = host.split('.').filter(|s| !s.is_empty()).collect();
    let site = if parts.len() >= 2 {
        parts[parts.len() - 2].to_string()
    } else {
        host.clone()
    };
    // strip leading 2-letter language path segment (/en/, /bg/, /de/…)
    let mut path = parsed.path().to_lowercase();
    if path.len() > 4 {
        let b = path.as_bytes();
        if b[0] == b'/' && b[1].is_ascii_alphanumeric() && b[2].is_ascii_alphanumeric() && b[3] == b'/' {
            path = path[3..].to_string();
        }
    }
    // keep #chunk-N fragments: chunks of one book are distinct entries
    let frag = parsed.fragment().map(|f| format!("#{f}")).unwrap_or_default();
    // cross-TLD collapse only for deep content paths (avoids merging
    // unrelated same-name domains on short index paths)
    if path.len() > 8 {
        return format!("{site}{path}{frag}");
    }
    format!("{host}{path}{frag}")
}

/// Boost: hosts where specs / reliability / dimensions actually live.
fn boost_host(h: &str) -> i32 {
    let h = h.to_lowercase();
    let mut b = 0;
    for good in [
        "cars-data.com",
        "ultimatespecs.com",
        "cardimension",
        "engineexplained",
        "fordengines",
        "opel-infos",
        "parkers.co.uk",
        "rac.co.uk",
        "autogidas",
        "curbsideclassic",
        "honestjohn",
        "adac.de",
    ] {
        if h.contains(good) {
            b += 4;
        }
    }
    b
}

/// Second-hop thoroughness: harvest outbound links from fetched pages
/// so we still get many various sources when DDG/Marginalia rate-limit.
/// Only links matching a query token survive (kills off-topic drift);
/// boosted hosts rank first.
pub async fn expand_second_hop(query: &str, pages: &[Page], max_extra: usize) -> Vec<String> {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = pages.iter().map(|p| canon_url(&p.url)).collect();
    let mut scored: Vec<(i32, String)> = vec![];
    let re = Regex::new(r#"(?i)href\s*=\s*"(https?://[^"]+)""#).unwrap();
    for p in pages {
        let html = fetch_raw(&p.url).await.unwrap_or_default();
        for cap in re.captures_iter(&html) {
            let u = cap[1].to_string();
            if !u.starts_with("http") || u.contains("wikimedia.org") {
                continue;
            }
            if u.ends_with(".jpg") || u.ends_with(".png") || u.ends_with(".svg") || u.ends_with(".css") || u.ends_with(".js") {
                continue;
            }
            if spam_url(&u) {
                continue;
            }
            // skip low-value interlanguage wikipedia unless DE/PL/IT/EN
            if u.contains("wikipedia.org") {
                if !(u.contains("en.wikipedia") || u.contains("de.wikipedia") || u.contains("pl.wikipedia") || u.contains("it.wikipedia")) {
                    continue;
                }
            }
            if !seen.insert(canon_url(&u)) {
                continue;
            }
            let lu = u.to_lowercase();
            if !model_gate(query, &u) {
                let _ = lu;
                continue;
            }
            let mut score = boost_host(&host_of(&u));
            for kw in ["opel", "corsa", "cdti", "multijet", "jtd", "diesel", "fiat", "vauxhall", "auto", "motor", "car", "transit", "focus", "spec", "reliability", "dimension", "engine"] {
                if lu.contains(kw) {
                    score += 2;
                }
            }
            // prefer informational over pure shop spam, but keep shops if topical
            if lu.contains("battery") || lu.contains("fitting") || lu.contains("product/") {
                score -= 1;
            }
            scored.push((score, u));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored.into_iter().take(max_extra).map(|(_, u)| u).collect()
}

async fn fetch_raw(url: &str) -> std::result::Result<String, ()> {
    // re-fetch raw html for link harvest (best-effort)
    let c = client();
    let r = c.get(url).send().await.map_err(|_| ())?;
    if !r.status().is_success() {
        return Err(());
    }
    r.text().await.map_err(|_| ())
}
