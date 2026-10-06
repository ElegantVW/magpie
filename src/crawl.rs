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
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(12))
        .build()
        .expect("client")
}

fn clean_ws(s: &str) -> String {
    let re = Regex::new(r"\s+").unwrap();
    re.replace_all(s.trim(), " ").to_string()
}

fn strip_tags(html: &str) -> String {
    // preserve structure: block elements -> newlines before stripping
    let re_script = Regex::new(r"(?is)<script.*?</script>").unwrap();
    let re_style = Regex::new(r"(?is)<style.*?</style>").unwrap();
    let re_block = Regex::new(r"(?is)</?(p|div|br|tr|li|h1|h2|h3|h4|table)[^>]*>").unwrap();
    let t = re_script.replace_all(html, " ");
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
        .filter(|l| !l.is_empty())
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
async fn wiki_data(query: &str) -> Option<Page> {
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

async fn wiki_entity(query: &str) -> Option<Page> {
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
    }
    let prop_name = |p: &str| -> String {
        match p {
            "P31" => "instance of",
            "P176" => "manufacturer",
            "P571" => "inception",
            "P580" => "start time",
            "P582" => "end time",
            "P1128" => "employees",
            "P2139" => "total revenue",
            "P856" => "official website",
            _ => return p.to_string(),
        }
        .to_string()
    };
    let mut lines = vec![format!("{label} ({desc}) [Wikidata {id}]")];
    for (prop, stmts) in claims.iter().take(18) {
        let mut vals = vec![];
        for st in stmts.as_array().cloned().unwrap_or_default().iter().take(3) {
            let dv = st.get("mainsnak").and_then(|m| m.get("datavalue"));
            let s = match dv.and_then(|d| d.get("type").and_then(|t| t.as_str())) {
                Some("wikibase-entityid") => dv
                    .and_then(|d| d.get("value")).and_then(|x| x.get("id")).and_then(|x| x.as_str())
                    .map(|q| labels.get(q).cloned().unwrap_or_else(|| q.to_string()))
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
            lines.push(format!("{}: {}", prop_name(prop), vals.join(", ")));
        }
    }
    let text = lines.join("\n");
    if text.chars().count() < 80 {
        return None;
    }
    let chars = text.chars().count();
    Some(Page { url: format!("https://www.wikidata.org/wiki/{id}"), title: format!("{label} — Wikidata"), text: text.clone(), chars, dup_of: None })
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
        let mut hits = ddg_search(&q, 2).await;
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
        "extreme" => (10, 5),
        _ => (6, 3),
    };
    let mut urls: Vec<String> = vec![];
    let mut prefetched: Vec<Page> = vec![];
    let mut seen = HashSet::new();

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
    // relevance gate for unscoped engine hits (wikipedia/verticals exempt)
    let gated: Vec<String> = raw.drain(..).filter(|u| !spam_url(u) && model_gate(query, u)).collect();
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
            if urls.len() >= max_pages || n >= 6 {
                break;
            }
        }
        send(&format!("search: wikipedia got {n}"));
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
    if urls.len() < max_pages {
        send("search: verticals (site:)…");
        for u in site_scoped(query, &prog).await {
            if spam_url(&u) {
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
    }
    // bulk legs: sitemaps + Common Crawl on hosts already seen (no search engine)
    if urls.len() < max_pages {
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
    }
    // structured facts need no crawl: synthetic page straight into prefetch.
    // Always fetched (cheap, reliable) — not gated on engine results.
    send("search: wikidata…");
    if let Some(p) = wiki_data(query).await {
        if !spam_url(&p.url) && seen.insert(canon_url(&p.url)) {
            prefetched.push(p);
            send("search: wikidata got 1");
        }
    }
    if depth != "quick" && urls.len() < max_pages + 4 {
        let mut variants = vec![format!("{query} specifications"), format!("{query} problems")];
        for a in alias_queries(query) {
            variants.push(a);
        }
        for variant in variants {
            if urls.len() >= max_pages + 4 {
                break;
            }
            send(&format!("search: variant {variant}…"));
            let extra = match tokio::time::timeout(std::time::Duration::from_secs(20), ddg_search(&variant, 3)).await {
                Ok(v) => v,
                Err(_) => {
                    send("search: variant timed out, continuing…");
                    vec![]
                }
            };
            for u in extra {
                if spam_url(&u) {
                    continue;
                }
                if !model_gate(&variant, &u) && !model_gate(query, &u) {
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
        }
    }
    urls.truncate(max_pages);
    CrawlPlan { urls, prefetched }
}

pub async fn crawl_urls(urls: &[String], prog: Option<tokio::sync::mpsc::UnboundedSender<String>>) -> Vec<Page> {
    let mut out = vec![];
    let total = urls.len();
    for (i, u) in urls.iter().enumerate() {
        if let Some(tx) = &prog {
            let host = url::Url::parse(u)
                .map(|p| p.host_str().unwrap_or("").to_string())
                .unwrap_or_else(|_| u.chars().take(40).collect());
            let _ = tx.send(format!("fetch {}/{} {}", i + 1, total, host));
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        match fetch_one(u).await {
            Ok(p) => {
                if p.text.chars().count() < 200 {
                    if let Some(tx) = &prog {
                        let _ = tx.send(format!("skip {}/{} (thin)", i + 1, total));
                    }
                    continue;
                }
                if let Some(tx) = &prog {
                    let _ = tx.send(format!("got {}/{} {}c", i + 1, total, p.chars));
                }
                out.push(p);
            }
            Err(_) => {
                if let Some(tx) = &prog {
                    let _ = tx.send(format!("fail {}/{}", i + 1, total));
                }
                continue;
            }
        }
    }
    out
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
        if b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2].is_ascii_alphabetic() && b[3] == b'/' {
            path = path[3..].to_string();
        }
    }
    // cross-TLD collapse only for deep content paths (avoids merging
    // unrelated same-name domains on short index paths)
    if path.len() > 8 {
        return format!("{site}{path}");
    }
    format!("{host}{path}")
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
