// sidecar_host.js — the ONE embedded Node handler host (Sweet Plugin Plan).
//
// A dumb host, by decision: coercion, defaults, context injection, gating
// and the required-check all happen in Rust before a call reaches here.
// This process only does what Rust cannot — import JavaScript modules and
// run their plain functions.
//
// Protocol: NDJSON over stdio, strictly sequential — one request line in,
// one response line out, same order.
//   {"id":1,"op":"handlers","path":"<abs handlers.js>"}
//     -> {"id":1,"ok":true,"value":{"exports":["roll", ...]}}
//   {"id":2,"op":"call","path":"...","fn":"roll","args":{...}}
//     -> {"id":2,"ok":true,"value":<whatever the handler returned>}
//     -> {"id":2,"ok":false,"error":{"message":"...","status":500}}
//   {"id":3,"op":"vocabulary","path":"<abs commands.js>"}
//     -> loadVocabulary's {version, commands, itemType, fallback}
//        (version-check error strings match the reference's vocabulary.js)
//
// Handler modules are imported once and cached, like the farm's own
// mountDeclaration. The merge of default and named exports mirrors
// mount.js: named exports win.

'use strict'
const readline = require('node:readline')
const { pathToFileURL } = require('node:url')

const SUPPORTED_VOCABULARY_VERSIONS = [1]
const modules = new Map()

async function load(path) {
  if (!modules.has(path)) {
    const ns = await import(pathToFileURL(path).href)
    const fromDefault = ns.default && typeof ns.default === 'object' ? ns.default : {}
    modules.set(path, { ...fromDefault, ...ns })
  }
  return modules.get(path)
}

async function handle(req) {
  switch (req.op) {
    case 'handlers': {
      const handlers = await load(req.path)
      const exports = Object.keys(handlers).filter(k => typeof handlers[k] === 'function')
      return { exports }
    }
    case 'call': {
      const handlers = await load(req.path)
      const fn = handlers[req.fn]
      if (typeof fn !== 'function') throw new Error(`handlers module exports no function ${req.fn}`)
      return await fn(req.args || {})
    }
    case 'vocabulary': {
      const ns = await load(req.path)
      const version = ns.VOCABULARY_VERSION
      if (!SUPPORTED_VOCABULARY_VERSIONS.includes(version)) {
        throw new Error(
          `vocabulary version ${JSON.stringify(version)} is not understood ` +
            `(this farm understands ${SUPPORTED_VOCABULARY_VERSIONS.join(', ')})`,
        )
      }
      if (!ns.COMMANDS || typeof ns.COMMANDS !== 'object') throw new Error('vocabulary exports no COMMANDS table')
      return {
        version,
        commands: ns.COMMANDS,
        itemType: ns.ITEM_TYPE || req.name,
        fallback: ns.FALLBACK || [],
      }
    }
    default:
      throw new Error(`unknown op ${req.op}`)
  }
}

const rl = readline.createInterface({ input: process.stdin, terminal: false })
let queue = Promise.resolve()
rl.on('line', line => {
  queue = queue.then(async () => {
    let req
    try {
      req = JSON.parse(line)
    } catch (e) {
      process.stdout.write(JSON.stringify({ id: null, ok: false, error: { message: e.message } }) + '\n')
      return
    }
    try {
      const value = await handle(req)
      process.stdout.write(JSON.stringify({ id: req.id, ok: true, value: value === undefined ? null : value }) + '\n')
    } catch (e) {
      process.stdout.write(
        JSON.stringify({ id: req.id, ok: false, error: { message: e.message, status: e.status } }) + '\n',
      )
    }
  })
})
rl.on('close', () => process.exit(0))
