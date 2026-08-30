#!/usr/bin/env python3
"""sweet-toggle-api — the on-host door for the Sweet Farm Toggle.

Serves (bound 127.0.0.1, fronted by Caddy as toggle.<host>.private.fish,
tailnet-only):
  GET  /                   the selector UI (used by the Sweet Toggle Demo
                           wiki page; queries this host same-origin and the
                           sibling host over CORS)
  GET  /api/status         door groups: doors, routed target, live probe
  POST /api/toggle         {"site": <entry>, "to": "sweet"|"node"}

Sibling of ~/bin/sweet-switch on the laptop — same snippet, same rules
(door groups flip together, preflight before routing, ETag = node probe),
but local to the host so a wiki page can drive it. Toggling is routing
only; a site on sweet is READ-ONLY until toggled back.
"""

import argparse
import datetime
import json
import re
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

CFG = None  # argparse namespace


def utcnow():
    return datetime.datetime.now(datetime.timezone.utc).strftime(
        "%Y-%m-%dT%H:%M:%SZ")


def sh(cmd, stdin=None, timeout=20):
    r = subprocess.run(["/bin/sh", "-c", cmd], input=stdin,
                       capture_output=True, text=True, timeout=timeout)
    return r.returncode, r.stdout, r.stderr


# ---------------------------------------------------------------- model

def door_groups():
    groups = {}
    root = Path(CFG.farm)
    for e in sorted(root.iterdir()):
        if e.is_dir() and (e / "pages").is_dir():
            groups.setdefault(str(e.resolve()), []).append(e.name)
    return {r: sorted(ds) for r, ds in groups.items()}


def read_snippet():
    try:
        text = Path(CFG.snippet).read_text()
    except OSError:
        return []
    for line in text.splitlines():
        if line.startswith("@sweetlive host "):
            return line[len("@sweetlive host "):].split()
    return []


def write_snippet(doors):
    head = f"# managed by sweet-toggle-api — do not hand-edit. {utcnow()}\n"
    body = head
    if doors:
        body += ("@sweetlive host " + " ".join(sorted(doors)) + "\n"
                 + "handle @sweetlive {\n\treverse_proxy localhost:"
                 + str(CFG.sweet_port) + "\n}\n")
    prev, _ = (Path(CFG.snippet).read_text(), None) if Path(CFG.snippet).exists() else ("", None)
    code, _, err = sh(f"{CFG.sudo}tee {CFG.snippet} > /dev/null", stdin=body)
    if code != 0:
        raise RuntimeError(f"snippet write failed: {err.strip()}")
    code, _, err = sh(CFG.reload_cmd, timeout=30)
    if code != 0:
        sh(f"{CFG.sudo}tee {CFG.snippet} > /dev/null", stdin=prev)
        sh(CFG.reload_cmd, timeout=30)
        raise RuntimeError(f"caddy reload rejected the snippet (restored): "
                           f"{err.strip()[:300]}")


def which_server(door):
    if door.endswith(".local"):
        cmd = (f'curl -skI --max-time 6 -H "Host: {door}" '
               f'http://127.0.0.1/system/sitemap.json')
    else:
        cmd = (f'curl -skI --max-time 6 --resolve {door}:443:127.0.0.1 '
               f'https://{door}/system/sitemap.json')
    _, out, _ = sh(cmd)
    status = next((l.split()[1] for l in out.splitlines()
                   if l.startswith("HTTP/")), "000")
    if status != "200":
        return f"error:{status}"
    return "node" if any(l.lower().startswith("etag:")
                         for l in out.splitlines()) else "sweet"


def status_payload():
    from concurrent.futures import ThreadPoolExecutor

    live = set(read_snippet())
    groups = sorted(door_groups().items(), key=lambda kv: kv[1][0])
    # probe every group's first door concurrently — the pi5 farm has ~75
    # groups and a sequential curl sweep took 20s
    with ThreadPoolExecutor(max_workers=16) as pool:
        probes = list(pool.map(lambda kv: which_server(kv[1][0]), groups))
    out = []
    for (real, doors), probe in zip(groups, probes):
        out.append({
            "doors": doors,
            "routed": "sweet" if any(d in live for d in doors) else "node",
            "probe": probe,
            "urls": [("http://" if d.endswith(".local") else "https://") + d
                     for d in doors],
        })
    return {"host": CFG.label, "sweet_port": CFG.sweet_port,
            "checked": utcnow(), "sites": out}


def toggle(site, to):
    groups = door_groups()
    match = [ds for ds in groups.values()
             if site in ds or any(d.split(".")[0] == site for d in ds)]
    if not match:
        raise ValueError(f"no site '{site}' under {CFG.farm}")
    if len(match) > 1 and not any(site in ds for ds in match):
        raise ValueError(f"'{site}' is ambiguous")
    doors = next((ds for ds in match if site in ds), match[0])
    if to == "sweet":
        code, out, _ = sh(
            f'curl -s -o /dev/null -w "%{{http_code}}" --max-time 6 '
            f'-H "Host: {doors[0]}" '
            f'http://127.0.0.1:{CFG.sweet_port}/system/sitemap.json')
        if out.strip() != "200":
            raise RuntimeError(
                f"sweet farm on :{CFG.sweet_port} will not serve {doors[0]}")
        live = set(read_snippet()) | set(doors)
    else:
        live = set(read_snippet()) - set(doors)
    write_snippet(sorted(live))
    return {"toggled": doors, "to": to}


# ---------------------------------------------------------------- ui

UI = """<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>Sweet Toggle</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  :root { color-scheme: light dark; }
  body { font: 14px/1.5 system-ui, sans-serif; margin: 0.8rem; }
  h2 { font-size: 1rem; margin: 1rem 0 0.3rem; }
  .site { display: flex; align-items: baseline; gap: 0.6em;
          padding: 0.25rem 0.4rem; border-radius: 6px; flex-wrap: wrap; }
  .site:nth-child(odd) { background: rgba(128,128,128,0.08); }
  .state { font-weight: 600; width: 4.2em; }
  .sweet .state { color: #0a9396; }
  .node .state { color: #9b7300; }
  .mismatch .state { color: #c1121f; }
  button { font: inherit; padding: 0.1em 0.7em; border-radius: 6px;
           border: 1px solid rgba(128,128,128,0.5); background: transparent;
           cursor: pointer; }
  button:hover { background: rgba(128,128,128,0.15); }
  button[disabled] { opacity: 0.4; cursor: wait; }
  a { color: inherit; }
  .doors { flex: 1; min-width: 14em; }
  .note { opacity: 0.65; font-size: 0.85em; margin: 0.4rem 0 1rem; }
  .err { color: #c1121f; }
</style></head><body>
<div class="note">Toggle a wiki between the <b>node server</b> and the
<b>Sweet Server</b> (Rust, classic client, read-only while toggled — flip
back to edit). Links open the live site through its normal door.</div>
<input id="filter" type="search" placeholder="filter sites…"
  style="font: inherit; padding: 0.2em 0.6em; margin-bottom: 0.4rem;
         border-radius: 6px; border: 1px solid rgba(128,128,128,0.5);
         background: transparent; color: inherit; width: 16em;">
<div id="hosts"></div>
<script>
const APIS = __APIS__;
const hostsDiv = document.getElementById("hosts");

// stable section order + visible loading state, whatever answers first
for (const api of APIS) {
  const sec = document.createElement("section");
  sec.id = "h-" + api;
  sec.innerHTML = "<h2>" + api.replace(/^https:\\/\\/toggle\\./, "")
    .replace(/\\.private\\.fish\\/?$/, "") + "</h2><div class='note'>checking…</div>";
  hostsDiv.appendChild(sec);
}

document.getElementById("filter").addEventListener("input", e => {
  const q = e.target.value.toLowerCase();
  document.querySelectorAll(".site").forEach(row => {
    row.style.display = row.textContent.toLowerCase().includes(q) ? "" : "none";
  });
});

async function fetchStatus(api) {
  const r = await fetch(api + "/api/status", {cache: "no-store"});
  if (!r.ok) throw new Error(r.status);
  return r.json();
}

function render(api, data, err) {
  let sec = document.getElementById("h-" + api);
  if (!sec) {
    sec = document.createElement("section");
    sec.id = "h-" + api;
    hostsDiv.appendChild(sec);
  }
  if (err) {
    sec.innerHTML = "<h2>" + api + "</h2><div class='err'>unreachable: "
      + err + "</div>";
    return;
  }
  const rows = data.sites.map(s => {
    const name = s.doors[0];
    const onSweet = s.routed === "sweet";
    const cls = s.probe.startsWith("error") ? s.routed
      : (s.probe === s.routed ? s.routed : "mismatch");
    const links = s.urls.map(u =>
      "<a href='" + u + "' target='_blank'>" + u.replace(/^https?:\\/\\//, "")
      + "</a>").join(" · ");
    const probeNote = s.probe.startsWith("error") ? " <small>(no front door)</small>"
      : (s.probe !== s.routed ? " <small class='err'>probe: " + s.probe + "</small>" : "");
    return "<div class='site " + cls + "'>"
      + "<span class='state'>" + s.routed + "</span>"
      + "<button onclick=\\"doToggle('" + api + "','" + name + "','"
      + (onSweet ? "node" : "sweet") + "',this)\\">"
      + (onSweet ? "back to node" : "serve from sweet") + "</button>"
      + "<span class='doors'>" + links + probeNote + "</span></div>";
  }).join("");
  sec.innerHTML = "<h2>" + data.host + " <small>(sweet :"
    + data.sweet_port + ", checked " + data.checked + ")</small></h2>" + rows;
}

async function refresh() {
  await Promise.all(APIS.map(async api => {
    try { render(api, await fetchStatus(api), null); }
    catch (e) { render(api, null, e.message); }
  }));
  // rows were rebuilt — re-apply the filter
  document.getElementById("filter").dispatchEvent(new Event("input"));
}

async function doToggle(api, site, to, btn) {
  btn.disabled = true;
  try {
    const r = await fetch(api + "/api/toggle", {
      method: "POST", headers: {"Content-Type": "application/json"},
      body: JSON.stringify({site, to})});
    const j = await r.json();
    if (!r.ok) alert(j.error || r.status);
  } catch (e) { alert(e.message); }
  await refresh();
}

refresh();
setInterval(refresh, 30000);
</script></body></html>
"""


# ---------------------------------------------------------------- http

class Handler(BaseHTTPRequestHandler):
    def _send(self, code, body, ctype="application/json"):
        data = body if isinstance(body, bytes) else json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Headers", "Content-Type")
        self.send_header("Access-Control-Allow-Methods", "GET, POST, OPTIONS")
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(data)

    def do_OPTIONS(self):
        self._send(204, b"")

    def do_GET(self):
        if self.path in ("/", "/index.html"):
            ui = UI.replace("__APIS__", json.dumps(CFG.apis))
            self._send(200, ui.encode(), "text/html; charset=utf-8")
        elif self.path == "/api/status":
            self._send(200, status_payload())
        else:
            self._send(404, {"error": "not found"})

    def do_POST(self):
        if self.path != "/api/toggle":
            return self._send(404, {"error": "not found"})
        try:
            n = int(self.headers.get("Content-Length", 0))
            req = json.loads(self.rfile.read(n) or b"{}")
            site = str(req.get("site", ""))
            to = str(req.get("to", ""))
            if to not in ("sweet", "node") or not re.fullmatch(
                    r"[a-z0-9.-]{1,120}", site):
                return self._send(400, {"error": "need site + to=sweet|node"})
            self._send(200, toggle(site, to))
        except (ValueError, RuntimeError) as e:
            self._send(409, {"error": str(e)})
        except Exception as e:  # noqa: BLE001 — surface, never crash the door
            self._send(500, {"error": f"{type(e).__name__}: {e}"})

    def log_message(self, fmt, *args):
        sys.stderr.write(f"{utcnow()} {self.address_string()} {fmt % args}\n")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--label", required=True)
    ap.add_argument("--farm", required=True)
    ap.add_argument("--snippet", required=True)
    ap.add_argument("--reload-cmd", required=True)
    ap.add_argument("--sweet-port", type=int, required=True)
    ap.add_argument("--port", type=int, default=4264)
    ap.add_argument("--sudo", default="")
    ap.add_argument("--apis", nargs="+", required=True,
                    help="API base URLs the UI should query (this host first)")
    global CFG
    CFG = ap.parse_args()
    srv = ThreadingHTTPServer(("127.0.0.1", CFG.port), Handler)
    sys.stderr.write(f"sweet-toggle-api [{CFG.label}] on 127.0.0.1:{CFG.port} "
                     f"snippet={CFG.snippet} sweet=:{CFG.sweet_port}\n")
    srv.serve_forever()


if __name__ == "__main__":
    main()
