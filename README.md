![Magpie hero](assets/hero/magpie.svg)

# Magpie — thorough seeker

Rust engine for [faeOS](https://github.com/ElegantVW/faeOS). One binary: TUI + seek + PDF. Privacy-first: DDG + Marginalia, never Google. Thorough by default: many various sources, never wikipedia-only.

Status: v0.1 TUI prompt box + `seek` + local PDF (`seek.json` + `processed.json` + `manual.pdf`).

```
  /\
 (\/)
 <>>
/    \
\    /
```

## Use

```bash
magpie                         # TUI: prompt box, Enter seeks, returns PDF
magpie seek "opel corsa C 2005 1.3 cdti" --depth extreme --pages 12
magpie seek "query" --depth quick --pages 6 --out /tmp/out --no-pdf
magpie process /tmp/out        # pixie pass: one quick local AI task per entry, then structured PDF
magpie browse --dump https://example.com   # compat for pixie
```

TUI keys: type + `Enter` seek, `p` pixie pass on last result, `e` entry list, `Enter` opens entry, `Esc` cancel/quit.

## Thoroughness

* Chain: DDG HTML POST → old-search Marginalia → Wikipedia API → query variants → second-hop outbound links (query-token gated, dealer-lot blocklist)
* Gate: warns on `<4 pages`, `<3 hosts`, wiki-only. No PDF-only-from-wikipedia claim.

## Thoroughness

* Chain: DDG HTML POST → old-search Marginalia → Wikipedia API → query variants → second-hop outbound links
* Gate: warns on `<4 pages`, `<3 hosts`, wikipedia-only. No PDF-only-from-wikipedia claim.
* Cache: `~/.cache/magpie/seek/<slug>/seek.json`, `processed.json`, `manual.pdf`
* PDF is local minimal writer (Helvetica A4), no LLM, no cloud.

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
| `src/main.rs/cli.rs` | CLI: TUI + seek + browse --dump compat |
| `src/crawl.rs` | search chain + fetch + extract + second-hop |
| `src/seek.rs` | orchestrate + diversity gate + JSON |
| `src/pdf.rs` | minimal local PDF writer |
| `src/tui.rs` | crossterm prompt box (no ratatui) |
| `src/store.rs` | cache dir + slug |
| `scripts/magpie` | thin launcher, never ELF in git |

## License

MIT — see [LICENSE](LICENSE).
