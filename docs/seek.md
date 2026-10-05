# seek — thorough pipeline

`magpie seek "q"` does:

1. `plan_urls`: DDG HTML POST (q, kl=us-en) + old-search Marginalia + Wikipedia API + variants
2. `crawl_urls`: 400ms politeness, 12s timeout, skip <500 chars
3. `expand_second_hop`: harvest outbound hrefs, score by opel/corsa/cdti/multijet/jtd/diesel/fiat/vauxhall/auto/motor/car, skip wikimedia/creativecommons/binaries/interlanguage noise
4. Gate: warns if few pages / low hosts / wiki-only
5. Write `seek.json` + `processed.json` (pixie-ready) + `manual.pdf` (minimal writer)

Data file `seek.json` is what pixie formats; PDF step needs no LLM, saving compute/tokens.
