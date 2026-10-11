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

/// Stopwords that are capitalized only by position (sentence-initial)
/// or are pronunciation guides, never names. Shared by the lexicon and
/// the distinguishing marks.
const NAME_STOP: &[&str] = &[
    "from", "with", "and", "the", "for", "that", "this", "are", "was", "were", "have", "has",
    "which", "their", "been", "also", "into", "such", "than", "then", "them", "when", "where",
    "what", "there", "these", "those", "will", "would", "about", "after", "before", "between",
];

/// Proper nouns: capitalized words longer than 3 chars, minus stopwords
/// and short all-caps abbreviations ("IPA" is a guide, not a person).
fn proper_nouns(s: &str) -> Vec<String> {
    let mut out = vec![];
    for tok in s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| t.len() > 3) {
        if !tok.chars().next().map(|c| c.is_uppercase()).unwrap_or(false) {
            continue;
        }
        if tok.chars().all(|c| c.is_ascii_uppercase()) {
            continue;
        }
        if NAME_STOP.contains(&tok.to_lowercase().as_str()) {
            continue;
        }
        out.push(tok.to_lowercase());
    }
    out
}

/// Distinguishing marks: the tokens that tell same-words-different-subject
/// apart. Digit-bearing whitespace tokens with dots kept ("1.3" stays one
/// token, not "1"+"3") plus proper nouns from query, subject and meaning.
/// The scorer prompt names these explicitly: a page matching the words
/// with different marks (another year, another engine, another person)
/// is a different subject. ("opel corsa 2005 1.3 cdti" → 2005, 1.3;
/// hermeticism → hermes, trismegistus via the card.)
pub fn discriminators(query: &str, card: &QueryCard) -> Vec<String> {
    let mut set: std::collections::HashSet<String> = Default::default();
    for tok in query.split_whitespace() {
        let t = tok.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if t.len() > 1 && t.chars().any(|c| c.is_ascii_digit()) {
            set.insert(t.to_lowercase());
        }
    }
    for n in proper_nouns(query).into_iter().chain(proper_nouns(&card.subject)).chain(proper_nouns(&card.meaning)) {
        set.insert(n);
    }
    let mut out: Vec<String> = set.into_iter().collect();
    out.sort();
    out
}

/// Shared stemmer (suffix-strip): "hermeticism" → "hermet", matching
/// "hermetic"/"hermeticism"/"hermeticum" as substrings (short lex token
/// inside longer text words — the right direction). One copy, used by the
/// lexicon and the title gate alike.
fn stem_token(t: &str) -> String {
    let mut s = t.to_string();
    for suf in ["icism", "ism", "ists", "ist", "tion", "ing", "ed", "es", "s"] {
        if s.len() > suf.len() + 3 && s.ends_with(suf) {
            s.truncate(s.len() - suf.len());
            break;
        }
    }
    s
}

/// Subject lexicon: query + card subject + aliases, stemmed. Every book
/// and chunk gate works from THESE tokens — never from chapter-augmented
/// search strings ("hermeticism History" must not let a book through on
/// the word "history" alone). That was the Politica Aeterna hole: a Dugin
/// volume matched "history" in front matter and rode the History chapter
/// into 24 keeper slots.
pub fn subject_lexicon(query: &str, card: &QueryCard) -> Vec<String> {
    let mut set: std::collections::HashSet<String> = Default::default();
    let mut feed = |s: &str| {
        for tok in s.to_lowercase().split(|c: char| !c.is_ascii_alphanumeric()).filter(|t| t.len() > 2) {
            set.insert(tok.to_string());
            let st = stem_token(tok);
            if st.len() > 2 {
                set.insert(st);
            }
        }
    };
    feed(query);
    feed(&card.subject);
    for a in &card.aliases {
        feed(a);
    }
    // proper nouns from the meaning line (central figures): intake matching
    // must know Hermes Trismegistus belongs to Hermeticism (shared helper:
    // same hygiene as the distinguishing marks).
    for n in proper_nouns(&card.meaning) {
        set.insert(n);
    }
    let mut out: Vec<String> = set.into_iter().collect();
    out.sort();
    out
}

/// Book-level intake: the whole work must be ABOUT the subject. Title or
/// front matter must name it, and the full text must mention it densely.
/// One passing mention deep in an unrelated volume never qualifies.
/// Is this candidate a PERIODICAL/CONTAINER rather than a work about the
/// subject? A magazine issue ("New Scientist 28 March 2015") mentions the
/// subject plenty — often dozens of times across 400K chars — so the
/// on-subject gate below waves it through, and then EVERY chunk competes:
/// the bomb-tests-in-Antarctic-snow chunk scored 45 and entered a black-hole
/// manual. Containers are refused outright: a manual wants works ABOUT the
/// subject, and per-chunk scoring cannot tell an article from the issue
/// around it (lexical chunk gating kills canon; this is the intake-level
/// answer instead). Signal is the TITLE only — a monograph never titles
/// itself "Vol 4" or "March 2015".
fn is_periodical(title: &str) -> bool {
    let t = title.to_lowercase();
    // strip the site suffix magpie appends (" — Internet Archive")
    let t = t.split("—").next().unwrap_or(&t);
    // explicit volume/issue/number markers
    for pat in [
        r"\bvol\.?\s*\d+",
        r"\bno\.?\s*\d+",
        r"\bnos\.?\s*\d+",
        r"\bissue\s*\d+",
        r"\bnumber\s*\d+",
        r"\bnr\.?\s*\d+",
        r"\bissue\s*#?\s*\d{1,4}\b",
        r"\d+\s*(?:st|nd|rd|th)\s+(?:january|february|march|april|may|june|july|august|september|october|november|december)\b",
    ] {
        if Regex::new(pat).unwrap().is_match(t) {
            return true;
        }
    }
    // bare "March 2015" / "Mar 2015" — a month-year dateline
    let months = "january|february|march|april|may|june|july|august|september|october|november|december|jan|feb|mar|apr|jun|jul|aug|sep|sept|oct|nov|dec";
    if Regex::new(&format!(r"\b(?:{months})\.?\s+(?:19|20)\d{{2}}\b")).unwrap().is_match(t) {
        return true;
    }
    // ISO-ish date or a bare year in a periodical-ish context
    if Regex::new(r"\b(?:19|20)\d{2}-\d{2}-\d{2}\b").unwrap().is_match(t) {
        return true;
    }
    false
}

fn book_about_subject(title: &str, head: &str, text_lt: &str, lex: &[String]) -> bool {
    let titled = title.to_lowercase();
    if !lex.iter().any(|t| titled.contains(&t[..]) || head.contains(&t[..])) {
        return false;
    }
    let hits: usize = lex.iter().map(|t| text_lt.matches(&t[..]).count()).sum();
    // good books mention the subject 50+ times; passing mentions score <8
    hits >= 8
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
                                    crate::db::append("dict", serde_json::json!({"word": query, "extract": def}));
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
    // thesaurus record: entity + aliases persist for every later run
    crate::db::append(
        "thesaurus",
        serde_json::json!({"query": query, "subject": card.subject, "aliases": card.aliases, "instance": card.instance}),
    );
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
    // thin wrapper: the record is already ledgered inside fetch_measured,
    // so wiki/judge/second-hop fetches count as data too, not just the
    // crawl_urls ring
    let (res, rec) = fetch_measured(url).await;
    crate::db::append("fetch", serde_json::to_value(&rec).unwrap_or_default());
    res
}

/// Document fetch: download capped, content-type honored, text extracted.
/// A doc URL answering HTML (viewer page, login wall) falls back to the
/// page path — the server lied, not the link. Same ledger discipline as
/// fetch_measured; outcome vocabulary gains "oversize".
async fn fetch_doc_measured(url: &str, kind: crate::docs::DocKind, hint: &str) -> (Result<Page>, FetchRecord) {
    let host = host_of(url);
    let t0 = std::time::Instant::now();
    let c = client();
    let mut attempts = 0u8;
    let mut status: Option<u16> = None;
    let mut note = String::new();
    for _ in 0..2 {
        attempts += 1;
        let r = match c.get(url).send().await {
            Ok(r) => r,
            Err(e) => {
                note = format!("net:{}", e.to_string().chars().take(80).collect::<String>());
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
        };
        let st = r.status().as_u16();
        status = Some(st);
        if st == 429 || st == 503 {
            note = format!("{st} throttle");
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            continue;
        }
        if !r.status().is_success() {
            note = format!("{st}");
            break;
        }
        let ctype = r
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        // server answered HTML for a doc URL (viewer/landing/login page):
        // fetch it as a page instead of failing the link
        if ctype.contains("html") {
            return fetch_measured(url).await;
        }
        // content-length gate first (never download blind into 20MB+)
        if let Some(len) = r.headers().get("content-length").and_then(|v| v.to_str().ok()).and_then(|s| s.parse::<u64>().ok()) {
            if len > crate::docs::DOC_MAX_BYTES {
                let rec = FetchRecord {
                    url: url.to_string(),
                    host,
                    outcome: "oversize".into(),
                    status,
                    bytes: 0,
                    ms: t0.elapsed().as_millis() as u64,
                    attempts,
                    note: format!("content-length:{len}"),
                };
                return (Err(MagpieError::Network(format!("oversize {len}b {url}"))), rec);
            }
        }
        // stream with a hard ceiling (chunked bodies carry no length)
        use futures_util::StreamExt as _;
        let mut bytes: Vec<u8> = vec![];
        let mut over = false;
        let mut stream = r.bytes_stream();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(b) => {
                    bytes.extend_from_slice(&b);
                    if bytes.len() as u64 > crate::docs::DOC_MAX_BYTES {
                        over = true;
                        break;
                    }
                }
                Err(e) => {
                    note = format!("body:{e}");
                    break;
                }
            }
        }
        if over {
            let rec = FetchRecord {
                url: url.to_string(),
                host,
                outcome: "oversize".into(),
                status,
                bytes: bytes.len() as u64,
                ms: t0.elapsed().as_millis() as u64,
                attempts,
                note: "stream-ceiling".into(),
            };
            return (Err(MagpieError::Network(format!("oversize stream {url}"))), rec);
        }
        let nbytes = bytes.len() as u64;
        // content-type may refine the kind (server truth beats URL guess),
        // but never downgrades to HTML here (handled above)
        let kind = crate::docs::classify_ctype(&ctype).unwrap_or(kind);
        match crate::docs::extract(kind, &bytes) {
            Ok(text) => {
                let chars = text.chars().count();
                let title = crate::docs::title_for_hint(url, kind, hint);
                let rec = FetchRecord {
                    url: url.to_string(),
                    host,
                    outcome: "ok".into(),
                    status,
                    bytes: nbytes,
                    ms: t0.elapsed().as_millis() as u64,
                    attempts,
                    note: format!("{} {}c", crate::docs::kind_suffix(kind).to_lowercase(), chars),
                };
                return (
                    Ok(Page { url: url.to_string(), title, text: text.clone(), chars, dup_of: None }),
                    rec,
                );
            }
            Err(e) => {
                note = e.chars().take(80).collect();
                break;
            }
        }
    }
    let rec = FetchRecord {
        url: url.to_string(),
        host,
        outcome: "fail".into(),
        status,
        bytes: 0,
        ms: t0.elapsed().as_millis() as u64,
        attempts,
        note,
    };
    (Err(MagpieError::Network(format!("{} {url}", rec.note))), rec)
}

/// One ledger line per fetch attempt-chain. Failures are data: throttles,
/// blocks and dead hosts are what the next run plans around.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FetchRecord {
    pub url: String,
    pub host: String,
    /// ok | thin | fail | robots | non-html | bulk | oversize
    pub outcome: String,
    pub status: Option<u16>,
    pub bytes: u64,
    pub ms: u64,
    pub attempts: u8,
    pub note: String,
}

/// Per-host roll-up for seek.log and seek.json: (host, ok, fail, other, median_ms)
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FetchHost {
    pub host: String,
    pub ok: usize,
    pub fail: usize,
    pub other: usize,
    pub ms_med: u64,
}

pub fn summarize_fetches(recs: &[FetchRecord]) -> Vec<FetchHost> {
    let mut by_host: std::collections::HashMap<String, Vec<&FetchRecord>> = Default::default();
    for r in recs {
        by_host.entry(r.host.clone()).or_default().push(r);
    }
    let mut out: Vec<FetchHost> = by_host
        .into_iter()
        .map(|(host, rs)| {
            let ok = rs.iter().filter(|r| r.outcome == "ok").count();
            let fail = rs.iter().filter(|r| r.outcome == "fail").count();
            let other = rs.len() - ok - fail;
            let mut ms: Vec<u64> = rs.iter().map(|r| r.ms).collect();
            ms.sort_unstable();
            let ms_med = ms.get(ms.len() / 2).copied().unwrap_or(0);
            FetchHost { host, ok, fail, other, ms_med }
        })
        .collect();
    out.sort_by(|a, b| b.ok.cmp(&a.ok).then(a.host.cmp(&b.host)));
    out
}

async fn fetch_measured(url: &str) -> (Result<Page>, FetchRecord) {
    let host = host_of(url);
    let t0 = std::time::Instant::now();
    let c = client();
    let mut attempts = 0u8;
    let mut status: Option<u16> = None;
    let mut note = String::new();
    // one retry, only for throttles/overload: 429/503 recover if out-waited
    // (DDG/Marginalia throttle this IP intermittently). 4xx never retried.
    for _ in 0..2 {
        attempts += 1;
        match c.get(url).send().await {
            Ok(r) => {
                let st = r.status().as_u16();
                status = Some(st);
                if st == 429 || st == 503 {
                    note = format!("{st} throttle");
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    continue;
                }
                if !r.status().is_success() {
                    note = format!("{st}");
                    break;
                }
                let ctype = r
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                // non-HTML bodies (PDF/epub/images): ledger, never ingest
                // as text (r.text() on a PDF is mojibake, not content)
                if !ctype.is_empty()
                    && !ctype.contains("html")
                    && !ctype.contains("text")
                    && !ctype.contains("xml")
                    && !ctype.contains("json")
                {
                    let rec = FetchRecord {
                        url: url.to_string(),
                        host,
                        outcome: "non-html".into(),
                        status,
                        bytes: 0,
                        ms: t0.elapsed().as_millis() as u64,
                        attempts,
                        note: ctype.chars().take(60).collect(),
                    };
                    return (Err(MagpieError::Network(format!("non-html {ctype} {url}"))), rec);
                }
                let html = match r.text().await {
                    Ok(t) => t,
                    Err(e) => {
                        note = format!("body:{e}");
                        break;
                    }
                };
                let bytes = html.len() as u64;
                let (title, text) = extract_text(&html);
                let chars = text.chars().count();
                let rec = FetchRecord {
                    url: url.to_string(),
                    host,
                    outcome: "ok".into(),
                    status,
                    bytes,
                    ms: t0.elapsed().as_millis() as u64,
                    attempts,
                    note: format!("{chars}c"),
                };
                return (
                    Ok(Page { url: url.to_string(), title, text: text.clone(), chars, dup_of: None }),
                    rec,
                );
            }
            Err(e) => {
                // connect/dns/timeout: one retry after a breath
                note = format!("net:{}", e.to_string().chars().take(80).collect::<String>());
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
    let rec = FetchRecord {
        url: url.to_string(),
        host,
        outcome: "fail".into(),
        status,
        bytes: 0,
        ms: t0.elapsed().as_millis() as u64,
        attempts,
        note,
    };
    (Err(MagpieError::Network(format!("{} {url}", rec.note))), rec)
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

/// Seznam (Czech independent index): plain GET, direct result anchors,
/// no wall from this box (verified live 2026-10-11: Czech car results,
/// no captcha). Browser UA required — the house UA gets the bot stare.
async fn seznam_search(query: &str, n: usize) -> Vec<String> {
    let c = reqwest::Client::builder()
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0")
        .timeout(std::time::Duration::from_secs(20))
        .build();
    let c = match c {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let url = format!(
        "https://search.seznam.cz/?q={}",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    let html = match c.get(&url).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let re_a = Regex::new(r#"(?i)<a[^>]+href="(https?://[^"]+)"[^>]*>"#).unwrap();
    let mut out = vec![];
    let mut seen = std::collections::HashSet::new();
    for cap in re_a.captures_iter(&html) {
        let h = cap[1].to_string();
        let l = h.to_lowercase();
        // chrome, not results (nav, help, browser promos, thumbnails)
        if l.contains("seznam.cz") || l.contains("o-seznam") {
            continue;
        }
        if !seen.insert(h.clone()) {
            continue;
        }
        out.push(h);
        if out.len() >= n {
            break;
        }
    }
    out
}

/// Per-engine outcome history for the measured pool. Reads the tail of
/// db/engines.jsonl (cheap: last 300 lines) → name -> (success_rate,
/// consecutive_recent_fails, newest_row_hours_ago). Pure over lines:
/// unit-tested. Rows carry _at timestamps (db::append stamps them).
fn engine_stats(lines: &[String], now_h: f64) -> std::collections::HashMap<String, (f64, usize, f64)> {
    // name -> (oks, consec_fails, total, newest_age_h)
    let mut acc: std::collections::HashMap<String, (usize, usize, usize, f64)> = Default::default();
    for line in lines.iter().rev().take(300) {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let name = v.get("engine").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if name.is_empty() {
            continue;
        }
        let ok = v.get("urls").and_then(|x| x.as_u64()).unwrap_or(0) > 0;
        let age_h = v
            .get("_at")
            .and_then(|x| x.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| now_h - t.timestamp() as f64 / 3600.0)
            .unwrap_or(999.0);
        let e = acc.entry(name).or_insert((0, 0, 0, age_h));
        if e.3 > age_h {
            e.3 = age_h; // newest-first scan: first seen is freshest... keep min
        }
        if ok {
            e.0 += 1;
            e.1 = 0; // success resets the fail streak (newest-first scan)
        } else if e.0 == 0 {
            // only leading (most-recent) fails count as consecutive
            e.1 += 1;
        }
        e.2 += 1;
    }
    acc.into_iter()
        .map(|(k, (oks, fails, total, age))| {
            let rate = if total == 0 { 0.0 } else { oks as f64 / total as f64 };
            (k, (rate, fails, age))
        })
        .collect()
}

/// Last 300 lines of db/engines.jsonl (empty when the pool is young).
fn engine_history() -> Vec<String> {
    let path = crate::db::db_dir().join("engines.jsonl");
    let txt = std::fs::read_to_string(path).unwrap_or_default();
    txt.lines().rev().take(300).map(|s| s.to_string()).collect::<Vec<_>>().into_iter().rev().collect()
}

/// Sideline rule: ≥3 consecutive recent fails AND the latest attempt is
/// fresh (<2h) → skip this run. Stale failures parole automatically
/// (a sidelined engine that is never tried could never recover).
fn engine_sidelined(fails: usize, newest_age_h: f64) -> bool {
    fails >= 3 && newest_age_h < 2.0
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

/// A rejected work: quarantined, never deleted, reason on file.
/// Dugin goes here with "off-subject" attached — the next run (or the
/// rinse pass) can audit refusals instead of trusting them.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Quarantine {
    pub title: String,
    pub url: String,
    pub leg: String,
    pub reason: String,
}

/// Internet Archive books: public-domain/open full text only.
/// advancedsearch → metadata (confirm `_djvu.txt` exists) → download.
/// Lending-library and restricted items never qualify — listed, never fetched.
/// `search_q` is the IA query (may be chapter-augmented); gating always uses
/// the SUBJECT lexicon (original query + card), never the augmented string.
async fn ia_books(search_q: &str, subject_q: &str, card: &QueryCard, prog: &Option<tokio::sync::mpsc::UnboundedSender<String>>, max_books: usize) -> (Vec<Page>, Vec<Quarantine>) {
    let send = |m: &str| {
        if let Some(tx) = prog {
            let _ = tx.send(m.to_string());
        }
    };
    send("search: archive.org books…");
    let c = client();
    let q = format!(
        "https://archive.org/advancedsearch.php?q={}+AND+mediatype%3Atexts&fl%5B%5D=identifier&fl%5B%5D=title&fl%5B%5D=year&rows=12&output=json",
        url::form_urlencoded::byte_serialize(search_q.as_bytes()).collect::<String>()
    );
    let txt = match c.get(&q).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return (vec![], vec![]),
    };
    let v: serde_json::Value = match serde_json::from_str(&txt) {
        Ok(v) => v,
        Err(_) => return (vec![], vec![]),
    };
    let docs = v["response"]["docs"].as_array().cloned().unwrap_or_default();
    let mut out = vec![];
    let mut refused: Vec<Quarantine> = vec![];
    // every refusal carries its reason: quarantine is auditable, trust is not
    let mut refuse = |id: &str, title: &str, reason: String| {
        refused.push(Quarantine {
            title: title.to_string(),
            url: format!("https://archive.org/details/{id}"),
            leg: "ia-books".into(),
            reason,
        });
    };
    for d in docs {
        if out.len() >= max_books.max(1) {
            break;
        }
        let id = d.get("identifier").and_then(|x| x.as_str()).unwrap_or("").to_string();
        let title = d.get("title").and_then(|x| x.as_str()).unwrap_or(&id).to_string();
        if id.is_empty() {
            continue;
        }
        // metadata: resolve a full-text file, preferring _djvu.txt, then
        // EPUB, then PDF (many items lack djvu.txt but ship the others —
        // refusing them all starves the canon). NOTE: newer items name files
        // by title, not identifier — resolve exact names, never guess.
        let meta = match c.get(&format!("https://archive.org/metadata/{id}")).send().await {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(_) => {
                refuse(&id, &title, "no-metadata".into());
                continue;
            }
        };
        // (file name, is_plain_text): djvu.txt reads as text, the rest parse
        let txt_name: Option<(String, bool)> = (|| {
            let v: serde_json::Value = serde_json::from_str(&meta).ok()?;
            let files = v.get("files")?.as_array()?;
            let mut pick = |sufs: &[&str]| -> Option<String> {
                let mut cands: Vec<String> = vec![];
                for f in files {
                    let n = f.get("name")?.as_str()?;
                    if sufs.iter().any(|s| n.ends_with(s)) {
                        cands.push(n.to_string());
                    }
                }
                // prefer the identifier-stemmed file, else first candidate
                cands
                    .iter()
                    .find(|n| n.starts_with(&id))
                    .cloned()
                    .or_else(|| cands.into_iter().next())
            };
            if let Some(n) = pick(&["_djvu.txt"]) {
                return Some((n, true));
            }
            if let Some(n) = pick(&[".epub"]) {
                return Some((n, false));
            }
            // any pdf: extraction decides (scanned-image PDFs come back
            // empty and die at the thin gate with cause)
            if let Some(n) = pick(&[".pdf"]) {
                return Some((n, false));
            }
            None
        })();
        let (txt_name, is_text) = match txt_name {
            Some(n) => n,
            None => {
                refuse(&id, &title, "no-fulltext".into());
                continue;
            }
        };
        let text: String = if is_text {
            let body = match c.get(&format!("https://archive.org/download/{id}/{txt_name}")).send().await {
                Ok(r) => {
                    // restricted/lending items answer 403 with an error page —
                    // never ingest those as book text
                    if !r.status().is_success() {
                        refuse(&id, &title, format!("restricted:{}", r.status().as_u16()));
                        continue;
                    }
                    r.text().await.unwrap_or_default()
                }
                Err(_) => {
                    refuse(&id, &title, "download-failed".into());
                    continue;
                }
            };
            if body.contains("Page not found on Internet Archive") {
                refuse(&id, &title, "not-found".into());
                continue;
            }
            // full text, not a sample: books are chunked downstream, and
            // runtime no longer matters more than coverage (cap ~400KB)
            body.chars().take(400_000).collect()
        } else {
            // EPUB/PDF full text through the doc pipeline (same caps,
            // same bomb guards as web documents)
            let file_url = format!("https://archive.org/download/{id}/{txt_name}");
            let kind = if txt_name.ends_with(".epub") {
                crate::docs::DocKind::Epub
            } else {
                crate::docs::DocKind::Pdf
            };
            match fetch_doc_measured(&file_url, kind, &title).await {
                (Ok(p), _) => {
                    // retitle to the WORK (file stems are noise)
                    p.text
                }
                (Err(_), rec) => {
                    refuse(&id, &title, format!("bookfile:{}", rec.note.chars().take(40).collect::<String>()));
                    continue;
                }
            }
        };
        if text.chars().count() < 2000 {
            refuse(&id, &title, format!("thin:{}c", text.chars().count()));
            continue;
        }
        // intake gate: the whole work must be ABOUT the subject (title or
        // front matter names it, full text mentions it densely). Gated on
        // the SUBJECT lexicon — chapter-augmented search words never count.
        // (Politica Aeterna rode "history" in; poetry volumes ride title
        // substrings. Both classes die here now.)
        let lex = subject_lexicon(subject_q, card);
        let lt = text.to_lowercase();
        let head: String = text.chars().take(1500).collect::<String>().to_lowercase();
        // containers first: a magazine issue is not a work ABOUT the
        // subject however densely the subject is mentioned inside it.
        if is_periodical(&title) {
            send(&format!("search: archive.org skip {title} (periodical container)"));
            refuse(&id, &title, "periodical".into());
            continue;
        }
        if !book_about_subject(&title, &head, &lt, &lex) {
            send(&format!("search: archive.org skip {title} (off-subject)"));
            refuse(&id, &title, "off-subject".into());
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
    (out, refused)
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

/// Tier 2 source legs (all keyless, all verified live from this box):
/// scholarly (OpenAlex, arXiv), experience (HN, StackExchange, Lemmy),
/// structured (DBpedia). Scholarly/structured legs yield synthetic Pages
/// (API content is substantive); experience legs yield discussion URLs for
/// the normal fetch+score pipeline (question pages, not snippets — thin
/// content must die at the gates, not in the leg). Every outcome ledgered
/// to db/sources (yields per source, ms per call).

/// Title carries a query token (len>3), raw or stemmed — "hermeticism"
/// must match "Hermetic Tradition" (a test caught the raw-only gap).
/// The scholarly title gate: OpenAlex returns 100K+ passing mentions;
/// only titled works qualify (same philosophy as the book title gate).
fn title_matches(query: &str, title: &str) -> bool {
    let lt = title.to_lowercase();
    query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 3)
        .any(|t| lt.contains(t) || lt.contains(&stem_token(t)[..]))
}

/// OpenAlex scholarly works: abstract + authors + venue + year + DOI.
/// Anonymous lane (no key, no mailto); identified UA per their etiquette.
async fn openalex_pages(query: &str) -> Vec<Page> {
    let c = client();
    let url = format!(
        "https://api.openalex.org/works?search={}&per-page=6",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    let txt = match c
        .get(&url)
        .header(reqwest::header::USER_AGENT, "magpie-research/0.1 (https://github.com/ElegantVW/magpie)")
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or(serde_json::Value::Null);
    let results = v.get("results").and_then(|r| r.as_array()).cloned().unwrap_or_default();
    let mut out = vec![];
    for w in results {
        let title = w.get("display_name").and_then(|x| x.as_str()).unwrap_or("").to_string();
        if title.is_empty() || !title_matches(query, &title) {
            continue;
        }
        // abstract_inverted_index: word -> positions; rebuild by position
        let mut words: Vec<(usize, String)> = vec![];
        if let Some(idx) = w.get("abstract_inverted_index").and_then(|x| x.as_object()) {
            for (word, pos) in idx {
                if let Some(arr) = pos.as_array() {
                    for p in arr.iter().filter_map(|x| x.as_u64()) {
                        words.push((p as usize, word.clone()));
                    }
                }
            }
        }
        words.sort_by_key(|(p, _)| *p);
        let abs: String = words.iter().map(|(_, word)| word.clone()).collect::<Vec<_>>().join(" ").chars().take(3000).collect();
        let authors: String = w
            .get("authorships")
            .and_then(|a| a.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("author").and_then(|au| au.get("display_name")).and_then(|n| n.as_str()))
                    .take(6)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let year = w.get("publication_year").map(|y| y.to_string()).unwrap_or_else(|| "?".into());
        let venue = w
            .get("primary_location")
            .and_then(|l| l.get("source"))
            .and_then(|s| s.get("display_name"))
            .and_then(|n| n.as_str())
            .unwrap_or("?");
        let doi = w.get("doi").and_then(|x| x.as_str()).unwrap_or("");
        let cited = w.get("cited_by_count").map(|x| x.to_string()).unwrap_or_else(|| "?".into());
        let text = format!("{title}\n{authors} ({year}). {venue}. Cited by {cited}.\n{doi}\n\n{abs}");
        let chars = text.chars().count();
        if chars < 300 {
            continue;
        }
        out.push(Page {
            url: format!("https://openalex.org/works/{}", w.get("id").and_then(|x| x.as_str()).unwrap_or("").rsplit('/').next().unwrap_or("")),
            title: format!("{title} — OpenAlex"),
            text,
            chars,
            dup_of: None,
        });
        if out.len() >= 3 {
            break;
        }
    }
    out
}

/// arXiv preprints: title-gated like OpenAlex (feed XML parsed by regex —
/// no XML dep for one feed). Returns (pages, pdf_urls): PDF links flow
/// into the doc pipeline (extensionless arxiv URLs need the non-html
/// reroute in crawl_urls). One request per run (arXiv asks 3s spacing).
async fn arxiv_pages(query: &str) -> (Vec<Page>, Vec<(String, String)>) {
    let c = client();
    let url = format!(
        "https://export.arxiv.org/api/query?search_query=all:{}&max_results=5&sortBy=relevance&sortOrder=descending",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    let txt = match c
        .get(&url)
        .header(reqwest::header::USER_AGENT, "magpie-research/0.1 (https://github.com/ElegantVW/magpie)")
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return (vec![], vec![]),
    };
    let re_entry = Regex::new(r"(?s)<entry>(.*?)</entry>").unwrap();
    let field = |block: &str, tag: &str| -> String {
        Regex::new(&format!(r"(?s)<{tag}[^>]*>(.*?)</{tag}>", tag = tag))
            .ok()
            .and_then(|re| re.captures(block).map(|c| c[1].to_string()))
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut pages = vec![];
    let mut pdfs = vec![];
    for cap in re_entry.captures_iter(&txt) {
        let b = &cap[1];
        let title = field(b, "title");
        if title.is_empty() || !title_matches(query, &title) {
            continue;
        }
        let summary: String = field(b, "summary").chars().take(2500).collect();
        let id = field(b, "id");
        let arxiv_id = id.rsplit('/').next().unwrap_or("").to_string();
        let authors: Vec<String> = Regex::new(r"<name>(.*?)</name>")
            .unwrap()
            .captures_iter(b)
            .map(|c| c[1].to_string())
            .take(5)
            .collect();
        let text = format!("{title}\n{}.\n{}\n\n{summary}", authors.join(", "), arxiv_id);
        let chars = text.chars().count();
        if chars < 300 {
            continue;
        }
        if !arxiv_id.is_empty() {
            pdfs.push((format!("https://arxiv.org/pdf/{arxiv_id}"), title.clone()));
        }
        pages.push(Page {
            url: format!("https://arxiv.org/abs/{arxiv_id}"),
            title: format!("{title} — arXiv"),
            text,
            chars,
            dup_of: None,
        });
        if pages.len() >= 3 {
            break;
        }
    }
    (pages, pdfs)
}

/// HN Algolia: story links only (comments are thin here; discussion
/// quality lives on Lemmy/SE). URLs join the normal fetch pipeline.
async fn hn_urls(query: &str) -> Vec<String> {
    let c = client();
    let url = format!(
        "https://hn.algolia.com/api/v1/search?query={}&tags=story&hitsPerPage=5",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    );
    let txt = match c.get(&url).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or(serde_json::Value::Null);
    v.get("hits")
        .and_then(|h| h.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|h| h.get("url").and_then(|u| u.as_str()).map(|s| s.to_string()))
                .filter(|u| u.starts_with("http"))
                .take(4)
                .collect()
        })
        .unwrap_or_default()
}

/// StackExchange sites per domain (intitle search, relevance sort).
/// Question LINKS join the pipeline (full pages score normally; snippets
/// would die thin at the gates — same rule as HN).
fn se_sites(query: &str, domain: &str) -> Vec<&'static str> {
    let lq = query.to_lowercase();
    if lq.contains("cocktail") || lq.contains("mixolog") || lq.contains("drink") || lq.contains("recipe") || lq.contains("food") || lq.contains("cook") {
        return vec!["cooking", "alcohol"];
    }
    match domain {
        "vehicle" => vec!["mechanics"],
        "place" => vec!["travel"],
        "field" => vec!["philosophy", "history"],
        _ => vec!["superuser"],
    }
}

async fn se_urls(query: &str, domain: &str) -> Vec<String> {
    let c = client();
    let mut out = vec![];
    for site in se_sites(query, domain) {
        let url = format!(
            "https://api.stackexchange.com/2.3/search?order=desc&sort=relevance&intitle={}&site={}&pagesize=3",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>(),
            site
        );
        let txt = match c.get(&url).send().await {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(_) => continue,
        };
        let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or(serde_json::Value::Null);
        if let Some(items) = v.get("items").and_then(|i| i.as_array()) {
            for it in items.iter().take(2) {
                if let Some(link) = it.get("link").and_then(|l| l.as_str()) {
                    out.push(link.to_string());
                }
            }
        }
        if out.len() >= 4 {
            break;
        }
    }
    out
}

/// Lemmy discussion, instance-rotated (.world and sopuli are dead from
/// this box; rotation is the lesson). First instance with results wins;
/// every tried instance ledgered (failures are data). Top comments become
/// ONE attributed digest page (lived experience, voiced as such — the
/// attribution prompt line makes opinionated sources safe); post links
/// join the normal pipeline.
const LEMMY_INSTANCES: &[&str] = &["https://lemmy.ml", "https://lemmy.world", "https://sopuli.xyz", "https://lemmy.dbzer0.com"];

async fn lemmy_discussion(query: &str, prog: &Option<tokio::sync::mpsc::UnboundedSender<String>>) -> (Option<Page>, Vec<String>) {
    let send = |m: &str| {
        if let Some(tx) = prog {
            let _ = tx.send(m.to_string());
        }
    };
    let c = client();
    for inst in LEMMY_INSTANCES {
        let t0 = std::time::Instant::now();
        let url = format!(
            "{inst}/api/v3/search?q={}&type_=All&limit=4",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        );
        let txt = match tokio::time::timeout(std::time::Duration::from_secs(15), c.get(&url).send()).await {
            Ok(Ok(r)) => r.text().await.unwrap_or_default(),
            _ => {
                crate::db::append("sources", serde_json::json!({"source": format!("lemmy:{inst}"), "query": query, "n": 0, "ms": t0.elapsed().as_millis() as u64}));
                continue;
            }
        };
        let v: serde_json::Value = serde_json::from_str(&txt).unwrap_or(serde_json::Value::Null);
        let mut lines = vec![format!("Lemmy discussion on “{query}” (voices, not verdicts):")];
        let mut urls = vec![];
        if let Some(posts) = v.get("posts").and_then(|p| p.as_array()) {
            for p in posts.iter().take(3) {
                let name = p.get("post").and_then(|x| x.get("name")).and_then(|x| x.as_str()).unwrap_or("");
                let ap = p.get("post").and_then(|x| x.get("ap_id")).and_then(|x| x.as_str()).unwrap_or("");
                let who = p.get("creator").and_then(|x| x.get("name")).and_then(|x| x.as_str()).unwrap_or("?");
                if name.is_empty() {
                    continue;
                }
                lines.push(format!("- u/{who}: {name}"));
                if !ap.is_empty() && urls.len() < 4 {
                    urls.push(ap.to_string());
                }
            }
        }
        if let Some(comments) = v.get("comments").and_then(|x| x.as_array()) {
            for cm in comments.iter().take(4) {
                let body: String = cm
                    .get("comment")
                    .and_then(|x| x.get("content"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(500)
                    .collect();
                let who = cm.get("creator").and_then(|x| x.get("name")).and_then(|x| x.as_str()).unwrap_or("?");
                if body.chars().count() > 60 {
                    lines.push(format!("- u/{who} writes: {body}"));
                }
            }
        }
        let n = urls.len() + lines.len().saturating_sub(1);
        crate::db::append("sources", serde_json::json!({"source": format!("lemmy:{inst}"), "query": query, "n": n, "ms": t0.elapsed().as_millis() as u64}));
        if n == 0 {
            continue;
        }
        send(&format!("search: lemmy ({inst}) got {n}"));
        let text = lines.join("\n");
        let chars = text.chars().count();
        let page = if chars >= 200 {
            Some(Page { url: format!("{inst}/search?q={query}"), title: format!("Lemmy voices: {query}"), text, chars, dup_of: None })
        } else {
            None
        };
        return (page, urls);
    }
    (None, vec![])
}

/// DBpedia structured top-up: enwiki title → resource JSON → labels in
/// languages Wikidata missed + abstract page. Deterministic, no scraping.
async fn dbpedia_topup(query: &str, have_langs: &std::collections::HashSet<String>) -> (Vec<(String, String, Vec<String>)>, Option<Page>) {
    let c = client();
    // enwiki title for the query (limit 1)
    let s = match c
        .get(&format!(
            "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit=1&format=json",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        ))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return (vec![], None),
    };
    let v: serde_json::Value = serde_json::from_str(&s).unwrap_or(serde_json::Value::Null);
    let title = v
        .as_array()
        .and_then(|a| a.get(1))
        .and_then(|x| x.as_array())
        .and_then(|a| a.first())
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    if title.is_empty() {
        return (vec![], None);
    }
    let resource = title.replace(' ', "_");
    let t = match c
        .get(&format!(
            "https://dbpedia.org/data/{}.json",
            url::form_urlencoded::byte_serialize(resource.as_bytes()).collect::<String>()
        ))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return (vec![], None),
    };
    let v: serde_json::Value = serde_json::from_str(&t).unwrap_or(serde_json::Value::Null);
    let mut out = vec![];
    let mut abstract_en = String::new();
    if let Some(obj) = v.as_object().and_then(|m| m.values().next()).and_then(|e| e.as_object()) {
        if let Some(labels) = obj.get("http://www.w3.org/2000/01/rdf-schema#label").and_then(|l| l.as_array()) {
            for l in labels {
                let lang = l.get("lang").and_then(|x| x.as_str()).unwrap_or("").to_string();
                let val = l.get("value").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if lang.is_empty() || lang == "en" || val.is_empty() || have_langs.contains(&lang) {
                    continue;
                }
                // per-language opensearch like the Wikidata leg (limit 2)
                let u = format!(
                    "https://{lang}.wikipedia.org/w/api.php?action=opensearch&search={}&limit=2&format=json",
                    url::form_urlencoded::byte_serialize(val.as_bytes()).collect::<String>()
                );
                let r = match c.get(&u).send().await {
                    Ok(r) => r.text().await.unwrap_or_default(),
                    Err(_) => continue,
                };
                let rv: serde_json::Value = serde_json::from_str(&r).unwrap_or(serde_json::Value::Null);
                let mut urls = vec![];
                if let Some(arr) = rv.as_array() {
                    if let Some(links) = arr.get(3).and_then(|x| x.as_array()) {
                        for link in links.iter().take(2) {
                            if let Some(s) = link.as_str() {
                                urls.push(s.to_string());
                            }
                        }
                    }
                }
                if !urls.is_empty() {
                    out.push((lang, val, urls));
                }
                if out.len() >= 6 {
                    break;
                }
            }
        }
        if let Some(abs) = obj.get("http://dbpedia.org/ontology/abstract").and_then(|l| l.as_array()) {
            for a in abs {
                if a.get("lang").and_then(|x| x.as_str()) == Some("en") {
                    abstract_en = a.get("value").and_then(|x| x.as_str()).unwrap_or("").chars().take(3000).collect();
                    break;
                }
            }
        }
    }
    let page = if abstract_en.chars().count() >= 300 {
        let chars = abstract_en.chars().count();
        Some(Page {
            url: format!("https://dbpedia.org/page/{resource}"),
            title: format!("{title} — DBpedia"),
            text: abstract_en,
            chars,
            dup_of: None,
        })
    } else {
        None
    };
    (out, page)
}

/// One ledger row per source-leg call (yields + ms). The engines table
/// stays engines-only; sources are not interchangeable providers.
fn ledger_source(leg: &str, query: &str, n: usize, ms: u64) {
    crate::db::append("sources", serde_json::json!({"source": leg, "query": query, "n": n, "ms": ms}));
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

/// Thirty languages, fixed list (top Wikipedias). Translations come from
/// the entity's own labels — deterministic, no MT, no key. Each language
/// then searches IN its own tongue through its own Wikipedia.
const LANGS30: &[&str] = &[
    "en", "de", "fr", "es", "it", "pt", "nl", "pl", "ru", "ja",
    "zh", "sv", "fi", "no", "da", "cs", "hu", "ro", "tr", "ar",
    "fa", "hi", "ko", "id", "ms", "th", "vi", "el", "he", "uk",
];

/// Translations of the query entity + per-language Wikipedia fan-out.
/// Returns (lang, label, urls). Capped at 12 productive languages.
async fn translations(query: &str, prog: &Option<tokio::sync::mpsc::UnboundedSender<String>>) -> Vec<(String, String, Vec<String>)> {
    let send = |m: &str| {
        if let Some(tx) = prog {
            let _ = tx.send(m.to_string());
        }
    };
    let c = client();
    let s = match c
        .get(&format!(
            "https://www.wikidata.org/w/api.php?action=wbsearchentities&search={}&language=en&format=json&limit=1",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        ))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let id: String = match Regex::new(r#""id":"(Q\d+)""#)
        .unwrap()
        .captures(&s)
        .map(|c| c[1].to_string())
    {
        Some(id) => id,
        None => return vec![],
    };
    let t = match c
        .get(&format!("https://www.wikidata.org/w/api.php?action=wbgetentities&ids={id}&props=labels&format=json"))
        .send()
        .await
    {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(_) => return vec![],
    };
    let v: serde_json::Value = serde_json::from_str(&t).unwrap_or(serde_json::Value::Null);
    let labels = v.get("entities").and_then(|e| e.get(&id)).and_then(|e| e.get("labels"));
    let mut out = vec![];
    for lang in LANGS30.iter().filter(|l| **l != "en") {
        let label = labels
            .and_then(|l| l.get(*lang))
            .and_then(|l| l.get("value"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if label.is_empty() {
            continue;
        }
        let u = format!(
            "https://{lang}.wikipedia.org/w/api.php?action=opensearch&search={}&limit=2&format=json",
            url::form_urlencoded::byte_serialize(label.as_bytes()).collect::<String>()
        );
        let r = match c.get(&u).send().await {
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(_) => continue,
        };
        let v: serde_json::Value = serde_json::from_str(&r).unwrap_or(serde_json::Value::Null);
        let mut urls = vec![];
        if let Some(arr) = v.as_array() {
            if let Some(links) = arr.get(3).and_then(|x| x.as_array()) {
                for link in links.iter().take(2) {
                    if let Some(s) = link.as_str() {
                        urls.push(s.to_string());
                    }
                }
            }
        }
        if !urls.is_empty() {
            out.push((lang.to_string(), label, urls));
        }
        if out.len() >= 12 {
            break;
        }
    }
    send(&format!("search: translations ({} langs with hits)", out.len()));
    out
}

/// Verification search for the judge: distinct query per claim,
/// top hits only. Part of the ≥10-searches-of-our-own evidence round.
pub(crate) async fn verify_search(claim: &str) -> Vec<String> {
    let full: String = claim.chars().take(80).collect();
    let mut out = search_clean(&full).await;
    if out.len() < 2 {
        // rephrase when the sentence form starves: engines match keyword
        // queries better, and an empty first round must not become a
        // verdict against the manual (retrieval failure ≠ guilt)
        let keys: String = claim
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| w.len() > 3)
            .take(8)
            .collect::<Vec<_>>()
            .join(" ");
        if !keys.is_empty() && keys != full {
            for u in search_clean(&keys).await {
                if out.len() >= 3 {
                    break;
                }
                if !out.contains(&u) {
                    out.push(u);
                }
            }
        }
    }
    out.into_iter().take(3).collect()
}

async fn search_clean(q: &str) -> Vec<String> {
    let mut out = ddg_search_tries(q, 3, 2, 10).await;
    if out.is_empty() {
        out = marginalia_tries(q, 3, 2, 10).await;
    }
    out.into_iter().filter(|u| !junk_evidence_url(u)).take(3).collect()
}

/// Issue trackers read as "evidence" but carry no checkable content: a REDO
/// once blamed the manual for GitHub-issue "sources" the engines coughed up
/// while throttled. Filtered before fetching (saves fetches too).
fn junk_evidence_url(u: &str) -> bool {
    let l = u.to_lowercase();
    l.contains("/issues/")
        || l.contains("/pull/")
        || l.contains("/discussions/")
        || l.contains("/-/issues")
        || l.contains("/-/merge_requests")
}

fn wikipedia_api(query: &str) -> String {
    format!(
        "https://en.wikipedia.org/w/api.php?action=query&list=search&srsearch={}&format=json&srlimit=5",
        url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
    )
}

/// Snippet fetch for evidence: title + first 600 chars, or None.
pub(crate) async fn fetch_snippet(url: &str) -> Option<String> {
    match fetch_one(url).await {
        Ok(p) => {
            let t: String = p.text.chars().take(600).collect();
            if t.chars().count() < 80 {
                return None;
            }
            Some(format!("{} — {}", p.title.chars().take(80).collect::<String>(), t))
        }
        Err(_) => None,
    }
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
    /// LLM-spawned web queries from the expansion pass
    #[serde(default)]
    pub expansion: Vec<String>,
    /// per-language query forms: (lang, label, urls found)
    #[serde(default)]
    pub translations: Vec<(String, String, String)>,
    /// refused works with reasons (quarantine, not deletion)
    #[serde(default)]
    pub quarantine: Vec<Quarantine>,
    /// URL → display-title hints for fetched documents (arXiv paper titles;
    /// bare numeric stems name nothing)
    #[serde(default)]
    pub title_hints: std::collections::HashMap<String, String>,
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
    let mut plan_quarantine: Vec<Quarantine> = vec![];
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

    // expansion pass: one LLM call reads the query + card and spawns
    // additional web queries from what it knows (subtopics, aliases,
    // technique queries). Stored in seek.json as `expansion`.
    let mut expansion_queries: Vec<String> = vec![];
    {
        let (endpoint, _) = crate::llm::llm_endpoint().await;
        let client = client();
        let sys = "You expand a research query into web searches. Reply ONLY JSON.";
        let user = serde_json::json!({
            "query": query,
            "meaning": card.meaning,
            "aliases": card.aliases,
            "domain": subject_domain(query, &card.instance),
            "want": {"queries": ["6-10 distinct web search queries: subtopics, alias forms, key figures, technique queries"]}
        })
        .to_string();
        if let Some(o) = crate::llm::chat_json(&client, &endpoint, sys, &user, 300, 0.2).await {
            if let Some(arr) = o.v.get("queries").and_then(|q| q.as_array()) {
                for q in arr.iter().take(10) {
                    if let Some(s) = q.as_str() {
                        let s = s.trim().to_string();
                        if !s.is_empty() && s.len() < 120 {
                            expansion_queries.push(s);
                        }
                    }
                }
            }
        }
        if !expansion_queries.is_empty() {
            send(&format!("understand: +{} spawned queries", expansion_queries.len()));
        }
        crate::db::append(
            "expansions",
            serde_json::json!({"query": query, "queries": expansion_queries}),
        );
    }

    // measured engine pool: DDG + Marginalia + Seznam, ordered by recent
    // success, sidelined on fresh fail streaks, every outcome ledgered.
    // (15 engines probed live 2026-10-11 from this box; Seznam is the only
    // survivor besides the throttled incumbents. Failures are data: the
    // planner routes around throttles instead of hoping.)
    let now_h = chrono::Utc::now().timestamp() as f64 / 3600.0;
    let stats = engine_stats(&engine_history(), now_h);
    let mut pool: Vec<(&str, f64)> = ["duckduckgo", "marginalia", "seznam"]
        .iter()
        .map(|e| (*e, stats.get(*e).map(|s| s.0).unwrap_or(0.5)))
        .collect();
    // unknown engines (no history) sort middle (0.5): tried, not trusted
    pool.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut raw: Vec<String> = vec![];
    for (name, _rate) in pool {
        let (fails, age) = stats.get(name).map(|s| (s.1, s.2)).unwrap_or((0, 999.0));
        if engine_sidelined(fails, age) {
            send(&format!("search: {name} sidelined (3 straight fresh fails)"));
            continue;
        }
        send(&format!("search: {name}…"));
        let t0 = std::time::Instant::now();
        let found: Vec<String> = match tokio::time::timeout(
            std::time::Duration::from_secs(20),
            async {
                match name {
                    "duckduckgo" => ddg_search(query, per_engine).await,
                    "marginalia" => marginalia_search(query, per_engine).await,
                    _ => seznam_search(query, per_engine).await,
                }
            },
        )
        .await
        {
            Ok(v) => v,
            Err(_) => {
                send(&format!("search: {name} timed out (20s), continuing…"));
                vec![]
            }
        };
        let ms = t0.elapsed().as_millis() as u64;
        send(&format!("search: {name} got {}", found.len()));
        crate::db::append(
            "engines",
            serde_json::json!({"engine": name, "query": query, "urls": found.len(), "ms": ms}),
        );
        legs.push((format!("engines:{name}"), found.len()));
        raw.extend(found);
    }

    // raw already holds the pooled engine hits above
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
        let mut refused: Vec<Quarantine> = vec![];
        send("search: books (archive.org)…");
        let (found, mut r) = ia_books(query, query, &card, &prog, 3).await;
        refused.append(&mut r);
        for p in found {
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
            let (found, mut r) = ia_books(&bq, query, &card, &prog, 1).await;
            refused.append(&mut r);
            for p in found {
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
        legs.push(("books-refused".into(), refused.len()));
        plan_quarantine = refused;
    }
    // Tier 2 source legs: scholarly (OpenAlex, arXiv), experience (HN,
    // StackExchange, Lemmy), structured (DBpedia top-up inside translations
    // below). Synthetic Pages for substantive API content; discussion URLs
    // join the normal fetch pipeline (thin content dies at gates, not here).
    let mut arxiv_hints: std::collections::HashMap<String, String> = Default::default();
    {
        let mark = prefetched.len();
        let t0 = std::time::Instant::now();
        send("search: openalex…");
        for p in openalex_pages(query).await {
            if spam_url(&p.url) || !seen.insert(canon_url(&p.url)) {
                continue;
            }
            prefetched.push(p);
        }
        ledger_source("openalex", query, prefetched.len() - mark, t0.elapsed().as_millis() as u64);
        legs.push(("openalex".into(), prefetched.len() - mark));
        let t0 = std::time::Instant::now();
        send("search: arxiv…");
        let (apages, apdfs) = arxiv_pages(query).await;
        let mut an = 0usize;
        for p in apages {
            if spam_url(&p.url) || !seen.insert(canon_url(&p.url)) {
                continue;
            }
            prefetched.push(p);
            an += 1;
        }
        for (u, hint) in apdfs {
            if urls.len() >= max_pages + 8 || !seen.insert(canon_url(&u)) {
                continue;
            }
            arxiv_hints.insert(u.clone(), hint);
            urls.push(u);
        }
        ledger_source("arxiv", query, an, t0.elapsed().as_millis() as u64);
        legs.push(("arxiv".into(), an));
        let t0 = std::time::Instant::now();
        send("search: hn…");
        let mut hn = 0usize;
        for u in hn_urls(query).await {
            if urls.len() >= max_pages + 8 || spam_url(&u) || !seen.insert(canon_url(&u)) {
                continue;
            }
            urls.push(u);
            hn += 1;
        }
        ledger_source("hn", query, hn, t0.elapsed().as_millis() as u64);
        legs.push(("hn".into(), hn));
        let t0 = std::time::Instant::now();
        send("search: stackexchange…");
        let domain_se = subject_domain(query, &card.instance);
        let mut sn = 0usize;
        for u in se_urls(query, &domain_se).await {
            if urls.len() >= max_pages + 8 || spam_url(&u) || !seen.insert(canon_url(&u)) {
                continue;
            }
            urls.push(u);
            sn += 1;
        }
        ledger_source("stackexchange", query, sn, t0.elapsed().as_millis() as u64);
        legs.push(("stackexchange".into(), sn));
        send("search: lemmy…");
        let (lpage, lurls) = lemmy_discussion(query, &prog).await;
        let mut ln = 0usize;
        if let Some(p) = lpage {
            if !spam_url(&p.url) && seen.insert(canon_url(&p.url)) {
                prefetched.push(p);
                ln += 1;
            }
        }
        for u in lurls {
            if urls.len() >= max_pages + 8 || spam_url(&u) || !seen.insert(canon_url(&u)) {
                continue;
            }
            urls.push(u);
            ln += 1;
        }
        // (per-instance rows above are the ledger truth; no summary row —
        // a double ledger once counted lemmy twice per call)
        legs.push(("lemmy".into(), ln));
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
    // translations: the query in up to 30 tongues, each searching its own
    // Wikipedia (Orange -> naranja, orange, arancia…). Corroboration across
    // languages, plus non-English quotes for the manual.
    let mut trans_flat: Vec<(String, String, String)> = vec![];
    if urls.len() < max_pages + 8 {
        send("search: translations (30 langs)…");
        for (lang, label, turls) in translations(query, &prog).await {
            for u in turls {
                if spam_url(&u) || !seen.insert(canon_url(&u)) {
                    continue;
                }
                trans_flat.push((lang.clone(), label.clone(), u.clone()));
                urls.push(u);
                if urls.len() >= max_pages + 8 {
                    break;
                }
            }
            if urls.len() >= max_pages + 8 {
                break;
            }
        }
        // DBpedia top-up: structured labels for languages Wikidata missed
        // (deterministic, no scraping) + abstract page. Fills gaps, never
        // duplicates: have_langs skips what's already covered.
        if urls.len() < max_pages + 8 {
            let t0 = std::time::Instant::now();
            send("search: dbpedia…");
            let have: std::collections::HashSet<String> =
                trans_flat.iter().map(|(l, _, _)| l.clone()).collect();
            let (extra, abstract_page) = dbpedia_topup(query, &have).await;
            let mut dn = 0usize;
            for (lang, label, turls) in extra {
                for u in turls {
                    if spam_url(&u) || !seen.insert(canon_url(&u)) {
                        continue;
                    }
                    trans_flat.push((lang.clone(), label.clone(), u.clone()));
                    urls.push(u.clone());
                    dn += 1;
                    if urls.len() >= max_pages + 8 {
                        break;
                    }
                }
                if urls.len() >= max_pages + 8 {
                    break;
                }
            }
            if let Some(p) = abstract_page {
                if !spam_url(&p.url) && seen.insert(canon_url(&p.url)) {
                    prefetched.push(p);
                    dn += 1;
                }
            }
            ledger_source("dbpedia", query, dn, t0.elapsed().as_millis() as u64);
            legs.push(("dbpedia".into(), dn));
        }
        legs.push(("translations".into(), trans_flat.len()));
        crate::db::append(
            "translations",
            serde_json::json!({"query": query, "hits": trans_flat.iter().map(|(l, t, u)| serde_json::json!({"lang": l, "label": t, "url": u})).collect::<Vec<_>>()}),
        );
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
        // LLM-spawned queries join the same ring budgets and gates
        for q in expansion_queries.iter().take(8) {
            variants.push(q.clone());
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
    urls.truncate(max_pages + 8);
    CrawlPlan { urls, prefetched, legs, query_card: card, expansion: expansion_queries, translations: trans_flat, quarantine: plan_quarantine, title_hints: arxiv_hints }
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

pub async fn crawl_urls(urls: &[String], prog: Option<tokio::sync::mpsc::UnboundedSender<String>>, title_hints: &std::collections::HashMap<String, String>) -> (Vec<Page>, Vec<FetchRecord>) {
    use std::sync::Arc;
    use tokio::sync::Semaphore;
    // 3 concurrent fetches, 250ms launch pacing: fast but polite.
    // (Was serial 400ms; time no longer matters more than coverage,
    // but citizenship still does.)
    let total = urls.len();
    let hints = std::sync::Arc::new(title_hints.clone());
    let sem = Arc::new(Semaphore::new(3));
    let mut handles = vec![];
    for (i, u) in urls.iter().cloned().enumerate() {
        let prog = prog.clone();
        let sem = sem.clone();
        let hints = hints.clone();
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
                let rec = FetchRecord {
                    url: u.clone(),
                    host: host_of(&u),
                    outcome: "robots".into(),
                    status: None,
                    bytes: 0,
                    ms: 0,
                    attempts: 0,
                    note: "disallow".into(),
                };
                crate::db::append("fetch", serde_json::to_value(&rec).unwrap_or_default());
                return (None, Some(rec));
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            // document route: catalogs, datasheets, spreadsheets and book
            // files download through the doc pipeline (extracted, gated by
            // the same URL model-gate as pages — file stems carry query
            // tokens the same way; density judgment stays with the LLM
            // relevance pass downstream, exactly like web pages).
            if let Some(kind) = crate::docs::classify_url(&u) {
                match fetch_doc_measured(&u, kind, hints.get(&u).map(|s| s.as_str()).unwrap_or("")).await {
                    (Ok(p), rec) => {
                        if p.text.chars().count() < 200 {
                            if let Some(tx) = &prog {
                                let _ = tx.send(format!("skip {}/{} (thin doc)", i + 1, total));
                            }
                            let mut rec = rec;
                            rec.outcome = "thin".into();
                            crate::db::append("fetch", serde_json::to_value(&rec).unwrap_or_default());
                            return (None, Some(rec));
                        }
                        crate::db::append(
                            "crawl",
                            serde_json::json!({"url": p.url, "title": p.title, "chars": p.chars, "text": p.text}),
                        );
                        crate::db::append("fetch", serde_json::to_value(&rec).unwrap_or_default());
                        if let Some(tx) = &prog {
                            let _ = tx.send(format!("got {}/{} {}c doc", i + 1, total, p.chars));
                        }
                        (Some((i, p)), Some(rec))
                    }
                    (Err(_), rec) => {
                        crate::db::append("fetch", serde_json::to_value(&rec).unwrap_or_default());
                        if let Some(tx) = &prog {
                            let _ = tx.send(format!("fail {}/{} doc ({} {})", i + 1, total, rec.status.map(|s| s.to_string()).unwrap_or_else(|| "net".into()), rec.note.chars().take(30).collect::<String>()));
                        }
                        (None, Some(rec))
                    }
                }
            } else {
                match fetch_measured(&u).await {
                (Ok(p), rec) => {
                    if p.text.chars().count() < 200 {
                        if let Some(tx) = &prog {
                            let _ = tx.send(format!("skip {}/{} (thin)", i + 1, total));
                        }
                        let mut rec = rec;
                        rec.outcome = "thin".into();
                        crate::db::append("fetch", serde_json::to_value(&rec).unwrap_or_default());
                        return (None, Some(rec));
                    }
                    // raw reserve: extracted text persisted for later
                    // rinse/filter passes (TBs spare, keep everything)
                    crate::db::append(
                        "crawl",
                        serde_json::json!({"url": p.url, "title": p.title, "chars": p.chars, "text": p.text}),
                    );
                    crate::db::append("fetch", serde_json::to_value(&rec).unwrap_or_default());
                    if let Some(tx) = &prog {
                        let _ = tx.send(format!("got {}/{} {}c", i + 1, total, p.chars));
                    }
                    (Some((i, p)), Some(rec))
                }
                (Err(_), rec) => {
                    // extensionless documents (arxiv /pdf/ links carry no
                    // suffix): the non-html note names the content-type —
                    // reroute into the doc pipeline instead of failing.
                    // classify_ctype decides; images and unknowns stay failed.
                    if rec.outcome == "non-html" {
                        if let Some(kind) = crate::docs::classify_ctype(&rec.note) {
                            match fetch_doc_measured(&u, kind, hints.get(&u).map(|s| s.as_str()).unwrap_or("")).await {
                                (Ok(p), rec) => {
                                    if p.text.chars().count() < 200 {
                                        let mut rec = rec;
                                        rec.outcome = "thin".into();
                                        crate::db::append("fetch", serde_json::to_value(&rec).unwrap_or_default());
                                        return (None, Some(rec));
                                    }
                                    crate::db::append(
                                        "crawl",
                                        serde_json::json!({"url": p.url, "title": p.title, "chars": p.chars, "text": p.text}),
                                    );
                                    crate::db::append("fetch", serde_json::to_value(&rec).unwrap_or_default());
                                    if let Some(tx) = &prog {
                                        let _ = tx.send(format!("got {}/{} {}c doc", i + 1, total, p.chars));
                                    }
                                    return (Some((i, p)), Some(rec));
                                }
                                (Err(_), rec) => {
                                    crate::db::append("fetch", serde_json::to_value(&rec).unwrap_or_default());
                                    return (None, Some(rec));
                                }
                            }
                        }
                    }
                    crate::db::append("fetch", serde_json::to_value(&rec).unwrap_or_default());
                    if let Some(tx) = &prog {
                        let _ = tx.send(format!("fail {}/{} ({} {})", i + 1, total, rec.status.map(|s| s.to_string()).unwrap_or_else(|| "net".into()), rec.note.chars().take(30).collect::<String>()));
                    }
                    (None, Some(rec))
                }
                }
            }
        }));
    }
    let mut out: Vec<(usize, Page)> = vec![];
    let mut recs: Vec<FetchRecord> = vec![];
    for h in handles {
        if let Ok((page, rec)) = h.await {
            if let Some(r) = rec {
                recs.push(r);
            }
            if let Some(p) = page {
                out.push(p);
            }
        }
    }
    // restore plan order (progress may arrive out of order; order matters
    // for dup_of indices and ring-budget expectations)
    out.sort_by_key(|(i, _)| *i);
    recs.sort_by(|a, b| a.url.cmp(&b.url));
    (out.into_iter().map(|(_, p)| p).collect(), recs)
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
    // boost tokens from the QUERY (subject-agnostic): the old hardcoded car
    // list boosted opel/corsa on hermeticism runs. Generic doc-ish words stay.
    let qtokens: Vec<String> = query
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| t.len() > 2)
        .map(|t| t.to_string())
        .collect();
    let generic = ["spec", "manual", "guide", "review", "problem", "recall", "datasheet", "catalog", "handbook", "treatise"];
    let boosts: Vec<String> = qtokens.into_iter().chain(generic.iter().map(|s| s.to_string())).collect();
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
            for tok in boosts.iter() {
                if lu.contains(&tok[..]) {
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
    // catalogs link hundreds of PDFs: cap doc-classified links so one
    // index page can't flood the run (each still capped at 20MB downstream)
    let mut docs = 0usize;
    let mut out = vec![];
    for (_, u) in scored {
        if crate::docs::classify_url(&u).is_some() {
            if docs >= 8 {
                continue;
            }
            docs += 1;
        }
        out.push(u);
        if out.len() >= max_extra {
            break;
        }
    }
    out
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

#[cfg(test)]
mod tests {
    use super::{book_about_subject, subject_lexicon, QueryCard};

    fn herm_card() -> QueryCard {
        QueryCard {
            subject: "Hermeticism".into(),
            meaning: "philosophical and religious tradition based primarily upon writings attributed to Hermes Trismegistus".into(),
            aliases: vec!["Hermetism".into(), "hermetism".into()],
            instance: String::new(),
        }
    }

    #[test]
    fn dugin_rides_history_no_more() {
        // the Politica Aeterna hole: title + front matter name everything
        // except the subject; "history" alone must not admit it
        let lex = subject_lexicon("hermeticism", &herm_card());
        // stemmer strips -icism first: "hermeticism" -> "hermet", which
        // still matches "hermetic"/"hermeticism" as a substring (right
        // direction: short lex token inside longer text words)
        assert!(lex.iter().any(|t| t == "hermet"), "lexicon lost the stem: {lex:?}");
        // sentence-initial stopwords and pronunciation guides are not names
        assert!(!lex.iter().any(|t| t == "from"), "stopword leaked: {lex:?}");
        assert!(!lex.iter().any(|t| t == "ipa"), "abbreviation leaked: {lex:?}");
        let head = "politica aeterna alexandr dugin history of political philosophy ".to_string();
        let text = "history ".repeat(50) + &"hermeticism ".repeat(2);
        assert!(
            !book_about_subject("Politica Aeterna (Alexandr Dugin) — Internet Archive", &head, &text.to_lowercase(), &lex),
            "off-subject volume admitted"
        );
    }

    #[test]
    fn canon_passes_intake() {
        // central figures count: a chunk naming Hermes but never the
        // -ism still belongs to the subject
        let lex = subject_lexicon("hermeticism", &herm_card());
        assert!(lex.iter().any(|t| t == "hermes"), "lexicon lost the figure: {lex:?}");        let lex = subject_lexicon("hermeticism", &herm_card());
        let head = "the corpus hermeticum attributed to hermes trismegistus ".to_string();
        let text = "hermeticism ".repeat(30) + &"hermes trismegistus ".repeat(20);
        assert!(
            book_about_subject("Corpus Hermeticum — Internet Archive", &head, &text.to_lowercase(), &lex),
            "canon volume rejected"
        );
    }
}

#[cfg(test)]
mod ledger_tests {
    use super::{summarize_fetches, FetchRecord};

    fn rec(host: &str, outcome: &str, ms: u64) -> FetchRecord {
        FetchRecord {
            url: format!("https://{host}/x"),
            host: host.into(),
            outcome: outcome.into(),
            status: Some(200),
            bytes: 100,
            ms,
            attempts: 1,
            note: String::new(),
        }
    }

    #[test]
    fn ledger_rolls_up_per_host() {
        let rs = vec![
            rec("a.example", "ok", 100),
            rec("a.example", "ok", 300),
            rec("a.example", "fail", 50),
            rec("b.example", "robots", 0),
        ];
        let s = summarize_fetches(&rs);
        assert_eq!(s.len(), 2);
        let a = s.iter().find(|f| f.host == "a.example").unwrap();
        assert_eq!((a.ok, a.fail, a.other), (2, 1, 0));
        assert_eq!(a.ms_med, 100);
        let b = s.iter().find(|f| f.host == "b.example").unwrap();
        assert_eq!((b.ok, b.fail, b.other), (0, 0, 1));
    }
}

#[cfg(test)]
mod marks_tests {
    use super::{discriminators, QueryCard};

    fn card(subject: &str, meaning: &str) -> QueryCard {
        QueryCard { subject: subject.into(), meaning: meaning.into(), aliases: vec![], instance: String::new() }
    }

    #[test]
    fn digits_survive_with_dots() {
        let m = discriminators("opel corsa 2005 1.3 cdti", &card("opel corsa 2005 1.3 cdti", "supermini car"));
        assert!(m.iter().any(|t| t == "2005"), "year kept: {m:?}");
        assert!(m.iter().any(|t| t == "1.3"), "dotted engine kept whole: {m:?}");
        assert!(!m.iter().any(|t| t == "1" || t == "3"), "no digit shards: {m:?}");
    }

    #[test]
    fn figures_come_from_the_card() {
        let m = discriminators(
            "hermeticism",
            &card("Hermeticism", "philosophical tradition based on writings attributed to Hermes Trismegistus"),
        );
        assert!(m.iter().any(|t| t == "hermes"), "figure kept: {m:?}");
        assert!(m.iter().any(|t| t == "trismegistus"), "figure kept: {m:?}");
    }
}

#[cfg(test)]
mod evidence_tests {
    use super::junk_evidence_url;

    #[test]
    fn issue_trackers_are_not_evidence() {
        assert!(junk_evidence_url("https://github.com/MarginaliaSearch/MarginaliaSearch/issues/305"));
        assert!(junk_evidence_url("https://github.com/a/b/pull/12"));
        assert!(junk_evidence_url("https://gitlab.com/a/b/-/issues/3"));
        assert!(junk_evidence_url("https://github.com/a/b/discussions/7"));
    }

    #[test]
    fn real_sources_survive() {
        assert!(!junk_evidence_url("https://en.wikipedia.org/wiki/Hermeticism"));
        assert!(!junk_evidence_url("https://github.com/ElegantVW/magpie"));
        assert!(!junk_evidence_url("https://archive.org/details/corpus-hermeticum-htp"));
        assert!(!junk_evidence_url("https://plato.stanford.edu/entries/hermetism/"));
    }
}

#[cfg(test)]
mod pool_tests {
    use super::{engine_sidelined, engine_stats};

    fn row(engine: &str, urls: u64, hours_ago: f64, now_secs: f64) -> String {
        let ts = chrono::DateTime::from_timestamp((now_secs - hours_ago * 3600.0) as i64, 0)
            .unwrap()
            .to_rfc3339();
        serde_json::json!({"engine": engine, "urls": urls, "_at": ts}).to_string()
    }
    // now as (epoch seconds, hours) — units kept honest
    fn now() -> (f64, f64) {
        let s = 1_700_000_000.0;
        (s, s / 3600.0)
    }

    #[test]
    fn fresh_fail_streak_sidelines() {
        let (now_s, now_h) = now();
        let lines = vec![
            row("ddg", 0, 0.1, now_s),
            row("ddg", 0, 0.5, now_s),
            row("ddg", 0, 1.0, now_s),
            row("seznam", 4, 0.2, now_s),
        ];
        let s = engine_stats(&lines, now_h);
        let (_, fails_d, age_d) = s["ddg"];
        assert_eq!(fails_d, 3);
        assert!(engine_sidelined(fails_d, age_d));
        let (rate_s, fails_s, _) = s["seznam"];
        assert!(rate_s > 0.0 && fails_s == 0);
        assert!(!engine_sidelined(fails_s, 0.2));
    }

    #[test]
    fn stale_failures_parole() {
        let (now_s, now_h) = now();
        let lines = vec![
            row("ddg", 0, 5.0, now_s),
            row("ddg", 0, 6.0, now_s),
            row("ddg", 0, 7.0, now_s),
        ];
        let s = engine_stats(&lines, now_h);
        let (_, fails, age) = s["ddg"];
        assert_eq!(fails, 3);
        assert!(age > 2.0);
        assert!(!engine_sidelined(fails, age), "old failures must not ban forever");
    }

    #[test]
    fn success_resets_the_streak() {
        let (now_s, now_h) = now();
        let lines = vec![
            row("ddg", 5, 0.1, now_s),
            row("ddg", 0, 0.5, now_s),
            row("ddg", 0, 1.0, now_s),
        ];
        let s = engine_stats(&lines, now_h);
        assert_eq!(s["ddg"].1, 0);
    }

    #[test]
    fn junk_lines_ignored() {
        let now = 1_000_000.0;
        let lines = vec!["not json".to_string(), row("ddg", 0, 0.1, now)];
        let s = engine_stats(&lines, now);
        assert_eq!(s["ddg"].1, 1);
    }
}

#[cfg(test)]
mod tier2_tests {
    use super::{se_sites, title_matches};

    #[test]
    fn title_gate_admits_titled_rejects_mention() {
        assert!(title_matches("hermeticism", "Giordano Bruno and the Hermetic Tradition"));
        assert!(title_matches("opel corsa 1.3 cdti", "Opel Corsa D 1.3 CDTI specs"));
        assert!(!title_matches("hermeticism", "A General History of Magic volume 2"));
        // short tokens never gate (avoid "of"/"the" matches)
        assert!(!title_matches("it", "Anything At All"));
    }

    #[test]
    fn se_sites_follow_domain_and_drink_words() {
        assert_eq!(se_sites("opel corsa", "vehicle"), vec!["mechanics"]);
        assert_eq!(se_sites("margarita cocktail", "field"), vec!["cooking", "alcohol"]);
        assert_eq!(se_sites("lisbon", "place"), vec!["travel"]);
        assert_eq!(se_sites("hermeticism", "field"), vec!["philosophy", "history"]);
    }
}

#[cfg(test)]
mod periodical_tests {
    use super::is_periodical;

    #[test]
    fn catches_the_container_that_sank_black_hole() {
        assert!(is_periodical("New Scientist 28 March 2015 — Internet Archive"));
        assert!(is_periodical("Nature Vol 512, No 3"));
        assert!(is_periodical("Journal of Cosmology, issue 45"));
        assert!(is_periodical("Scientific American December 1998"));
        assert!(is_periodical("AIAA Journal vol. 9 no. 4"));
        assert!(is_periodical("Der Spiegel 2021-03-28"));
        assert!(is_periodical("The Lancet, 1st May 1902"));
    }

    #[test]
    fn real_works_never_look_like_issues() {
        // the four APPROVED manuals' actual books must survive
        assert!(!is_periodical("Corpus Hermeticum — Internet Archive"));
        assert!(!is_periodical("The Cocktail Book — Internet Archive"));
        assert!(!is_periodical("Fatal Youth Of The Universe Black Hole Threat"));
        assert!(!is_periodical("Black Holes — Internet Archive"));
        assert!(!is_periodical("A History of Lisbon"));
        // "may" as a word, "vol" inside a word, bare years
        assert!(!is_periodical("You May Find It Interesting"));
        assert!(!is_periodical("Revolutions per volume of fluid"));
        assert!(!is_periodical("Gravity 2015 Retrospective"));
    }
}
