// similarity_embed.js — sweet's bridge to wiki-plugin-similarity's own
// embedder (server/embedder.js), so a query becomes the vector the stored
// indexes were built with: the same model, the same child process or
// semindex delegation, the same mock under WIKI_EMBED_MOCK=1.
//
// NDJSON over stdio, one answer per request, in order:
//   {"id":1,"op":"embed","path":"<embedder.js>","text":"…"} -> {"id":1,"ok":true,"value":[…]}
//   {"id":2,"op":"status","path":"…"}                      -> the health route's embedder block
//   {"id":3,"op":"warm","path":"…"}                        -> null (model warming in the background)
// A failure answers {"ok":false,"error":{"message","status"}}, status 503
// when the embedder is down (err.code EMBEDDER_DOWN), else 502.
//
// The embedder and its forked child may print to stdout; the reader in
// sidecar.rs skips any line that is not an answer to its request.

'use strict'
const readline = require('node:readline')

const modules = new Map()
const load = path => {
  if (!modules.has(path)) modules.set(path, require(path))
  return modules.get(path)
}
const answer = o => process.stdout.write(JSON.stringify(o) + '\n')

const rl = readline.createInterface({ input: process.stdin, terminal: false })
let queue = Promise.resolve()
rl.on('line', line => {
  queue = queue.then(async () => {
    let req
    try { req = JSON.parse(line) } catch { return }
    try {
      const embedder = load(req.path)
      let value = null
      if (req.op === 'embed') value = await embedder.embed(req.text)
      else if (req.op === 'status') value = embedder.status()
      else if (req.op === 'warm') embedder.warm()
      else throw new Error(`unknown op ${req.op}`)
      answer({ id: req.id, ok: true, value })
    } catch (e) {
      answer({ id: req.id, ok: false, error: { message: e.message, status: e.code === 'EMBEDDER_DOWN' ? 503 : 502 } })
    }
  })
})
rl.on('close', () => process.exit(0))
