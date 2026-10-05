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

fn wikipedia_api(query: &str) -> String {
    format!(
        "https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&format=json&srlimit=5",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    )
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CrawlPlan {
    pub urls: Vec<String>,
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

    for u in ddg.into_iter().chain(mar) {
        if spam_url(&u) {
            continue;
        }
        if seen.insert(u.clone()) {
            urls.push(u);
        }
        if urls.len() >= max_pages {
            break;
        }
    }
    if urls.len() < max_pages {
        send("search: wikipedia…");
        match tokio::time::timeout(std::time::Duration::from_secs(15), fetch_one(&wikipedia_api(query))).await {
            Ok(Ok(p)) => {
                let mut n = 0;
                for cap in Regex::new(r#""title":"([^"]+)""#)
                    .unwrap()
                    .captures_iter(&p.text)
                    .take(wiki_n)
                {
                    let t = cap[1].replace(' ', "_");
                    let u = format!("https://en.wikipedia.org/wiki/{t}");
                    if seen.insert(u.clone()) {
                        urls.push(u);
                        n += 1;
                    }
                    if urls.len() >= max_pages {
                        break;
                    }
                }
                send(&format!("search: wikipedia got {n}"));
            }
            _ => send("search: wikipedia timed out/failed, continuing…"),
        }
    }
    if depth != "quick" && urls.len() < max_pages {
        for variant in [format!("{query} specifications"), format!("{query} problems")] {
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
                if seen.insert(u.clone()) {
                    urls.push(u);
                }
                if urls.len() >= max_pages {
                    break;
                }
            }
        }
    }
    urls.truncate(max_pages);
    CrawlPlan { urls }
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
    // for-sale / classified paths, even on legit hosts (parkers, rac)
    lu.contains("/for-sale/")
        || lu.contains("/srpsearch")
        || lu.contains("/cars-vans-motorbikes/")
        || lu.contains("/used-cars/")
        || lu.contains("/vans-for-sale")
        || lu.contains("usedvfs-")
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
    let tokens: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 2)
        .map(|t| t.to_string())
        .collect();
    let mut seen: HashSet<String> = pages.iter().map(|p| p.url.clone()).collect();
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
            if !seen.insert(u.clone()) {
                continue;
            }
            let lu = u.to_lowercase();
            if !tokens.iter().any(|t| lu.contains(t.as_str())) {
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
