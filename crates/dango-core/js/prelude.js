// Host environment for remote provider modules running inside QuickJS.
// Recreates the subset of Node/WHATWG globals and the `ctx` object that
// provider modules were written against (see docs/providers.md).
// `__host` is installed by Rust before this file runs.
;(function () {
  'use strict'
  const host = globalThis.__host
  const G = globalThis

  G.global = G
  G.self = G
  G.window = undefined
  G.process = { env: {}, platform: 'dango', versions: {}, nextTick: (fn, ...a) => Promise.resolve().then(() => fn(...a)) }

  // ---------- console ----------
  const fmt = (args) =>
    args
      .map((a) => {
        if (typeof a === 'string') return a
        if (a instanceof Error) return `${a.name}: ${a.message}`
        try {
          return JSON.stringify(a)
        } catch {
          return String(a)
        }
      })
      .join(' ')
  G.console = {
    log: (...a) => host.log('debug', fmt(a)),
    info: (...a) => host.log('info', fmt(a)),
    warn: (...a) => host.log('warn', fmt(a)),
    error: (...a) => host.log('error', fmt(a)),
    debug: (...a) => host.log('debug', fmt(a)),
    trace: () => {},
  }

  // ---------- timers ----------
  let timerSeq = 1
  const liveTimers = new Set()
  G.setTimeout = (fn, ms, ...args) => {
    const id = timerSeq++
    liveTimers.add(id)
    host.sleep(Math.max(0, Number(ms) || 0)).then(() => {
      if (!liveTimers.delete(id)) return
      if (typeof fn === 'function') fn(...args)
    })
    return id
  }
  G.clearTimeout = (id) => {
    liveTimers.delete(id)
  }
  G.setInterval = (fn, ms, ...args) => {
    const id = timerSeq++
    liveTimers.add(id)
    const tick = () =>
      host.sleep(Math.max(1, Number(ms) || 1)).then(() => {
        if (!liveTimers.has(id)) return
        fn(...args)
        tick()
      })
    tick()
    return id
  }
  G.clearInterval = G.clearTimeout
  G.setImmediate = (fn, ...a) => G.setTimeout(fn, 0, ...a)
  G.queueMicrotask = G.queueMicrotask || ((fn) => Promise.resolve().then(fn))
  G.structuredClone = G.structuredClone || ((v) => (v === undefined ? v : JSON.parse(JSON.stringify(v))))

  // ---------- text encoding ----------
  class TextEncoder {
    get encoding() {
      return 'utf-8'
    }
    encode(str = '') {
      return new Uint8Array(G.Buffer.from(String(str), 'utf8'))
    }
  }
  class TextDecoder {
    constructor(label = 'utf-8') {
      this.encoding = String(label).toLowerCase()
    }
    decode(input) {
      if (!input) return ''
      const bytes = input instanceof ArrayBuffer ? new Uint8Array(input) : new Uint8Array(input.buffer, input.byteOffset, input.byteLength)
      const enc = this.encoding === 'latin1' || this.encoding === 'iso-8859-1' ? 'latin1' : 'utf8'
      return G.Buffer.from(bytes).toString(enc)
    }
  }
  G.TextEncoder = TextEncoder
  G.TextDecoder = TextDecoder
  G.atob = (s) => G.Buffer.from(String(s).replace(/[\s]/g, ''), 'base64').toString('latin1')
  G.btoa = (s) => {
    const str = String(s)
    for (let i = 0; i < str.length; i++) {
      if (str.charCodeAt(i) > 255) throw new Error('InvalidCharacterError: btoa')
    }
    return G.Buffer.from(str, 'latin1').toString('base64')
  }

  // ---------- URLSearchParams ----------
  const decodeForm = (s) => {
    try {
      return decodeURIComponent(s.replace(/\+/g, ' '))
    } catch {
      return s
    }
  }
  const encodeForm = (s) =>
    encodeURIComponent(s)
      .replace(/%20/g, '+')
      .replace(/[!'()~]/g, (c) => '%' + c.charCodeAt(0).toString(16).toUpperCase())

  class URLSearchParams {
    constructor(init) {
      this._list = []
      this._url = null
      if (init == null) return
      if (typeof init === 'string') {
        this._parse(init)
      } else if (init instanceof URLSearchParams) {
        this._list = init._list.map(([k, v]) => [k, v])
      } else if (typeof init[Symbol.iterator] === 'function') {
        for (const pair of init) this._list.push([String(pair[0]), String(pair[1])])
      } else if (typeof init === 'object') {
        for (const k of Object.keys(init)) this._list.push([k, String(init[k])])
      }
    }
    _parse(s) {
      this._list = []
      const q = s.startsWith('?') ? s.slice(1) : s
      if (!q) return
      for (const part of q.split('&')) {
        if (!part) continue
        const i = part.indexOf('=')
        const k = i < 0 ? part : part.slice(0, i)
        const v = i < 0 ? '' : part.slice(i + 1)
        this._list.push([decodeForm(k), decodeForm(v)])
      }
    }
    _update() {
      if (this._url) this._url._setSearchFromParams(this.toString())
    }
    get size() {
      return this._list.length
    }
    append(k, v) {
      this._list.push([String(k), String(v)])
      this._update()
    }
    delete(k) {
      this._list = this._list.filter(([x]) => x !== String(k))
      this._update()
    }
    get(k) {
      const e = this._list.find(([x]) => x === String(k))
      return e ? e[1] : null
    }
    getAll(k) {
      return this._list.filter(([x]) => x === String(k)).map(([, v]) => v)
    }
    has(k) {
      return this._list.some(([x]) => x === String(k))
    }
    set(k, v) {
      k = String(k)
      const i = this._list.findIndex(([x]) => x === k)
      if (i < 0) this._list.push([k, String(v)])
      else {
        this._list[i] = [k, String(v)]
        this._list = this._list.filter(([x], j) => x !== k || j === i)
      }
      this._update()
    }
    sort() {
      this._list.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0))
      this._update()
    }
    forEach(fn, thisArg) {
      for (const [k, v] of this._list) fn.call(thisArg, v, k, this)
    }
    keys() {
      return this._list.map(([k]) => k)[Symbol.iterator]()
    }
    values() {
      return this._list.map(([, v]) => v)[Symbol.iterator]()
    }
    entries() {
      return this._list.map(([k, v]) => [k, v])[Symbol.iterator]()
    }
    [Symbol.iterator]() {
      return this.entries()
    }
    toString() {
      return this._list.map(([k, v]) => `${encodeForm(k)}=${encodeForm(v)}`).join('&')
    }
  }
  G.URLSearchParams = URLSearchParams

  // ---------- URL (parsing delegated to the Rust `url` crate) ----------
  class URL {
    constructor(input, base) {
      const r = host.urlParse(String(input), base === undefined ? null : String(base))
      if (r === null || r === undefined) throw new TypeError(`Invalid URL: ${input}`)
      this._c = JSON.parse(r)
      this._sp = null
    }
    static canParse(input, base) {
      return host.urlParse(String(input), base === undefined ? null : String(base)) != null
    }
    static parse(input, base) {
      try {
        return new URL(input, base)
      } catch {
        return null
      }
    }
    _set(field, value) {
      const r = host.urlSet(this._c.href, field, String(value))
      if (r) this._c = JSON.parse(r)
      if (this._sp && field !== '__params') {
        this._sp._parse(this._c.search)
      }
    }
    _setSearchFromParams(s) {
      const r = host.urlSet(this._c.href, 'search', s)
      if (r) this._c = JSON.parse(r)
    }
    get href() {
      return this._c.href
    }
    set href(v) {
      const n = new URL(v)
      this._c = n._c
      if (this._sp) this._sp._parse(this._c.search)
    }
    get origin() {
      return this._c.origin
    }
    get protocol() {
      return this._c.protocol
    }
    set protocol(v) {
      this._set('protocol', v)
    }
    get username() {
      return this._c.username
    }
    set username(v) {
      this._set('username', v)
    }
    get password() {
      return this._c.password
    }
    set password(v) {
      this._set('password', v)
    }
    get host() {
      return this._c.host
    }
    set host(v) {
      this._set('host', v)
    }
    get hostname() {
      return this._c.hostname
    }
    set hostname(v) {
      this._set('hostname', v)
    }
    get port() {
      return this._c.port
    }
    set port(v) {
      this._set('port', v)
    }
    get pathname() {
      return this._c.pathname
    }
    set pathname(v) {
      this._set('pathname', v)
    }
    get search() {
      return this._c.search
    }
    set search(v) {
      this._set('search', v)
    }
    get hash() {
      return this._c.hash
    }
    set hash(v) {
      this._set('hash', v)
    }
    get searchParams() {
      if (!this._sp) {
        this._sp = new URLSearchParams(this._c.search)
        this._sp._url = this
      }
      return this._sp
    }
    toString() {
      return this._c.href
    }
    toJSON() {
      return this._c.href
    }
  }
  G.URL = URL

  // ---------- AbortController (timeouts are enforced by the Rust HTTP client) ----------
  class AbortSignal {
    constructor() {
      this.aborted = false
      this.reason = undefined
      this._timeoutMs = undefined
      this._listeners = []
    }
    static timeout(ms) {
      const s = new AbortSignal()
      s._timeoutMs = Number(ms) || undefined
      return s
    }
    static any(signals) {
      const s = new AbortSignal()
      const t = (signals || []).map((x) => x && x._timeoutMs).filter(Boolean)
      if (t.length) s._timeoutMs = Math.min(...t)
      return s
    }
    addEventListener(type, fn) {
      if (type === 'abort') this._listeners.push(fn)
    }
    removeEventListener(type, fn) {
      this._listeners = this._listeners.filter((f) => f !== fn)
    }
    throwIfAborted() {
      if (this.aborted) throw this.reason
    }
  }
  class AbortController {
    constructor() {
      this.signal = new AbortSignal()
    }
    abort(reason) {
      if (this.signal.aborted) return
      this.signal.aborted = true
      this.signal.reason = reason ?? new Error('AbortError')
      for (const fn of this.signal._listeners) {
        try {
          fn({ type: 'abort' })
        } catch {}
      }
    }
  }
  G.AbortSignal = AbortSignal
  G.AbortController = AbortController

  // ---------- fetch ----------
  class Headers {
    constructor(init) {
      this._list = []
      if (!init) return
      if (init instanceof Headers) this._list = init._list.map(([k, v]) => [k, v])
      else if (Array.isArray(init)) for (const [k, v] of init) this.append(k, v)
      else for (const k of Object.keys(init)) if (init[k] !== undefined) this.append(k, init[k])
    }
    append(k, v) {
      this._list.push([String(k).toLowerCase(), String(v)])
    }
    set(k, v) {
      this.delete(k)
      this.append(k, v)
    }
    delete(k) {
      k = String(k).toLowerCase()
      this._list = this._list.filter(([x]) => x !== k)
    }
    get(k) {
      k = String(k).toLowerCase()
      const vals = this._list.filter(([x]) => x === k).map(([, v]) => v)
      if (!vals.length) return null
      return k === 'set-cookie' ? vals.join(', ') : vals.join(', ')
    }
    getSetCookie() {
      return this._list.filter(([x]) => x === 'set-cookie').map(([, v]) => v)
    }
    has(k) {
      k = String(k).toLowerCase()
      return this._list.some(([x]) => x === k)
    }
    forEach(fn, thisArg) {
      for (const [k, v] of this._list) fn.call(thisArg, v, k, this)
    }
    entries() {
      return this._list.map(([k, v]) => [k, v])[Symbol.iterator]()
    }
    keys() {
      return this._list.map(([k]) => k)[Symbol.iterator]()
    }
    values() {
      return this._list.map(([, v]) => v)[Symbol.iterator]()
    }
    [Symbol.iterator]() {
      return this.entries()
    }
    toObject() {
      const o = {}
      for (const [k, v] of this._list) o[k] = o[k] ? `${o[k]}, ${v}` : v
      return o
    }
  }
  G.Headers = Headers

  class Response {
    constructor(body, meta = {}) {
      this._body = body == null ? '' : String(body)
      this.status = meta.status ?? 200
      this.statusText = meta.statusText ?? ''
      this.ok = this.status >= 200 && this.status < 300
      this.url = meta.url ?? ''
      this.redirected = !!meta.redirected
      this.headers = new Headers(meta.headers || [])
      this.bodyUsed = false
      this.type = 'basic'
    }
    async text() {
      this.bodyUsed = true
      return this._body
    }
    async json() {
      this.bodyUsed = true
      return JSON.parse(this._body.replace(/^﻿/, ''))
    }
    async arrayBuffer() {
      this.bodyUsed = true
      const b = G.Buffer.from(this._body, this._binary ? 'base64' : 'utf8')
      return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength)
    }
    async blob() {
      return this.arrayBuffer()
    }
    clone() {
      const r = new Response(this._body, { status: this.status, url: this.url, headers: this.headers._list })
      return r
    }
  }
  G.Response = Response
  G.Request = class Request {
    constructor(url, init = {}) {
      this.url = String(url && url.url ? url.url : url)
      Object.assign(this, init)
    }
  }

  function headersToPairs(h) {
    if (!h) return []
    if (h instanceof Headers) return h._list.map(([k, v]) => [k, v])
    if (Array.isArray(h)) return h.map(([k, v]) => [String(k), String(v)])
    return Object.keys(h)
      .filter((k) => h[k] !== undefined && h[k] !== null)
      .map((k) => [k, String(h[k])])
  }

  function bodyToString(body) {
    if (body == null) return null
    if (typeof body === 'string') return body
    if (body instanceof URLSearchParams) return body.toString()
    if (body instanceof ArrayBuffer || ArrayBuffer.isView(body)) return G.Buffer.from(body).toString('utf8')
    return String(body)
  }

  async function rawRequest(opts) {
    const [metaJson, body] = await host.http(JSON.stringify(opts))
    const meta = JSON.parse(metaJson)
    if (meta.error) {
      const err = new TypeError(`fetch failed: ${meta.error}`)
      err.cause = { code: meta.code || 'ENETWORK', message: meta.error }
      throw err
    }
    return { meta, body }
  }

  G.fetch = async function fetch(input, init = {}) {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url
    const headers = headersToPairs(init.headers || (input && input.headers))
    const body = bodyToString(init.body)
    if (body != null && init.body instanceof URLSearchParams && !headers.some(([k]) => k.toLowerCase() === 'content-type')) {
      headers.push(['content-type', 'application/x-www-form-urlencoded;charset=UTF-8'])
    }
    if (init.signal && init.signal.aborted) throw init.signal.reason || new Error('AbortError')
    const { meta, body: text } = await rawRequest({
      url: String(url),
      method: (init.method || 'GET').toUpperCase(),
      headers,
      body,
      timeoutMs: (init.signal && init.signal._timeoutMs) || undefined,
      redirect: init.redirect || 'follow',
    })
    return new Response(text, meta)
  }

  // ---------- ctx ----------
  const GENERIC_UA =
    'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36'

  function withDefaultUa(pairs) {
    if (!pairs.some(([k]) => k.toLowerCase() === 'user-agent')) pairs.unshift(['User-Agent', GENERIC_UA])
    return pairs
  }

  function makeLogger(id) {
    const emit = (level) => (obj, msg) => {
      let text = msg || ''
      if (typeof obj === 'string') text = obj + (msg ? ' ' + msg : '')
      else if (obj && typeof obj === 'object') {
        const extra = {}
        for (const k of Object.keys(obj)) {
          const v = obj[k]
          extra[k] = v instanceof Error ? v.message : v
        }
        try {
          text += ' ' + JSON.stringify(extra)
        } catch {}
      }
      host.log(level, `[${id}] ${text}`)
    }
    return { info: emit('info'), warn: emit('warn'), error: emit('error'), debug: emit('debug') }
  }

  // Title matching is a direct port of server/src/providers/title-matching.ts.
  const normalizeCompact = (s) => s.toLowerCase().replace(/[^a-z0-9]/g, '')
  const tokenize = (s) =>
    s
      .toLowerCase()
      .replace(/[^\p{L}\p{N}\s]/gu, ' ')
      .split(/\s+/)
      .filter((w) => w.length >= 2)
  function bigramDice(a, b) {
    if (a.length < 2 || b.length < 2) return a === b ? 1 : 0
    const grams = (s) => {
      const m = new Map()
      for (let i = 0; i < s.length - 1; i++) {
        const g = s.slice(i, i + 2)
        m.set(g, (m.get(g) || 0) + 1)
      }
      return m
    }
    const A = grams(a)
    const B = grams(b)
    let ta = 0
    let tb = 0
    let shared = 0
    A.forEach((c) => (ta += c))
    B.forEach((c) => (tb += c))
    A.forEach((c, g) => {
      const o = B.get(g)
      if (o) shared += Math.min(c, o)
    })
    return (2 * shared) / (ta + tb)
  }
  const STOP = new Set('the animation no wa ga wo ni de to e o x ii yo ova oad hd ep episode season part hen a'.split(' '))
  const SEASON = new Set('season 2nd 3rd 4th 5th ii iii iv v vi vii viii ix x two three four five six seven eight nine ten'.split(' '))
  const ROMAN = { ii: 2, iii: 3, iv: 4, v: 5, vi: 6, vii: 7, viii: 8, ix: 9, x: 10 }
  const WORDN = { two: 2, three: 3, four: 4, five: 5, six: 6, seven: 7, eight: 8, nine: 9, ten: 10 }
  const distinctNoSeason = (s) => [...new Set(tokenize(s).filter((w) => !STOP.has(w) && !SEASON.has(w)))]
  const distinct = (s) => [...new Set(tokenize(s).filter((w) => !STOP.has(w)))]
  function seasonMarker(s) {
    const t = s.toLowerCase()
    const sn = t.match(/\bseason\s*(\d+|ii|iii|iv|v|vi|vii|viii|ix|x|two|three|four|five|six|seven|eight|nine|ten)\b/)
    if (sn) return String(ROMAN[sn[1]] ?? WORDN[sn[1]] ?? sn[1])
    const ord = t.match(/\b(\d+)(st|nd|rd|th)\b/)
    if (ord) return ord[1]
    const tr = t.match(/\b(ii|iii|iv|v|vi|vii|viii|ix|x)\b(?!\s*(st|nd|rd|th|season))/)
    if (tr) return String(ROMAN[tr[1]])
    const tn = t.match(/\b(\d+)\s*(season)?\s*$/)
    if (tn) return tn[1]
    return ''
  }
  function titleSimilarity(query, candidate) {
    const q = normalizeCompact(query)
    const c = normalizeCompact(candidate)
    if (!q || !c) return 0
    if (q === c) return 1
    let score = bigramDice(q, c)
    if (q.startsWith(c) || c.startsWith(q)) {
      const p = 0.5 + 0.5 * (Math.min(q.length, c.length) / Math.max(q.length, c.length))
      if (p > score) score = p
    }
    const qD = distinctNoSeason(query)
    const cD = distinctNoSeason(candidate)
    if (qD.length > 0 && cD.length >= 2) {
      const cs = new Set(cD)
      let hits = 0
      for (const t of qD) if (cs.has(t)) hits++
      const cov = hits / Math.min(qD.length, cD.length)
      if (cov > score) score = cov
    }
    const qm = seasonMarker(query)
    if (qm) {
      const cm = seasonMarker(candidate)
      if (cm === qm) score += 0.25
      else if (!cm && score >= 0.5) score -= 0.25
    }
    return score >= 1 ? 0.99 : score
  }
  function pickBestMatch(candidates, targets, minScore = 0.6) {
    if (!candidates || !candidates.length) return null
    const clean = (targets || []).filter((t) => !!t && String(t).trim().length > 0)
    if (!clean.length) return null
    let best = null
    for (const item of candidates) {
      if (!item || !item.title) continue
      let score = 0
      for (const t of clean) {
        const s = titleSimilarity(t, item.title)
        if (s > score) score = s
      }
      if (!best || score > best.score) best = { item, score }
    }
    return best && best.score >= minScore ? best : null
  }
  function buildQueryVariants(title, romaji) {
    const seen = new Set()
    const out = []
    const push = (q) => {
      const v = String(q || '').replace(/\s+/g, ' ').trim()
      if (v && !seen.has(v.toLowerCase())) {
        seen.add(v.toLowerCase())
        out.push(v)
      }
    }
    title = String(title || '')
    if (romaji && romaji.trim()) push(romaji)
    push(title)
    if (/^re\s*zero$/i.test(title.trim())) push('Re Zero')
    const stripped = (s) => s.replace(/[^\p{L}\p{N}\s]/gu, ' ')
    push(stripped(title))
    if (romaji) push(stripped(romaji))
    const words = stripped(title).split(/\s+/).filter(Boolean)
    if (words.length > 3) push(words.slice(0, 3).join(' '))
    if (words.length > 4) push(words.slice(0, 4).join(' '))
    const tokens = distinct(title)
      .concat(romaji ? distinct(romaji) : [])
      .filter((w) => w.length >= 4)
    const uniq = [...new Set(tokens)].sort((a, b) => b.length - a.length)
    for (const t of uniq.slice(0, 4)) push(t)
    return out
  }
  G.__titleSimilarity = titleSimilarity
  G.__pickBestMatch = pickBestMatch

  function parseMalId(id) {
    if (typeof id === 'number') return id < 0 ? Math.abs(id) : null
    const s = String(id).trim()
    const p = /^mal-(\d+)$/i.exec(s)
    if (p) return parseInt(p[1], 10)
    if (/^-\d+$/.test(s)) return Math.abs(parseInt(s, 10))
    return null
  }

  async function anilistRequest(query, variables) {
    const r = await host.anilist(String(query), JSON.stringify(variables || {}))
    return r ? JSON.parse(r) : null
  }

  async function searchAnilistByTitle(title) {
    const q = `query ($search: String) { Page(page: 1, perPage: 5) { media(search: $search, type: ANIME) { id title { romaji english native } } } }`
    const norm = String(title).normalize('NFD').replace(/[̀-ͯ]/g, '')
    const words = norm.split(/\s+/).filter((w) => w.length >= 3)
    const terms = [title, norm, norm.replace(/:/g, ''), words.slice(0, 4).join(' '), words.slice(0, 3).join(' ')].filter(
      (t, i, a) => t && a.indexOf(t) === i
    )
    for (const term of terms) {
      const r = await anilistRequest(q, { search: term })
      if (!r || !r.data) return null
      const media = r.data.Page?.media || []
      if (!media.length) continue
      const best = pickBestMatch(
        media.map((m) => ({ title: m.title.english || m.title.romaji || '', m })),
        [title],
        0.5
      )
      return (best && best.item.m) || media[0]
    }
    return null
  }

  async function kitsuMetaByAnilistId(anilistId) {
    try {
      const base = 'https://kitsu.io/api/edge'
      const hdr = { Accept: 'application/vnd.api+json' }
      const m = await (
        await G.fetch(`${base}/mappings?filter[externalSite]=anilist/anime&filter[externalId]=${anilistId}&include=item`, {
          headers: hdr,
        })
      ).json()
      const item = (m.included || []).find((x) => x.type === 'anime')
      if (!item) return null
      const maps = await (
        await G.fetch(`${base}/anime/${item.id}/mappings`, { headers: hdr })
      ).json()
      const mal = (maps.data || []).find((x) => x.attributes?.externalSite === 'myanimelist/anime')
      const a = item.attributes || {}
      return {
        id: anilistId,
        idMal: mal ? Number(mal.attributes.externalId) : null,
        title: { romaji: a.titles?.en_jp, english: a.titles?.en, native: a.titles?.ja_jp },
        episodes: a.episodeCount ?? null,
      }
    } catch {
      return null
    }
  }

  function sanitizeCfClearance(raw) {
    if (!raw) return ''
    return String(raw)
      .trim()
      .replace(/^cf_clearance/i, '')
      .replace(/^[:=]\s*/, '')
      .replace(/["']/g, '')
      .trim()
  }

  function makeCtx(id) {
    const cachePrefix = `p:${id}:`
    return {
      cache: {
        get: (key) => {
          const v = host.cacheGet(cachePrefix + key)
          return v == null ? undefined : JSON.parse(v)
        },
        set: (key, value, ttlSeconds) => {
          if (value === undefined) return
          try {
            host.cacheSet(cachePrefix + key, JSON.stringify(value), Number(ttlSeconds) || 0)
          } catch {}
        },
      },
      logger: makeLogger(id),
      fetchText: async (url, init) => {
        try {
          const r = await G.fetch(url, { ...(init || {}), headers: withDefaultUa(headersToPairs(init && init.headers)) })
          if (!r.ok) return null
          return await r.text()
        } catch {
          return null
        }
      },
      fetchJson: async (url, init) => {
        const r = await G.fetch(url, { ...(init || {}), headers: withDefaultUa(headersToPairs(init && init.headers)) })
        if (!r.ok) throw new Error(`HTTP ${r.status}: ${url}`)
        return JSON.parse((await r.text()).replace(/^﻿/, ''))
      },
      proxyUrl: (rawUrl, referer) =>
        `/api/proxy?url=${encodeURIComponent(rawUrl)}&referer=${encodeURIComponent(referer || '')}`,
      userAgent: GENERIC_UA,
      titleMatch: { buildQueryVariants, pickBestMatch },
      resolveBestShowId: async (title, romaji, searchFn) => {
        const targets = [title, romaji].filter((t) => typeof t === 'string' && t.trim().length > 0)
        if (!targets.length) return null
        for (const variant of buildQueryVariants(title, romaji)) {
          let results
          try {
            results = await searchFn(variant)
          } catch (e) {
            if (e && e.message === 'AUTH_REQUIRED') throw e
            continue
          }
          if (!results || !results.length) continue
          const match = pickBestMatch(
            results.filter((r) => r && typeof r.id === 'string' && r.id.length > 0),
            targets
          )
          if (match) return match.item.id
        }
        return null
      },
      anilist: { request: anilistRequest, parseMalId, searchByTitle: searchAnilistByTitle },
      kitsu: { metaByAnilistId: kitsuMetaByAnilistId },
      tmdb: {
        base: 'https://api.themoviedb.org/3',
        image: 'https://image.tmdb.org/t/p',
        get: async () => null,
      },
      request: { get: (key) => (G.__requestContext || {})[key] },
      cookies: {
        sanitizeCfClearance,
        buildCfClearanceCookie: (raw) => {
          const c = sanitizeCfClearance(raw)
          return c ? `cf_clearance=${c}` : ''
        },
      },
      crypto: {
        aes256CbcDecryptJson: (b64url, key, iv) => {
          const r = host.aesCbcDecrypt(String(b64url), String(key), String(iv))
          if (r == null) return null
          try {
            return JSON.parse(r)
          } catch {
            return null
          }
        },
        hmacSha256Base64Url: (key, message) => host.hmacSha256(String(key), String(message)),
      },
      scraping: {
        fetch: async (urlOrOptions, maybeOptions) => {
          const opts = typeof urlOrOptions === 'string' ? { url: urlOrOptions, ...(maybeOptions || {}) } : urlOrOptions
          const timeout = opts.timeout && (opts.timeout.request || opts.timeout.response)
          let body = opts.body
          if (opts.json !== undefined) body = JSON.stringify(opts.json)
          if (opts.form !== undefined) body = new URLSearchParams(opts.form).toString()
          let url = opts.url
          if (opts.searchParams) {
            const u = new URL(url)
            const sp = new URLSearchParams(opts.searchParams)
            sp.forEach((v, k) => u.searchParams.set(k, v))
            url = u.href
          }
          const headers = withDefaultUa(headersToPairs(opts.headers))
          if (opts.json !== undefined && !headers.some(([k]) => k.toLowerCase() === 'content-type'))
            headers.push(['content-type', 'application/json'])
          if (opts.form !== undefined && !headers.some(([k]) => k.toLowerCase() === 'content-type'))
            headers.push(['content-type', 'application/x-www-form-urlencoded'])
          const { meta, body: text } = await rawRequest({
            url: String(url),
            method: String(opts.method || 'GET').toUpperCase(),
            headers,
            body: bodyToString(body),
            timeoutMs: timeout || 30000,
            redirect: opts.followRedirect === false ? 'manual' : 'follow',
            profile: 'browser',
          })
          const hdrs = {}
          for (const [k, v] of meta.headers || []) {
            if (k === 'set-cookie') (hdrs[k] = hdrs[k] || []).push(v)
            else hdrs[k] = hdrs[k] ? `${hdrs[k]}, ${v}` : v
          }
          return { statusCode: meta.status, body: text, headers: hdrs, url: meta.url }
        },
      },
      curl: {
        getJson: async (url, headers) => {
          try {
            const r = await G.fetch(url, { headers: withDefaultUa(headersToPairs(headers)) })
            if (r.status !== 200) return null
            return await r.json()
          } catch {
            return null
          }
        },
        getText: async (url, headers) => {
          try {
            const r = await G.fetch(url, { headers: withDefaultUa(headersToPairs(headers)) })
            if (r.status !== 200) return null
            return await r.text()
          } catch {
            return null
          }
        },
      },
      cheerio: { load: (html, opts) => G.__cheerio.load(html, opts) },
    }
  }

  // ---------- provider bootstrapping & calls ----------
  G.__dango = {
    provider: null,
    facets: null,
    async init(id, ns) {
      const factory = ns && ns.default
      if (typeof factory !== 'function') throw new Error('missing default factory export')
      const provider = await factory(makeCtx(id))
      if (!provider || typeof provider.name !== 'string') throw new Error('invalid provider shape')
      for (const fn of ['search', 'getEpisodes', 'getStreamUrls']) {
        if (typeof provider[fn] !== 'function') throw new Error(`invalid provider shape: missing ${fn}`)
      }
      this.provider = provider
      return JSON.stringify({
        name: provider.name,
        resolveShowId: typeof provider.resolveShowId === 'function',
        isDirectId: typeof provider.isDirectId === 'function',
        browse: typeof provider.browse === 'function',
      })
    },
    // Fallback when resolveShowId is missing or fails: provider search + title matching.
    async resolveBySearch(targets) {
      const p = this.provider
      for (const query of targets.slice(0, 3)) {
        const results = await p.search({ query })
        const match = pickBestMatch(
          (results || []).map((r) => ({ title: r.name || r.englishName || '', id: String(r.id || r._id || '') })).filter((r) => r.id),
          targets
        )
        if (match) return match.item.id
      }
      return null
    },
    async call(method, argsJson, requestCtxJson, timeoutMs) {
      const p0 = this.provider
      if (!p0) throw new Error('provider not initialised')
      // `__name` calls a host helper on __dango instead of the provider.
      const p = method.startsWith('__') ? this : p0
      if (method.startsWith('__')) method = method.slice(2)
      if (typeof p[method] !== 'function') return 'null'
      G.__requestContext = requestCtxJson ? JSON.parse(requestCtxJson) : {}
      const args = JSON.parse(argsJson)
      let timer
      const limit = Number(timeoutMs) > 0 ? Number(timeoutMs) : 30000
      const timeout = new Promise((_, reject) => {
        timer = G.setTimeout(() => reject(new Error(`${method} timed out after ${Math.round(limit / 1000)}s`)), limit)
      })
      try {
        const result = await Promise.race([Promise.resolve().then(() => p[method](...args)), timeout])
        return JSON.stringify(result === undefined ? null : result)
      } catch (e) {
        const msg = e && e.message ? e.message : String(e)
        throw new Error(msg)
      } finally {
        G.clearTimeout(timer)
      }
    },
  }
})()
