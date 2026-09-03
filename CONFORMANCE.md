# Conformance watermark

Target: fedwiki-server (harness: ~/Code/conformance, ratchet:
`swamp workflow run conformance-run --input target_url=http://localhost:4251`)

Levels L0-L4 are run once per fixture tier; L5-L7 boot their own servers
(single-site, farm, mock-OIDC and plugin-declaring farms) and are
tier-agnostic. Counts are candidate-side passes.

| Level | Tier          | Tests      | Status | Date       |
|-------|---------------|------------|--------|------------|
| L0    | t0-minimal    | 11         | PASS   | 2026-09-01 |
| L0    | t1-core-items | 11         | PASS   | 2026-09-01 |
| L0    | t2-journal    | 11         | PASS   | 2026-09-01 |
| L1    | t0-minimal    | 16         | PASS   | 2026-09-01 |
| L1    | t1-core-items | 16         | PASS   | 2026-09-01 |
| L1    | t2-journal    | 16         | PASS   | 2026-09-01 |
| L2    | t0-minimal    | 20 (+1 s)  | PASS   | 2026-09-01 |
| L2    | t1-core-items | 20 (+1 s)  | PASS   | 2026-09-01 |
| L2    | t2-journal    | 20 (+1 s)  | PASS   | 2026-09-01 |
| L3    | t0-minimal    | 14         | PASS   | 2026-09-01 |
| L3    | t1-core-items | 14         | PASS   | 2026-09-01 |
| L3    | t2-journal    | 14         | PASS   | 2026-09-01 |
| L4    | t0-minimal    | 11         | PASS   | 2026-09-01 |
| L4    | t1-core-items | 11         | PASS   | 2026-09-01 |
| L4    | t2-journal    | 11         | PASS   | 2026-09-01 |
| L5    | self-booted   | 9 (+1 x)   | PASS   | 2026-09-01 |
| L6    | self-booted   | 19 (+1 x)  | PASS   | 2026-09-01 |
| L7    | self-booted   | 105 (+2 s) | PASS   | 2026-09-01 |

`s` = skipped, `x` = xfail on the REFERENCE (a registered spec BUG the
suite refuses to copy — see BUGS.toml). 210 tests collected in total.

## What each level pinned

**L1 COMPLETE (2026-08-28)**: native-hosted client, site-index generation
(MiniSearch serializationVersion 2 — validated by node loadJSON + live
client search), /system/export.json, /remote/:site/:slug.json, security
client statics, Last-Modified on sitemap/site-index (load-bearing for the
client's neighborhood — found by the Server Lineup Test). Known
divergence: /:slug.html serves the shell without server-rendered story
(0.27.0's own story rendering is a spec BUG).

**L2 COMPLETE (2026-08-28)**: six-verb action grammar (form-encoded
action-as-JSON-string body), implicit fork with journal prepend, fork
push/pull with recycler save, DELETE + recycler round-trip, copy-on-write
default pages, exact error bodies. Edit Round-Trip demo passed live
through the native client. Shell fix: isOwner=true under legacy security
(else the client edits localStorage, not the server — pinned by test).
The one skip is harness shape, not a gap: the stale-index test plants
files before boot, so it needs `--candidate-bin` and skips under
`--target-url`.

**L3 COMPLETE**: the friends claim/login flow (spec §6.3) as a strictly
ordered lifecycle — unclaimed, claim, authorized write, logout, reclaim
— with the seedrandom nickname port and the owner secret read from
status/owner.json the way a real owner knows it.

**L4 COMPLETE**: plugin server routes — the assets door (upload, list,
recursive index, session-gated delete) and plugmatic, both answered
natively.

**L5 COMPLETE**: farm mode — per-host multiplexing, the dir-with-pages
admission rule, exact refusal bodies for unknown and missing hosts, and
the OAuth2 farm write lifecycle (anonymous refused, owner write
persisted, farm-wide identity versus per-site authorization, sessions
surviving a restart). Carries the `farm-autovivify` xfail: the reference
creates empty wiki folders on an unknown-host GET, and sweet refuses
instead.

**L6 COMPLETE**: OIDC security against wiki-security-social as oracle —
discovery, PKCE, nonce, id-token verification against published keys,
the login dialog postMessage contract, claim-from-bare-pages, and
sign-out origin gating. Carries the `social-fat-token-431` xfail: real
Keycloak tokens in cookies overflow node's 16KB header cap, while
sweet's stateless signed session stays small.

**L7 COMPLETE**: the fedwiki.api door against wiki-plugin-farm as oracle
— merged /system/api.json with farm block and $ref namespacing, the MCP
endpoint, the try door, the key→place→person gate, and the honesty rules
for unserved plugin operations. Two shortlink probes skip when something
already listens on 127.0.0.1:80 (the port-80 wall, dossier §4.4.14).

## Running the candidate

Build first: `cargo build --release` (the sweep below used
`target/release/sweet-server`; a debug build works identically, slower).

All commands below run from the harness dir,
`~/Code/conformance/targets/fedwiki-server`, where `vendor` is
`vendor/node_modules` and the tiers live in `fixtures/sites/`.

L0-L2 — boot the candidate against a COPY of a fixture tier, then aim the
suite at it:

    target/release/sweet-server --data <copy-of-fixture-site> --port 4251 \
      --defaults vendor/node_modules/wiki-server/default-data \
      --vendor vendor/node_modules --security_legacy

    python3 -m pytest -q tests -m L0 --tier=t0-minimal \
      --target-url=http://127.0.0.1:4251

L3/L4 — the same, but boot with `--security_type friends` against a FRESH
(unclaimed) copy, and pass the data dir so the claim fixture can read the
owner secret:

    python3 -m pytest -q tests -m L3 --tier=t0-minimal \
      --target-url=http://127.0.0.1:4251 --target-data=<copy>

L5-L7 — the suite boots its own candidates; give it the binary:

    python3 -m pytest -q tests -m L7 \
      --candidate-bin=target/release/sweet-server

Note `--target-url` and `--candidate-bin` are different modes: a few
tests need to plant state before boot and so only run under
`--candidate-bin`.

## Deliberate divergence — asset Content-Type (2026-09-03)

Text-shaped assets (`.md .markdown .toml .txt .csv .tsv .log .yaml .yml .ini
.cfg .py .rs .sh .coffee .ts`) are served as `text/plain; charset=utf-8` so a
browser displays them. The node reference (express.static + mime-types) sends
`text/markdown`, `application/toml`, `application/octet-stream` (.py) and a
mime-db collision for `.rs` (`application/rls-services+xml`), all of which
browsers download. No suite test asserts asset Content-Type today; if the
differential runner ever byte-diffs headers this is a waiver, and the open
question is whether to record it as a `BUGS.toml` verdict (`asset-text-mime`,
L0, strict xfail) — David's call, tracked on the File Plugin Plan
(https://sweet.fedwiki.club/view/file-plugin-plan).
