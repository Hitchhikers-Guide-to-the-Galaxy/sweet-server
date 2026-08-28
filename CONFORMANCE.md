# Conformance watermark

Target: fedwiki-server (harness: ~/Code/conformance, ratchet:
`swamp workflow run conformance-run --input target_url=http://localhost:4251`)

| Level | Tier          | Status | Date       |
|-------|---------------|--------|------------|
| L0    | t0-minimal    | PASS   | 2026-08-28 |
| L0    | t1-core-items | PASS   | 2026-08-28 |
| L0    | t2-journal    | PASS   | 2026-08-28 |

L1 partial (2026-08-28, later): native-hosted client — HTML shell,
lineup URLs, client/plugin bundle statics, factories.json, plugins.json —
verified in the browser (Server Lineup Test, both variants). Remaining for
L1: site-index generation (MiniSearch-compatible), /system/export.json,
/remote/:site/:slug.json, server-rendered story in /:slug.html.

Run the candidate for the suite:

    cargo build
    target/debug/sweet-server --data <copy-of-fixture-site> --port 4251 \
      --defaults <wiki-server>/default-data
