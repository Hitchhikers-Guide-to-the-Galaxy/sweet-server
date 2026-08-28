# Conformance watermark

Target: fedwiki-server (harness: ~/Code/conformance, ratchet:
`swamp workflow run conformance-run --input target_url=http://localhost:4251`)

| Level | Tier          | Status | Date       |
|-------|---------------|--------|------------|
| L0    | t0-minimal    | PASS   | 2026-08-28 |
| L0    | t1-core-items | PASS   | 2026-08-28 |
| L0    | t2-journal    | PASS   | 2026-08-28 |
| L1    | t0-minimal    | PASS   | 2026-08-28 |
| L1    | t1-core-items | PASS   | 2026-08-28 |
| L1    | t2-journal    | PASS   | 2026-08-28 |
| L2    | t0-minimal    | PASS   | 2026-08-28 |
| L2    | t1-core-items | PASS   | 2026-08-28 |
| L2    | t2-journal    | PASS   | 2026-08-28 |

L1 COMPLETE (2026-08-28): native-hosted client, site-index generation
(MiniSearch serializationVersion 2 — validated by node loadJSON + live
client search), /system/export.json, /remote/:site/:slug.json, security
client statics, Last-Modified on sitemap/site-index (load-bearing for the
client's neighborhood — found by the Server Lineup Test). Known divergence:
/:slug.html serves the shell without server-rendered story (0.27.0's own
story rendering is a spec BUG). L2 COMPLETE (2026-08-28): six-verb action grammar (form-encoded action-as-JSON-string body), implicit fork with journal prepend, fork push/pull with recycler save, DELETE + recycler round-trip, copy-on-write default pages, exact error bodies. Edit Round-Trip demo passed live through the native client. Shell fix: isOwner=true under legacy security (else the client edits localStorage, not the server — pinned by test). Next rung: L3 security (friends claim/login).

Run the candidate for the suite:

    cargo build
    target/debug/sweet-server --data <copy-of-fixture-site> --port 4251 \
      --defaults <wiki-server>/default-data
