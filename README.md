![Magpie hero](assets/hero/magpie.svg)

# Magpie — thorough seeker

Rust engine for [faeOS](https://github.com/ElegantVW/faeOS). One binary: TUI + seek + PDF. Privacy-first: DDG + Marginalia, never Google. Thorough by default: many various sources, never wikipedia-only.

Status: producer of cited PDF manuals — `seek` crawls, `process` files with local AI, `manual` runs end-to-end, `judge` fact-checks, `rinse` audits, `nose`/`granary` query the hoard. (`seek.json` + `processed.json` + `dump.pdf`/`manual.pdf` + `judgment.*` + `quarantine.json` + `drops.json` + `manifest.json` + `fetch.jsonl`).

For: researchers/students needing a thorough cited PDF. Not for: Google-speed answers, offline boxes with no network/models.

```
  /\
 (\/)
 <>>
/    \
\    /
```

## GPU

llama.cpp is built with Vulkan (`~/.local/src/llama.cpp`, headers in
`~/.local/vulkan-deps`). The server lives at
`~/.local/lib/llama-vulkan/` (`llama-server-vk` wrapper sets
`LD_LIBRARY_PATH`); the stock `~/bin/llama-server` is untouched.
The magpie slot runs `qwen3-4b` with GPU inference through menagerie
(`menagerie backend magpie gpu` → house Vulkan server + full offload;
`cpu` for the stock fallback). Backend is per-creature steward business —
magpie passes no GPU env of its own (an env override once masked a
CPU-started slot for a week because ensure is a no-op when healthy).
Per-entry latency follows server speed; budgets cover bad days (600s LLM
round-trips), and failures log to stderr, never silent.
`process` passes that env on every `ensure`, so the slot self-heals
GPU-backed across evictions. Opt out with `MAGPIE_GPU=0`.
Switch models with `menagerie set magpie <name>`; pick the app with
`MAGPIE_LLM_APP=<app>`.

## Use

```bash
magpie                         # TUI: prompt box, Enter seeks, returns dump.pdf
magpie seek "opel corsa C 2005 1.3 cdti" --depth extreme --pages 12
magpie seek "query" --depth quick --pages 6 --out /tmp/out --no-pdf
magpie process /tmp/out        # pixie pass: one quick local AI task per entry, then manual.pdf
magpie manual "query" --depth deep --pages 12   # end-to-end: dump + manual.pdf
magpie judge /tmp/out          # fact-check the manual: judgment.json/txt + appendix on APPROVED
magpie rinse [--subject S] [--dir /tmp/out] [--max-dist 3]  # audit runs: near-dups + quarantine
magpie nose --rebuild --subject hermeticism [--dir /tmp/out]  # press the BM25 nose from seek dirs
magpie nose "hermeticism" --subject hermeticism [--lang de]   # smell the hoard
magpie granary --rebuild [--dir /tmp/out]       # press runs into the Parquet granary
magpie granary --sql "SELECT subject, COUNT(*) FROM pages GROUP BY subject"  # rinse
magpie browse --dump https://example.com   # compat for pixie
```

Features: default builds the Xapian nose (needs `xapian-core` dev files).
`--no-default-features` builds pure Rust. The granary (DataFusion+Parquet,
300 crates) is opt-in: `--features granary`.

TUI keys: type + `Enter` seek, `Ctrl-P` pixie pass, `Ctrl-J` judge, `Ctrl-E` entry list (`[kind|via]`), `Ctrl-O` opens the PDF, `Esc` cancel/quit. Plain letters are text now — the old bare `p`/`e` hijacked the first typed character.
`seek` writes `dump.pdf` (RAW DUMP, never a manual); only `process`/`manual` write `manual.pdf`.
The judge never renders a rejection: REDO leaves the PDF alone and reports reasons as text (`judgment.txt`), so a bad manual costs no local inference to read. Mostly-unscored manuals REDO without spending searches (fact-checking fallback bullets wastes inference).

## Producer passes

Three AI passes make magpie a producer, not a crawler:

1. **Query expansion** (plan time) — one local call reads the raw query + query card (meaning, aliases, Wiktionary senses) and spawns 6–10 web queries: subtopics, key figures, technique queries. Persisted as `expansion` in `seek.json`, joined into the same ring budgets as alias/chapter queries.
2. **30-language fan-out** — Wikidata labels for the query entity, one opensearch per language Wikipedia (Orange → naranja, arancia, 橘子…). Corroboration across tongues; per-language hits land in `translations` and in `translations.jsonl`.
3. **Judge** (post-crawl) — extracts the top 10 claims (specs/faults first, deduped), runs one verification search per claim through magpie's own engines, fetches evidence snippets, then deliberates. `APPROVED` appends a judge's appendix page to `manual.pdf`; `REDO` leaves the PDF untouched.

## Programs database

Append-only JSONL reserve under `~/.local/share/magpie/db/` (`MAGPIE_DB` overrides the parent): `dict.jsonl` (Wiktionary extracts), `thesaurus.jsonl` (entity + aliases per query), `translations.jsonl` (per-language hits), `expansions.jsonl` (spawned queries), `crawl.jsonl` (every crawled page's extracted text, keyed by URL). Raw-first, files-first: the house hoards now and rinses later.

## Document retrieval

When the crawl meets a catalog, datasheet, spreadsheet or book file, it
downloads and extracts it instead of ingesting mojibake. Pure-Rust parsers
only (`pdf-extract`, `zip`, `calamine`) — no system deps, no daemons, no
executables ever touched. Archives (zip/rar/tar) are refused outright.

Budgets: 20MB download ceiling (content-length gate + stream abort),
400K-char extraction cap (uniform with book chunks), ≤8 doc links per
second-hop (catalogs with 200 PDFs exist). IA books fall back
`_djvu.txt` → EPUB → PDF (many items lack djvu.txt). A doc URL answering
HTML (viewer/login wall) degrades to a page fetch instead of failing.
Docs ride the same URL gates as pages; books keep the whole-work intake
gate; the LLM relevance pass judges both.

## The rinse (audit, no LLM)

`magpie rinse` re-examines finished runs deterministically: trigram-simhash
near-dups across runs (shared jargon no longer counts — phrases do),
same-URL re-crawls with seam-stability distances (dist 0 = FastCDC seams
held), and the quarantine audit (refused somewhere but kept elsewhere
flags gate instability; sibling-chunk granularity does not flag).
Reason histograms show what the gates refuse and why. Text verdicts only.

## Compartments: quarantine + manifests

Refusals are data, not silence. Every refused work lands in `quarantine.json` per run *and* `db/<subject>/quarantine.jsonl` with its reason (`off-subject`, `no-fulltext`, `restricted:403`, `thin`, relevance score, `REDO: …`). Dugin sits there with cause attached — the rinse pass audits refusals instead of trusting them. Per-chunk drops live in `drops.json` (title, url, `relevance:N` or `namesake`).

Every run carries `manifest.json`, merged stage by stage (seek → process → judge, possibly separate invocations — merge never overwrites): each stage pins its artifacts by SHA256, so any claim traces claim → entry → page → fetch → run → verdict. A re-rendered PDF will not match its hash — honestly.

## The granary (DataFusion + Parquet)

The loop's food store: every finished run pressed into columnar tables
(`pages`, `fetch`, `quarantine`, `entries`, `verdicts`, `runs`), each
hive-partitioned by subject under `db/granary/` so compartments prune
for free. The amber stays source of truth; the granary is derived and
re-pressable. `press_run` is the function the future retrieval loop
calls per cycle: search ends → press → one word out → new search begins.

Rinse examples (the loop auditing itself):

```sql
-- refusals by reason, per subject
SELECT subject, reason, COUNT(*) FROM quarantine GROUP BY subject, reason ORDER BY subject;
-- where the fetches die
SELECT host, outcome, COUNT(*), AVG(ms) FROM fetch GROUP BY host, outcome ORDER BY host;
-- score distribution of what reached manuals
SELECT section, AVG(relevance), COUNT(*) FROM entries GROUP BY section;
```

## The nose (Xapian)

One BM25 index per subject under `db/<subject>/nose/`, pressed from seek dirs (`fetch.jsonl`-era runs and older alike). Per-doc language from the translation fan-out (stemmers are per-language and never meet — German `Hermetik` is indexed German); aliases + translation labels become synonym pairs both directions, so cross-lingual recall rides on synonyms, not stemming luck. Spelling suggestions compose with synonyms: typo → alias → canon.

Division of labor, stated plainly: the nose ranks lexical fit (a poetry movement named Hermeticism can top the list — it shares the words); meaning stays with the LLM relevance gate and the judge. The nose buries pretenders by density (Dugin: zero hits in the top 30); the gate and judge evict namesakes.

The C++ seam is owned, not depended on: `src/nose_shim.{h,cc}` (~200 lines, exceptions never cross) after all three crates.io bindings proved dead. Needs `xapian-core` dev files (`build.sh` says so plainly); bare boxes build pure Rust with `--no-default-features`.

## Thoroughness

* Chain: measured engine pool (DDG + Marginalia + Seznam, yields ledgered, fail streaks sidelined) → Wikipedia family (en/de/pl/it) → `site:` verticals → sitemaps → Common Crawl WARC → Wikidata structured facts → IA books (whole-work intake gate, djvu→EPUB→PDF) → documents (PDF/EPUB/DOCX/XLSX/CSV, capped, bomb-guarded) → 30-language fan-out → scored second-hop
* Every fetch measured into `fetch.jsonl` (outcome, status, bytes, ms, attempts): throttles retried once, failures kept as data; per-host roll-up in `seek.log`
* Repetition is signal: near-dups kept as `dup_of` corroboration (`claimed_by ×N`), skipped by the LLM pass to save compute; locale mirrors collapse at URL level; long texts split by FastCDC content-defined seams (stable across re-crawls, verbatim-safe)
* Gate: warns on `<4 pages`, `<3 hosts`, wiki-only, missing specs/faults. No PDF-only-from-wikipedia claim.
* Pixie pass via menagerie (`llm:magpie` slot, GPU-backed): one `/v1/chat/completions` JSON-enforced task per keeper entry at temperature 0 (seed lottery once wiped a manual), each tagged `via: llm:*|heuristic:*` with its score stored in entries, entries view, cover and sources. Distinguishing marks (years, engine codes, proper names) ride the prompt: same-words-different-subject scores below 40; unscored answers default to 0 (drop), never 50.
* Cache: `~/.cache/magpie/seek/<slug>/seek.json`, `processed.json`, `dump.pdf`/`manual.pdf`, `judgment.json`/`judgment.txt`, `quarantine.json`, `drops.json`, `manifest.json`, `fetch.jsonl`
* PDF is local minimal writer (Helvetica A4), no LLM in layout, no cloud.

## Install

```bash
git clone git@github.com:ElegantVW/magpie.git ~/magpie
cd ~/magpie && ./build.sh install
```

`build.sh install` writes ELF to `~/.local/lib/faeos/magpie` and launcher to `~/bin/magpie`. Override with `MAGPIE_BIN`.

Python legacy retired to `~/bin/magpie-py-legacy` + `magpie_*.py-legacy`. See CHANGELOG.

## Layout

| Path | Role |
|------|------|
| `src/main.rs/cli.rs` | CLI: TUI + seek/process/manual/judge/rinse + nose/granary + browse --dump compat |
| `src/llm.rs` | shared local-LLM plumbing: endpoint, JSON enforcement, truncation salvage, stderr failures |
| `src/nose.rs` + `src/nose_shim.{h,cc}` | Xapian nose: per-subject BM25 index, synonyms, spelling (feature `nose`) |
| `src/rinse.rs` | deterministic audit: trigram-simhash near-dups, quarantine stability |
| `src/granary.rs` | DataFusion+Parquet lakehouse: press runs, rinse with SQL (feature `granary`) |
| `src/judge.rs` | fact-check pass: claims → verification searches → verdict + appendix; unscored gate |
| `src/db.rs` | programs database: JSONL amber + subject partitions + SHA manifests |
| `src/docs.rs` | document retrieval: classify/download/extract PDF/EPUB/DOCX/XLSX/CSV (pure Rust, capped) |
| `src/crawl.rs` | search chain + measured engine pool + measured fetch + intake gate + lexicon/marks + 30-language fan-out |
| `src/seek.rs` | orchestrate + FastCDC chunking + dupes + diversity gate + dump.pdf |
| `src/process.rs` | per-entry pixie pass (temp 0, stored scores) + heuristic warnings + manual.pdf |
| `src/pdf.rs` | minimal local PDF writer: dump + structured manual |
| `src/tui.rs` | crossterm prompt box (no ratatui) |
| `src/store.rs` | cache dir + slug |
| `scripts/magpie` | thin launcher, never ELF in git |

## License

MIT — see [LICENSE](LICENSE).
