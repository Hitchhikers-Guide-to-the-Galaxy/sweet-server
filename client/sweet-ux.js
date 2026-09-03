// sweet-ux.js — the client half of the customisable tool pages.
//
// Served at /system/sweet-ux.js and injected into the shell by sweet-server
// itself, which is the whole point: the node farms can only get a global
// client hook by FORKING the security module (wiki-security-hitchhiker
// exists so its client/security.js can inject a script tag). Sweet writes
// its own shell, so it just adds the tag.
//
// It touches nothing but the public window.wiki / window.$ surface:
//   wiki.newPage(json)                 -> a page object
//   wiki.showResult(pageObject)        -> appends it to the lineup
//   wiki.lineup.changePageIndex(k, i)  -> puts it in the right place
//
// The tool pages themselves are ordinary wiki pages resolved server-side
// (/system/tool/<kind>/<slug>.json), so customising them is authoring, not
// patching this file.
(function () {
  'use strict'

  var READY_TRIES = 100
  var installed = false
  var tries = 0

  var poll = setInterval(function () {
    tries++
    var ready = window.wiki && window.$ && window.wiki.newPage &&
      window.wiki.showResult && window.wiki.lineup
    if (ready && !installed) {
      installed = true
      clearInterval(poll)
      try { install() } catch (e) { console.error('[sweet-ux] install failed', e) }
    } else if (tries > READY_TRIES) {
      clearInterval(poll)
    }
  }, 200)

  function toolUrl (kind, slug) {
    return '/system/tool/' + kind + '/' + encodeURIComponent(slug) + '.json'
  }

  // Probe once per kind. If the server does not offer this tool the
  // interceptor is never installed at all, so the client's own behaviour is
  // left completely intact — a missing or broken template can't strand a
  // reader.
  function offers (kind) {
    var home = (window.wiki && window.wiki.home) || 'welcome-visitors'
    return fetch(toolUrl(kind, home), { method: 'GET' })
      .then(function (r) { return r.ok })
      .catch(function () { return false })
  }

  function slugOf ($page) {
    return ($page.attr('id') || '').split('_rev')[0]
  }

  // Insert the tool page to the RIGHT of the page it came from, never
  // wiping the lineup — the behaviour the house farms established.
  function openBeside ($page, json) {
    var $ = window.$
    var wiki = window.wiki
    var index = $page.prevAll('.page').length
    wiki.showResult(wiki.newPage(json))
    var $tool = $('.main > .page').last()
    $tool.insertAfter($page)
    try { wiki.lineup.changePageIndex($tool.data('key'), index + 1) } catch (e) {}
    $tool.addClass('sweet-ux-tool')
    try {
      $tool[0].scrollIntoView({ behavior: 'smooth', inline: 'center', block: 'nearest' })
    } catch (e) {}
    return $tool
  }

  function openTool (kind, $page) {
    var slug = slugOf($page)
    if (!slug) return
    fetch(toolUrl(kind, slug))
      .then(function (r) {
        if (!r.ok) throw new Error('tool ' + kind + ' -> ' + r.status)
        return r.json()
      })
      .then(function (json) { openBeside($page, json) })
      .catch(function (e) { console.warn('[sweet-ux]', e.message) })
  }

  // Shift-click on a flag keeps the Simplify shortcut: collapse the trail to
  // Welcome Visitors plus this page.
  function simplify ($page) {
    var slug = slugOf($page)
    window.location.assign(
      slug === 'welcome-visitors'
        ? '/view/welcome-visitors'
        : '/view/welcome-visitors/view/' + slug)
  }

  function intercept (selector, handler) {
    document.addEventListener('click', function (e) {
      var hit = e.target && e.target.closest ? e.target.closest(selector) : null
      if (!hit) return
      var $page = window.$(hit).closest('.page')
      if (!$page.length || $page.hasClass('sweet-ux-tool')) return
      e.preventDefault()
      e.stopImmediatePropagation()
      handler($page, e)
    }, true) // capture: ahead of the client's own delegated handlers
  }

  function install () {
    offers('page').then(function (yes) {
      if (!yes) return
      intercept('.page h1 a', function ($page, e) {
        if (e.shiftKey) { simplify($page); return }
        openTool('page', $page)
      })
    })
    offers('search').then(function (yes) {
      if (!yes) return
      intercept('.page .footer a.search', function ($page) {
        openTool('search', $page)
      })
    })
    console.log('[sweet-ux] installed')
  }
})()
