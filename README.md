![Magpie hero](assets/hero/magpie.svg)

# Magpie — thorough seeker

Rust engine for [faeOS](https://github.com/ElegantVW/faeOS). One binary: TUI + seek + PDF. Privacy-first: DDG + Marginalia, never Google. Thorough by default: many various sources, never wikipedia-only.

Status: TUI prompt box + `seek`/`process`/`manual` + local PDF (`seek.json` + `processed.json` + `dump.pdf`/`manual.pdf`).

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
The magpie slot runs `qwen3-4b` on the RX 5700 XT
(`PIXIE_LLAMA_SERVER=…/llama-server-vk PIXIE_LLM_NGL=99 menagerie set magpie …`):
~150 tok/s prompt, ~120 tok/s gen — per-entry ~2-10s.
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
magpie browse --dump https://example.com   # compat for pixie
```

TUI keys: type + `Enter` seek, `p` pixie pass on last result, `e` entry list (`[kind|via]`), `Enter` opens entry, `Ctrl-O` opens the PDF, `Esc` cancel/quit.
`seek` writes `dump.pdf` (RAW DUMP, never a manual); only `process`/`manual` write `manual.pdf`.

## Thoroughness

* Chain: DDG HTML POST → old-search Marginalia → Wikipedia family (en/de/pl/it) → `site:` verticals → sitemaps → Common Crawl WARC → Wikidata structured facts
* Bulk without search engines: sitemap enumeration + CC index→WARC range fetch (gunzip locally) + Wikidata claims API, all keyless
* Repetition is signal: near-dups kept as `dup_of` corroboration (`claimed_by ×N`), skipped by the LLM pass to save compute; locale mirrors collapse at URL level
* Gate: warns on `<4 pages`, `<3 hosts`, wiki-only, missing specs/faults. No PDF-only-from-wikipedia claim.
* Pixie pass via menagerie (`llm:magpie` slot, GPU-backed): one `/v1/chat/completions` JSON-enforced task per keeper entry, each tagged `via: llm:*|heuristic:*` in entries, entries view, cover and sources.
* Cache: `~/.cache/magpie/seek/<slug>/seek.json`, `processed.json`, `dump.pdf`/`manual.pdf`
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
| `src/main.rs/cli.rs` | CLI: TUI + seek/process/manual + browse --dump compat |
| `src/crawl.rs` | search chain + fetch + extract + scored second-hop |
| `src/seek.rs` | orchestrate + dedupe + diversity gate + dump.pdf |
| `src/process.rs` | per-entry pixie pass + `via` tagging + manual.pdf |
| `src/pdf.rs` | minimal local PDF writer: dump + structured manual |
| `src/tui.rs` | crossterm prompt box (no ratatui) |
| `src/store.rs` | cache dir + slug |
| `scripts/magpie` | thin launcher, never ELF in git |

## License

MIT — see [LICENSE](LICENSE).
