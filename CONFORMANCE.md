# Conformance watermark

Target: fedwiki-server (harness: ~/Code/conformance, ratchet:
`swamp workflow run conformance-run --input target_url=http://localhost:4251`)

| Level | Tier          | Status | Date       |
|-------|---------------|--------|------------|
| L0    | t0-minimal    | PASS   | 2026-08-28 |
| L0    | t1-core-items | PASS   | 2026-08-28 |
| L0    | t2-journal    | PASS   | 2026-08-28 |

Next rung: L1 full read — HTML shell (`/:slug.html`), lineup URLs, client and
plugin bundles, `/system/{export,factories,plugins}.json`, site-index
*generation* (MiniSearch-compatible), `/remote/:site/:slug.json`.

Run the candidate for the suite:

    cargo build
    target/debug/sweet-server --data <copy-of-fixture-site> --port 4251 \
      --defaults <wiki-server>/default-data
