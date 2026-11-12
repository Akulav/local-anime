// Tsukimi desktop UI (made by Akulav). Plain ES modules, no framework: everything renders from
// template strings into #view; the player is a persistent overlay.

const T = window.__TAURI__
const invoke = (cmd, args) => T.core.invoke(cmd, args)

// ---------------------------------------------------------------- utils
const $ = (s, el = document) => el.querySelector(s)
const $$ = (s, el = document) => [...el.querySelectorAll(s)]
const esc = (s) =>
  String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c])
const titleOf = (m) => m?.title?.english || m?.title?.romaji || m?.title?.native || m?.title || '?'
const coverOf = (m) => m?.coverImage?.extraLarge || m?.coverImage?.large || m?.cover || ''
const fmtTime = (s) => {
  if (!isFinite(s) || s < 0) s = 0
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = Math.floor(s % 60)
  return h ? `${h}:${String(m).padStart(2, '0')}:${String(sec).padStart(2, '0')}` : `${m}:${String(sec).padStart(2, '0')}`
}
const fmtBytes = (b) => {
  if (!b) return '0 B'
  const u = ['B', 'KB', 'MB', 'GB', 'TB']
  const i = Math.min(u.length - 1, Math.floor(Math.log(b) / Math.log(1024)))
  return `${(b / 1024 ** i).toFixed(i > 1 ? 1 : 0)} ${u[i]}`
}
const debounce = (fn, ms) => {
  let t
  return (...a) => {
    clearTimeout(t)
    t = setTimeout(() => fn(...a), ms)
  }
}
const stripHtml = (s) => {
  const d = document.createElement('div')
  d.innerHTML = String(s || '').replace(/<br\s*\/?>/gi, '\n')
  return d.textContent || ''
}
const store = {
  get(k, d) {
    try {
      const v = localStorage.getItem(k)
      return v == null ? d : JSON.parse(v)
    } catch {
      return d
    }
  },
  set(k, v) {
    try {
      localStorage.setItem(k, JSON.stringify(v))
    } catch {}
  },
}

function toast(msg, bad = false, ms = 3500) {
  const text = String(msg)
  // the same message twice in a row (a failing action clicked again) doesn't stack
  const last = $('#toasts').lastElementChild
  if (last && last.textContent === text) last.remove()
  const el = document.createElement('div')
  el.className = `toast${bad ? ' bad' : ''}`
  el.textContent = text
  $('#toasts').append(el)
  while ($('#toasts').childElementCount > 4) $('#toasts').firstElementChild.remove()
  setTimeout(() => el.remove(), bad ? Math.max(ms, 6000) : ms)
}

// Destructive buttons ask once: the first click arms them for 4 s, the second one acts. The armed
// button is remembered by id (pages re-render while you decide) and drawn with `armedClass(id)`.
const Armed = { id: null, at: 0 }
const isArmed = (id) => Armed.id === id && Date.now() - Armed.at < 4000
const armedClass = (id) => (isArmed(id) ? 'confirm' : '')
function confirmed(id, label, btn) {
  if (isArmed(id)) {
    Armed.id = null
    return true
  }
  Armed.id = id
  Armed.at = Date.now()
  btn?.classList.add('confirm')
  setTimeout(() => btn?.classList.remove('confirm'), 4000)
  toast(label)
  return false
}

// Images fade in once loaded (inline handlers are blocked by the CSP).
document.addEventListener('load', (e) => e.target.tagName === 'IMG' && e.target.classList.add('ok'), true)
document.addEventListener('error', (e) => e.target.tagName === 'IMG' && e.target.classList.add('ok'), true)

// ---------------------------------------------------------------- state
const S = {
  boot: null,
  settings: null,
  probes: new Map(), // anilistId -> snapshot
  details: new Map(), // anilistId -> media
  view: null,
}

T.event.listen('probe', ({ payload }) => {
  S.probes.set(payload.anilistId, payload)
  for (const el of $$(`[data-ready="${payload.anilistId}"]`)) el.classList.toggle('hidden', !payload.best)
  if (S.view?.onProbe) S.view.onProbe(payload)
  if (Player.open && Player.id === payload.anilistId) Player.onProbe(payload)
})

// Downloads, refreshed every 2 s for the top bar, the open page and the player.
const DL = {
  list: [],
  async refresh() {
    this.list = await invoke('downloads').catch(() => this.list)
    return this.list
  },
  pct: (d) => (d.total ? Math.min(100, (d.done / d.total) * 100) : 0),
  busy: () => DL.list.filter((d) => d.state === 'downloading' || d.state === 'queued'),
  // Downloads of one show in the current SUB/DUB mode, by episode.
  forShow: (id) => new Map(DL.list.filter((d) => d.anilistId === id && d.mode === S.settings.mode).map((d) => [String(d.episode), d])),
  expires(d) {
    const h = (d.expiresAt - Date.now() / 1000) / 3600
    return h <= 1 ? 'removed within 1h' : h < 48 ? `removed in ${Math.round(h)}h` : `removed in ${Math.round(h / 24)}d`
  },
  retryIn(d) {
    const m = Math.max(0, Math.round((d.retryAt - Date.now() / 1000) / 60))
    return m < 1 ? 'retrying now' : `retrying in ${m} min`
  },
}

// App updates (GitHub releases): the Update button in the rail and the panel in Settings / About.
const Upd = {
  info: null,
  set(info) {
    if (!info) return
    this.info = info
    const b = $('#rail-update')
    const latest = info.latest
    b.classList.toggle('hidden', !latest || info.selfBuilt)
    b.classList.toggle('busy', info.installing)
    b.disabled = info.installing
    if (latest) {
      b.title = info.installing ? `Downloading Tsukimi ${latest.version}…` : `Update to Tsukimi ${latest.version}`
      $('span', b).textContent = info.installing ? (info.size ? `${Math.floor((info.downloaded / info.size) * 100)}%` : '…') : 'Update'
    }
    S.view?.onUpdate?.()
  },
  async refresh() {
    this.set(await invoke('update_status').catch(() => null))
  },
  async check(manual = false) {
    if (manual) this.set({ ...this.info, checking: true })
    const info = await invoke('check_updates').catch((e) => ({ ...this.info, checking: false, error: String(e) }))
    this.set(info)
    if (manual) {
      if (info.error) toast(`Couldn't check for updates: ${info.error}`, true)
      else if (info.latest) toast(`Tsukimi ${info.latest.version} is available`)
      else toast(`Tsukimi ${info.current} is the latest version`)
    }
  },
  async install() {
    if (!this.info?.latest || this.info.installing) return
    if (Player.open) await Player.save()
    toast(`Downloading Tsukimi ${this.info.latest.version}… it restarts when ready`, false, 6000)
    this.set({ ...this.info, installing: true, downloaded: 0, size: 0 })
    const tick = setInterval(() => this.refresh(), 700)
    try {
      await invoke('install_update') // (the app quits when the setup / new version takes over)
    } catch (e) {
      toast(`Update failed: ${e}`, true)
    } finally {
      clearInterval(tick)
      this.refresh()
    }
  },
}
$('#rail-update').addEventListener('click', () => Upd.install())

let polling = false
let polls = 0
async function pollDownloads() {
  if (!S.boot || polling) return
  polling = true
  try {
    await DL.refresh()
  } finally {
    polling = false
  }
  // (the backend checks for updates by itself; pick up what it found once a minute)
  if (++polls % 30 === 0 && !Upd.info?.installing) Upd.refresh()
  const el = $('#status')
  const busy = DL.busy()
  const d = busy.find((x) => x.state === 'downloading') || busy[0]
  el.innerHTML = d
    ? `<a class="chip" href="#/downloads" title="Downloads"><i class="ic ic-disk" style="width:14px;height:14px"></i>${esc(d.title)} · EP ${esc(
        d.episode
      )} · ${d.state === 'queued' ? 'queued' : `${DL.pct(d).toFixed(0)}%`}${busy.length > 1 ? ` · +${busy.length - 1}` : ''}</a>`
    : ''
  S.view?.onDownloads?.()
  if (Player.open) Player.updateDisk()
}
setInterval(pollDownloads, 2000)

// Downloads episodes to keep (until deleted), in order, one at a time.
async function saveEpisodes(media, provider, episodes) {
  if (!provider) return toast('Still looking for a working source, try again in a moment', true)
  try {
    await invoke('download', { id: media.id, provider, episodes, title: titleOf(media), cover: coverOf(media), pinned: true })
    toast(episodes.length === 1 ? `Downloading EP ${episodes[0]}, it stays until you delete it` : `Downloading ${episodes.length} episodes, they stay until you delete them`)
    pollDownloads()
  } catch (e) {
    toast(String(e), true)
  }
}

// Links open in the system browser.
document.addEventListener('click', (e) => {
  const a = e.target.closest('[data-href]')
  if (!a) return
  e.preventDefault()
  invoke('plugin:opener|open_url', { url: a.dataset.href }).catch(() => window.open(a.dataset.href, '_blank'))
})

async function getDetails(id, force = false) {
  if (!force && S.details.has(id)) return S.details.get(id)
  const d = await invoke('details', { id })
  S.details.set(id, d)
  return d
}

// ---------------------------------------------------------------- cards
function card(m, opts = {}) {
  const id = m.id
  const sub = [m.format?.replace('_', ' '), m.seasonYear, m.episodes ? `${m.episodes} ep` : null].filter(Boolean).join(' · ')
  const ready = S.probes.get(id)?.best
  const score = m.averageScore ? `<span class="badge">★ ${(m.averageScore / 10).toFixed(1)}</span>` : ''
  return `<a class="card fade-in" href="#/anime/${id}" data-id="${id}">
    <div class="poster">${coverOf(m) ? `<img loading="lazy" decoding="async" src="${esc(coverOf(m))}" alt="">` : ''}${score}
      <span class="ready ${ready ? '' : 'hidden'}" data-ready="${id}" title="Sources ready"></span>${opts.progress ?? ''}</div>
    <div class="t">${esc(titleOf(m))}</div><div class="s">${esc(sub)}</div></a>`
}

function continueCard(p) {
  const pct = p.duration ? Math.min(100, (p.position / p.duration) * 100) : 0
  return `<a class="card fade-in" href="#/anime/${p.anilistId}" data-resume="${p.anilistId}" data-ep="${esc(p.episode)}">
    <div class="poster">${p.cover ? `<img loading="lazy" src="${esc(p.cover)}" alt="">` : ''}
      <span class="badge">EP ${esc(p.episode)}</span><div class="prog"><i style="width:${pct}%"></i></div></div>
    <div class="t">${esc(p.title || `#${p.anilistId}`)}</div><div class="s">${fmtTime(p.position)} / ${fmtTime(p.duration)}</div></a>`
}

function skeletonRow(n = 8) {
  return `<div class="row">${'<div class="card"><div class="poster skeleton"></div></div>'.repeat(n)}</div>`
}

// ---------------------------------------------------------------- views
const view = $('#view')

function setView(v) {
  S.view?.leave?.()
  S.view = v
  view.classList.toggle('flush', !!v.flush)
  view.scrollTop = 0
}

async function homeView() {
  setView({ flush: true })
  const v = S.view
  view.innerHTML = `<div class="hero"><div class="bg skeleton"></div></div>${skeletonRow()}${skeletonRow()}`
  const [home, cont] = await Promise.all([invoke('home').catch((e) => (toast(String(e), true), null)), invoke('continue_watching').catch(() => [])])
  if (S.view !== v) return
  if (!home) {
    view.innerHTML = `<div class="empty">Couldn't reach AniList. Check your connection.</div>`
    return
  }
  const trending = home.trending?.media || []
  const hero = trending.find((m) => m.bannerImage) || trending[0]
  const row = (title, list, sub = '') =>
    list.length ? `<h2 class="row-title">${title}${sub ? ` <small>${sub}</small>` : ''}</h2><div class="row">${list.map((m) => card(m)).join('')}</div>` : ''
  view.innerHTML = `
    ${
      hero
        ? `<div class="hero"><div class="bg" style="background-image:url('${esc(hero.bannerImage || coverOf(hero))}')"></div>
      <div class="inner fade-in"><span class="chip accent">#1 Trending</span><h1>${esc(titleOf(hero))}</h1>
      <p>${esc(stripHtml(hero.description || ''))}</p>
      <div class="actions"><button class="btn primary" data-play="${hero.id}"><i class="ic ic-play"></i>Play</button>
      <a class="btn" href="#/anime/${hero.id}">Details</a></div></div></div>`
        : ''
    }
    <div style="padding:0 0 10px">
    ${cont.length ? `<h2 class="row-title">Continue watching</h2><div class="row">${cont.map(continueCard).join('')}</div>` : ''}
    ${row('Trending now', trending)}
    ${row('Popular this season', home.season?.media || [])}
    ${row('All-time favourites', home.top?.media || [])}
    </div>`
}

async function searchView(q) {
  setView({})
  $('#q').value = q
  if (!q.trim()) {
    view.innerHTML = `<div class="empty">Type to search. Sources for the top results are checked in the background.</div>`
    return
  }
  view.innerHTML = `<h2 class="row-title">Results for “${esc(q)}”</h2><div class="grid">${'<div class="card"><div class="poster skeleton"></div></div>'.repeat(12)}</div>`
  let page = 1
  let more = false
  let loading = false
  const render = async (append) => {
    loading = true
    try {
      const r = await invoke('search', { query: q, page })
      if (S.view?.q !== q) return
      more = !!r.pageInfo?.hasNextPage
      const html = (r.media || []).map((m) => card(m)).join('')
      if (append) $('.grid', view).insertAdjacentHTML('beforeend', html)
      else $('.grid', view).innerHTML = html || `<div class="empty">Nothing found.</div>`
    } catch (e) {
      toast(String(e), true)
    } finally {
      loading = false
    }
  }
  S.view.q = q
  const onScroll = () => {
    if (more && !loading && view.scrollTop + view.clientHeight > view.scrollHeight - 600) {
      page++
      render(true)
    }
  }
  view.addEventListener('scroll', onScroll)
  S.view.leave = () => view.removeEventListener('scroll', onScroll)
  await render(false)
}

const STATE_LABEL = { pending: 'waiting', resolving: 'searching…', verifying: 'testing stream…', ok: 'ready', missing: 'not found', auth: 'needs cookie', failed: 'error' }

function sourcesPanel(snap, selected) {
  if (!snap || !snap.sources.length) return `<div class="empty" style="padding:20px 0">Loading providers…</div>`
  return snap.sources
    .map((s) => {
      const usable = s.state === 'ok' && s.playable !== false
      const cls = `${s.state}${s.state === 'ok' && s.playable == null ? ' unverified' : ''}`
      const info =
        s.state === 'ok'
          ? `${s.episodeCount} ep · ${s.latencyMs ?? '–'} ms${s.playable ? ' · ✓' : ''}${s.embedOnly ? ' · embed' : ''}`
          : STATE_LABEL[s.state]
      return `<div class="src ${usable ? '' : 'off'} ${s.provider === selected ? 'on' : ''}" data-provider="${esc(s.provider)}" title="${esc(s.error || '')}">
        <span class="dot ${cls}"></span><span class="name">${esc(s.label)} <small>${esc(s.sub || '')}</small></span><span class="info">${esc(info)}</span>
        ${!usable && s.error ? `<span class="err">${esc(s.error)}</span>` : ''}</div>`
    })
    .join('')
}

async function showView(id) {
  setView({ flush: true, id })
  view.innerHTML = `<div class="show-hero"><div class="bg skeleton"></div></div>`
  let m
  try {
    m = await getDetails(id, true)
  } catch (e) {
    view.innerHTML = `<div class="empty">${esc(e)}</div>`
    return
  }
  if (S.view?.id !== id) return
  const v = S.view
  v.selected = store.get(`provider:${id}`, null)
  v.media = m
  const progress = new Map((m.progress || []).map((p) => [String(p.episode), p]))
  const last = (m.progress || []).slice().sort((a, b) => b.updatedAt - a.updatedAt)[0]
  const thumbs = (m.streamingEpisodes || []).map((e) => e.thumbnail)
  const studios = (m.studios?.nodes || []).map((s) => s.name).join(', ')
  const next = m.nextAiringEpisode
  const chips = [
    m.format?.replace('_', ' '),
    m.seasonYear,
    m.episodes ? `${m.episodes} episodes` : next ? `${next.episode - 1} aired` : null,
    m.averageScore ? `★ ${(m.averageScore / 10).toFixed(1)}` : null,
    m.status?.replaceAll('_', ' ').toLowerCase(),
    studios,
  ].filter(Boolean)
  const mode = S.settings.mode
  view.innerHTML = `
  <div class="show-hero"><div class="bg" style="background-image:url('${esc(m.bannerImage || coverOf(m))}')"></div>
    <div class="show-head fade-in">
      <div class="poster"><img src="${esc(coverOf(m))}" alt=""></div>
      <div>
        <h1>${esc(titleOf(m))}</h1>
        <div class="alt">${esc([m.title?.romaji, m.title?.native].filter((t) => t && t !== titleOf(m)).join(' · '))}</div>
        <div class="meta">${chips.map((c) => `<span class="chip">${esc(c)}</span>`).join('')}
          ${(m.genres || []).slice(0, 4).map((g) => `<span class="chip">${esc(g)}</span>`).join('')}
          ${next ? `<span class="chip accent">EP ${next.episode} in ${Math.ceil(next.timeUntilAiring / 86400)}d</span>` : ''}</div>
        <div class="actions">
          <button class="btn primary" id="play-main"><i class="ic ic-play"></i>${last ? `Resume EP ${esc(last.episode)}` : 'Play EP 1'}</button>
          <button class="btn" id="lib-toggle"><i class="ic ${m.inLibrary ? 'ic-check' : 'ic-plus'}"></i>${m.inLibrary ? 'In library' : 'Add to library'}</button>
          <button class="btn" id="dl-all" title="Download every episode to keep"><i class="ic ic-disk"></i>Download all</button>
          <div class="seg" id="mode-seg"><button data-mode="sub" class="${mode === 'sub' ? 'on' : ''}">SUB</button><button data-mode="dub" class="${mode === 'dub' ? 'on' : ''}">DUB</button></div>
        </div>
      </div>
    </div>
  </div>
  <div class="show-body">
    <div>
      <div class="desc clamp" id="desc">${esc(stripHtml(m.description || 'No description.'))}</div>
      <span class="link" id="desc-more">Show more</span>
      <div class="eps-head"><h2>Episodes</h2><span class="chip" id="ep-src"></span><input id="ep-filter" placeholder="Jump to #" /></div>
      <div class="eps" id="eps"></div>
      ${
        (m.recommendations?.nodes || []).filter((n) => n.mediaRecommendation).length
          ? `<h2 class="row-title">You might also like</h2><div class="row">${m.recommendations.nodes
              .filter((n) => n.mediaRecommendation)
              .map((n) => card(n.mediaRecommendation))
              .join('')}</div>`
          : ''
      }
    </div>
    <aside><div class="panel"><h3>Sources <span class="grow"></span><span id="probe-state" class="chip"></span></h3><div class="sources" id="sources"></div></div></aside>
  </div>`

  const fallbackEps = () => {
    const n = m.episodes || (next ? next.episode - 1 : 0) || thumbs.length || 1
    return Array.from({ length: n }, (_, i) => ({ number: String(i + 1), title: m.streamingEpisodes?.[i]?.title || null }))
  }
  v.episodes = fallbackEps()
  const renderEps = () => {
    const list = v.episodes
    const compact = list.length > 60
    const el = $('#eps')
    if (!el) return
    el.classList.toggle('compact', compact)
    el.innerHTML = list
      .map((e, i) => {
        const p = progress.get(String(e.number))
        const pct = p && p.duration ? Math.min(100, (p.position / p.duration) * 100) : 0
        const watched = pct > 90
        const th = thumbs[i]
        const t = e.title ? e.title.replace(/^Episode\s+\d+\s*-\s*/i, '') : `Episode ${e.number}`
        return `<div class="ep ${watched ? 'watched' : ''}" data-ep="${esc(e.number)}">
          <div class="th" ${th ? `style="background-image:url('${esc(th)}')"` : ''}>${th ? '' : esc(e.number)}</div>
          <div><div class="n">EP ${esc(e.number)}</div><div class="tt">${esc(t)}</div></div>${pct ? `<span class="bar" style="width:${pct}%"></span>` : ''}
          <span class="ep-dl" data-dl="${esc(e.number)}"></span></div>`
      })
      .join('')
    updateDl(true)
  }

  // Download badge on each episode: nothing, progress, on disk (expires) or saved.
  const updateDl = (force = false) => {
    const map = DL.forShow(id)
    for (const el of $$('.ep-dl', view)) {
      const d = map.get(el.dataset.dl)
      const done = d?.state === 'done'
      const sig = d ? `${d.state}${d.pinned}${Math.floor(DL.pct(d))}` : '-'
      if (!force && el.dataset.sig === sig) continue
      el.dataset.sig = sig
      el.className = `ep-dl${d ? ` has ${done ? (d.pinned ? 'saved' : 'auto') : 'busy'}` : ''}`
      el.innerHTML = d && !done ? (d.state === 'queued' ? '…' : `${Math.floor(DL.pct(d))}%`) : `<i class="ic ${d?.pinned ? 'ic-check' : 'ic-disk'}"></i>`
      el.title = !d
        ? 'Download to keep'
        : done
          ? d.pinned
            ? 'Downloaded, kept until you delete it'
            : `Cached, ${DL.expires(d)}. Click to keep it`
          : d.state === 'failed'
            ? `Failed: ${d.error || 'unknown error'}${d.retryAt ? ` (${DL.retryIn(d)})` : ''}. Click to retry`
            : `${d.state} · ${Math.floor(DL.pct(d))}%`
    }
  }
  v.onDownloads = updateDl
  renderEps()

  const loadEpisodes = async (provider) => {
    $('#ep-src').textContent = provider ? `from ${provider}` : ''
    if (!provider) return
    try {
      const eps = await invoke('episodes', { id, provider })
      if (S.view !== v || !eps.length) return
      // Keep AniList titles where the provider has none.
      const fb = new Map(fallbackEps().map((e) => [e.number, e.title]))
      v.episodes = eps.map((e) => ({ number: e.number, title: e.title || fb.get(e.number) || null }))
      renderEps()
      queueDownloads(provider)
    } catch (e) {
      console.warn(e)
    }
  }

  // Download the episode you'd play next plus the following ones in the background.
  const queueDownloads = (provider) => {
    const n = S.settings.predownloadEpisodes || 0
    if (!n || !v.episodes.length) return
    let start = 0
    if (last) {
      const i = v.episodes.findIndex((e) => String(e.number) === String(last.episode))
      const done = last.duration && last.position / last.duration > 0.9
      if (i >= 0) start = done ? i + 1 : i
    }
    const episodes = v.episodes.slice(start, start + n).map((e) => String(e.number))
    if (episodes.length) invoke('download', { id, provider, episodes, title: titleOf(m), cover: coverOf(m), pinned: false }).catch(() => {})
  }

  const renderSources = (snap) => {
    const sel = v.selected && snap?.sources.some((s) => s.provider === v.selected && s.state === 'ok') ? v.selected : snap?.best
    $('#sources').innerHTML = sourcesPanel(snap, sel)
    const ok = snap?.sources.filter((s) => s.state === 'ok' && s.playable !== false).length ?? 0
    $('#probe-state').textContent = snap ? (snap.done ? `${ok} working` : 'checking…') : ''
    if (sel && sel !== v.loadedProvider) {
      v.loadedProvider = sel
      loadEpisodes(sel)
    }
  }
  v.onProbe = (snap) => snap.anilistId === id && renderSources(snap)
  renderSources(S.probes.get(id))
  invoke('probe', { id })
    .then((snap) => {
      S.probes.set(id, snap)
      if (S.view === v) renderSources(snap)
    })
    .catch((e) => toast(String(e), true))

  $('#sources').addEventListener('click', (e) => {
    const el = e.target.closest('.src:not(.off)')
    if (!el) return
    v.selected = el.dataset.provider
    store.set(`provider:${id}`, v.selected)
    renderSources(S.probes.get(id))
  })
  $('#eps').addEventListener('click', (e) => {
    const dl = e.target.closest('.ep-dl')
    if (dl) {
      const d = DL.forShow(id).get(dl.dataset.dl)
      if (d?.pinned && d.state === 'done') toast(`EP ${d.episode} is downloaded (Downloads to delete it)`)
      else if (d?.state === 'failed') invoke('resume_download', { key: d.key }).then(pollDownloads, (e) => toast(String(e), true))
      else saveEpisodes(m, v.selected || v.loadedProvider, [dl.dataset.dl])
      return
    }
    const el = e.target.closest('.ep')
    if (el) Player.play(m, el.dataset.ep, { provider: v.selected, episodes: v.episodes })
  })
  let confirmAll = 0
  $('#dl-all').onclick = (e) => {
    const btn = e.currentTarget
    const saved = DL.forShow(id)
    const eps = v.episodes.map((x) => String(x.number)).filter((n) => !(saved.get(n)?.pinned && saved.get(n)?.state === 'done'))
    if (!eps.length) return toast('Every episode is already downloaded')
    if (Date.now() - confirmAll > 4000) {
      confirmAll = Date.now()
      btn.innerHTML = `<i class="ic ic-disk"></i>Download ${eps.length} episodes?`
      setTimeout(() => (btn.innerHTML = '<i class="ic ic-disk"></i>Download all'), 4000)
      return
    }
    confirmAll = 0
    btn.innerHTML = '<i class="ic ic-disk"></i>Download all'
    saveEpisodes(m, v.selected || v.loadedProvider, eps)
  }
  $('#play-main').onclick = () => Player.play(m, last ? last.episode : v.episodes[0]?.number || '1', { provider: v.selected, episodes: v.episodes })
  $('#desc-more').onclick = (e) => {
    $('#desc').classList.toggle('clamp')
    e.target.textContent = $('#desc').classList.contains('clamp') ? 'Show more' : 'Show less'
  }
  $('#ep-filter').addEventListener('input', (e) => {
    const n = e.target.value.trim()
    const el = n && $(`.ep[data-ep="${CSS.escape(n)}"]`)
    if (el) el.scrollIntoView({ block: 'center', behavior: 'smooth' })
  })
  $('#lib-toggle').onclick = async () => {
    try {
      if (m.inLibrary) await invoke('library_remove', { id })
      else await invoke('library_add', { id, title: titleOf(m), cover: coverOf(m) })
      m.inLibrary = !m.inLibrary
      $('#lib-toggle').innerHTML = `<i class="ic ${m.inLibrary ? 'ic-check' : 'ic-plus'}"></i>${m.inLibrary ? 'In library' : 'Add to library'}`
    } catch (e) {
      toast(String(e), true)
    }
  }
  $('#mode-seg').addEventListener('click', async (e) => {
    const b = e.target.closest('button')
    if (!b || b.dataset.mode === S.settings.mode) return
    S.settings = await invoke('set_settings', { settings: { ...S.settings, mode: b.dataset.mode } })
    showView(id)
  })
}

async function libraryView() {
  setView({})
  const [lib, cont] = await Promise.all([invoke('library').catch(() => []), invoke('continue_watching').catch(() => [])])
  view.innerHTML = `
    ${cont.length ? `<h2 class="row-title">Continue watching</h2><div class="row">${cont.map(continueCard).join('')}</div>` : ''}
    <h2 class="row-title">Library <small>${lib.length} shows</small></h2>
    ${
      lib.length
        ? `<div class="grid">${lib.map((e) => card({ id: e.anilistId, title: e.title, cover: e.cover })).join('')}</div>`
        : `<div class="empty">Shows you add appear here.</div>`
    }`
}

async function downloadsView() {
  setView({})
  const v = S.view
  view.innerHTML = `<div class="page fade-in">
    <h1>Downloads <small id="dl-total"></small><span class="grow"></span><button class="btn small hidden" data-dl-act="stop">Pause all</button></h1>
    <div class="sub">Downloaded episodes stay until you delete them: cache cleanup never touches them. If a download fails it is retried on its own, from other providers too.</div>
    <div id="dl-body"></div>
    <div class="foot"><i class="ic ic-folder" style="width:14px;height:14px"></i><span class="path">${esc(S.boot.paths.downloads)}</span></div></div>`
  const btn = (act, key, icon, title, cls = '') =>
    `<button class="ibtn ${cls} ${armedClass(`${act}:${key}`)}" data-dl-act="${act}" data-key="${esc(key)}" title="${title}"><i class="ic ${icon}"></i></button>`
  const busyRow = (d) => {
    const pct = DL.pct(d)
    const what = d.state === 'queued' ? 'queued' : `${d.playing ? 'playing · ' : ''}${pct.toFixed(0)}% · ${fmtBytes(d.bytes)}`
    return `<div class="dl"><div class="dl-l"><b>${esc(d.title)} · EP ${esc(d.episode)}</b><span>${what}</span></div>
      <div class="meter"><i style="width:${pct}%"></i></div><div class="acts">${btn('del', d.key, 'ic-trash', 'Cancel and delete', 'danger')}</div></div>`
  }
  const epRow = (d) => {
    const pct = DL.pct(d)
    const info =
      d.state === 'done'
        ? fmtBytes(d.bytes)
        : d.state === 'failed'
          ? `<span class="err" title="${esc(d.error)}">Failed${d.retryAt ? `, ${DL.retryIn(d)}` : ''}: ${esc(d.error)}</span>`
          : `<div class="meter"><i style="width:${pct}%"></i></div>${d.state} · ${pct.toFixed(0)}%`
    const tag = d.pinned
      ? d.state === 'done'
        ? '<span class="tag saved">Kept</span>'
        : ''
      : `<span class="tag" title="Cached while watching, removed automatically">${DL.expires(d)}</span>`
    return `<div class="dl-ep"><span class="n">EP ${esc(d.episode)}${d.mode === 'dub' ? ' · DUB' : ''}</span><div class="info">${tag}${info}</div><div class="acts">
      ${d.done > 0 ? btn('play', d.key, 'ic-play', 'Play') : ''}
      ${d.state === 'paused' || d.state === 'failed' ? btn('resume', d.key, 'ic-refresh', 'Retry now') : ''}
      ${d.pinned ? '' : btn('pin', d.key, 'ic-pin', 'Keep it (until I delete it)')}
      ${btn('del', d.key, 'ic-trash', 'Delete from disk', 'danger')}</div></div>`
  }
  const block = (eps, kept) => {
    const d = eps[0]
    eps.sort((a, b) => parseFloat(a.episode) - parseFloat(b.episode) || String(a.episode).localeCompare(b.episode))
    return `<div class="dl-show"><a class="cv" href="#/anime/${d.anilistId}" ${d.cover ? `style="background-image:url('${esc(d.cover)}')"` : ''}></a><div>
      <h4><a href="#/anime/${d.anilistId}">${esc(d.title)}</a><small>${eps.length} episode${eps.length > 1 ? 's' : ''} · ${fmtBytes(
        eps.reduce((a, x) => a + x.bytes, 0)
      )}</small>${kept ? `<span class="acts">${btn('del-show', String(d.anilistId), 'ic-trash', 'Delete every downloaded episode of this show', 'danger')}</span>` : ''}</h4>
      ${eps.map(epRow).join('')}</div></div>`
  }
  const byShow = (list) => {
    const shows = new Map()
    for (const d of [...list].sort((a, b) => b.usedAt - a.usedAt)) {
      if (!shows.has(d.anilistId)) shows.set(d.anilistId, [])
      shows.get(d.anilistId).push(d)
    }
    return [...shows.values()]
  }
  let cacheOpen = false
  let lastSig = ''
  const render = (force = false) => {
    if (S.view !== v) return
    const list = DL.list
    // (polled every 2 s: only touch the page when something changed)
    const sig = JSON.stringify([list, isArmed(Armed.id) && Armed.id, Math.floor(Date.now() / 60000)])
    if (!force && sig === lastSig) return
    lastSig = sig
    const busy = DL.busy()
    const kept = list.filter((d) => d.pinned)
    const cached = list.filter((d) => !d.pinned && d.state === 'done')
    $('[data-dl-act="stop"]', view).classList.toggle('hidden', !busy.length)
    const done = kept.filter((d) => d.state === 'done')
    $('#dl-total').textContent = kept.length ? `${done.length} downloaded · ${fmtBytes(done.reduce((a, d) => a + d.bytes, 0))}` : ''
    const open = $('#dl-cache', view)?.open ?? cacheOpen
    cacheOpen = open
    $('#dl-body').innerHTML = `
      ${busy.length ? `<div class="panel dl-sec"><h3>In progress</h3><div class="dl-list">${busy.map(busyRow).join('')}</div></div>` : ''}
      <div class="panel dl-sec" style="margin-top:18px"><h3>Downloaded</h3>${
        kept.length
          ? byShow(kept)
              .map((eps) => block(eps, true))
              .join('')
          : `<div class="empty" style="padding:30px 0">Nothing downloaded yet.<br>Use the download button on an episode, <b>Download all</b> on a show, or press <kbd>D</kbd> in the player.</div>`
      }</div>
      ${
        cached.length
          ? `<div class="panel dl-sec" style="margin-top:18px"><details id="dl-cache" ${open ? 'open' : ''}><summary>Cached while watching · ${cached.length} episode${
              cached.length > 1 ? 's' : ''
            } · ${fmtBytes(cached.reduce((a, d) => a + d.bytes, 0))} · removed ${S.settings.cacheMaxAgeHours}h after last use</summary>
            ${byShow(cached)
              .map((eps) => block(eps, false))
              .join('')}</details></div>`
          : ''
      }`
  }
  const onClick = async (e) => {
    const b = e.target.closest('[data-dl-act]')
    if (!b) return
    e.preventDefault()
    const key = b.dataset.key
    const d = DL.list.find((x) => x.key === key)
    try {
      switch (b.dataset.dlAct) {
        case 'play':
          if (d) Player.play(await getDetails(d.anilistId), d.episode, { provider: d.provider })
          return
        case 'pin':
          await invoke('pin_download', { key, pinned: true })
          toast(`EP ${d?.episode} is kept until you delete it`)
          break
        case 'resume':
          await invoke('resume_download', { key })
          break
        case 'del':
          if (d?.pinned && d.state === 'done' && !confirmed(`del:${key}`, `Click again to delete EP ${d.episode} from disk`, b)) return
          await invoke('delete_download', { key })
          break
        case 'del-show': {
          const eps = DL.list.filter((x) => String(x.anilistId) === key && x.pinned)
          const label = `Click again to delete ${eps.length} episode${eps.length > 1 ? 's' : ''} of ${eps[0]?.title || 'this show'}`
          if (!confirmed(`del-show:${key}`, label, b)) return
          for (const x of eps) await invoke('delete_download', { key: x.key })
          toast(`Deleted ${eps.length} episode${eps.length > 1 ? 's' : ''}`)
          break
        }
        case 'stop':
          await invoke('stop_downloads')
          toast('Downloads paused')
          break
      }
    } catch (err) {
      toast(String(err), true)
    }
    await DL.refresh()
    render(true)
  }
  view.addEventListener('click', onClick)
  v.leave = () => view.removeEventListener('click', onClick)
  v.onDownloads = () => render()
  await DL.refresh()
  render(true)
}

// The "Updates" panel (Settings and About share it).
function updatePanel() {
  const u = Upd.info || {}
  const l = u.latest
  const when = u.checkedAt ? new Date(u.checkedAt * 1000).toLocaleString() : 'not yet'
  const state = u.selfBuilt
    ? `<span class="upd-state">Built from source: update it with git pull and a new build</span>`
    : u.installing
      ? `<span class="upd-state new">Downloading Tsukimi ${esc(l?.version)}… ${u.size ? `${fmtBytes(u.downloaded)} of ${fmtBytes(u.size)}` : ''}</span>`
      : u.checking
        ? `<span class="upd-state">Checking…</span>`
        : l
          ? `<span class="upd-state new">Tsukimi ${esc(l.version)} is available</span>`
          : u.error
            ? `<span class="upd-state bad" title="${esc(u.error)}">Couldn't check: ${esc(u.error)}</span>`
            : `<span class="upd-state">You have the latest version</span>`
  return `<div class="upd">
    <div class="upd-row">${state}<span class="grow"></span>
      <button class="btn small" data-upd="check" ${u.checking || u.installing ? 'disabled' : ''}><i class="ic ic-refresh"></i>Check for updates</button>
      ${l && !u.selfBuilt ? `<button class="btn small primary" data-upd="install" ${u.installing ? 'disabled' : ''}><i class="ic ic-up"></i>Update to ${esc(l.version)}</button>` : ''}</div>
    ${u.installing && u.size ? `<div class="meter"><i style="width:${Math.min(100, (u.downloaded / u.size) * 100)}%"></i></div>` : ''}
    ${l?.notes ? `<div class="upd-notes">${esc(l.notes)}</div>` : ''}
    <div class="foot"><span>Version ${esc(S.boot.version)}</span><span>· last checked ${esc(when)}</span>${
      l?.url ? `<span>· <a class="ext" data-href="${esc(l.url)}" href="${esc(l.url)}">What's new</a></span>` : ''
    }</div></div>`
}

// Wires an updates panel inside `root`; re-renders it when the update state changes.
function mountUpdatePanel(root, v) {
  const box = $('[data-upd-panel]', root)
  if (!box) return
  const draw = () => (box.innerHTML = updatePanel())
  draw()
  v.onUpdate = draw
  root.addEventListener('click', (e) => {
    const b = e.target.closest('[data-upd]')
    if (!b) return
    if (b.dataset.upd === 'check') Upd.check(true)
    else Upd.install()
  })
}

function aboutView() {
  setView({})
  const v = S.view
  const link = (url, text) => `<a data-href="${url}" href="${url}">${text || url.replace('https://', '')}</a>`
  view.innerHTML = `<div class="page fade-in" id="about">
    <div class="about-head">${$('#rail .logo').innerHTML}<div><h1>Tsukimi</h1><p>Version ${esc(S.boot.version)} · a fast desktop anime player for Windows, Linux and macOS · <b>Made by Akulav</b></p></div></div>
    <div class="panel"><h3>Updates</h3><div data-upd-panel></div></div>
    <div class="panel credits">
      <h3>Credits</h3>
      <p>Made by <b>Akulav</b>, inspired by <b>dango</b> by <b>serifpersia</b>, the original app this one is rebuilt from: ${link('https://github.com/serifpersia/dango')}</p>
      <p>Anime sources available on the dango provider registry, maintained by serifpersia: ${link('https://github.com/serifpersia/dango-providers')}</p>
      <dl>
        <dt>Source code</dt><dd>${link('https://github.com/Akulav/local-anime')}</dd>
        <dt>Metadata &amp; artwork</dt><dd>${link('https://anilist.co', 'AniList')}</dd>
        <dt>Intro &amp; outro times</dt><dd>${link('https://aniskip.com', 'AniSkip')}</dd>
        <dt>Built with</dt><dd>Rust, Tauri, QuickJS (rquickjs), hls.js, cheerio</dd>
        <dt>License</dt><dd>Akulav Source-Available License. Original dango © 2025 serifpersia (MIT), this version © 2026 Akulav.</dd>
      </dl>
    </div>
    <div class="panel credits"><h3>Note</h3><p>Tsukimi doesn't host any video. It plays streams that the providers in the registry find on third-party sites.</p></div>
  </div>`
  mountUpdatePanel($('#about'), v)
}

async function settingsView() {
  setView({})
  const v = S.view
  const s = (S.settings = await invoke('get_settings'))
  const [providers, stats] = await Promise.all([invoke('providers'), invoke('cache_stats')])
  if (S.view !== v) return
  const pct = Math.min(100, (stats.bytes / (s.cacheLimitMb * 1024 * 1024)) * 100)
  const sw = (key, label, hint) => `<div class="field"><div class="l"><b>${label}</b><span>${hint}</span></div>
      <label class="switch"><input type="checkbox" data-key="${key}" ${s[key] ? 'checked' : ''}><span></span></label></div>`
  const num = (key, label, hint, min, max) => `<div class="field"><div class="l"><b>${label}</b><span>${hint}</span></div>
      <input type="number" data-key="${key}" value="${s[key]}" min="${min}" max="${max}"></div>`
  const txt = (key, label, hint, ph = '') => `<div class="field"><div class="l"><b>${label}</b><span>${hint}</span></div>
      <input type="text" data-key="${key}" value="${esc(s[key] ?? '')}" placeholder="${esc(ph)}" spellcheck="false"></div>`
  view.innerHTML = `<div class="settings fade-in" id="settings">
    <h1>Settings</h1>
    <div class="panel">
      <h3>Playback</h3>
      <div class="field"><div class="l"><b>Audio</b><span>Prefer subtitles or dubbed audio</span></div>
        <select data-key="mode"><option value="sub" ${s.mode === 'sub' ? 'selected' : ''}>Subbed</option><option value="dub" ${s.mode === 'dub' ? 'selected' : ''}>Dubbed</option></select></div>
      ${sw('autoplayNext', 'Autoplay next episode', 'Continue automatically when an episode ends')}
      ${sw('autoSkipIntro', 'Skip intros automatically', 'Uses community timestamps from aniskip')}
      ${sw('showMature', 'Show mature content', 'Include 18+ titles and providers')}
      <div class="field"><div class="l"><b>Preferred provider</b><span>Ranked first whenever it works</span></div>
        <select data-key="preferredProvider"><option value="">Automatic</option>${providers
          .map((p) => `<option value="${esc(p.id)}" ${s.preferredProvider === p.id ? 'selected' : ''}>${esc(p.label)}</option>`)
          .join('')}</select></div>
    </div>
    <div class="panel">
      <h3>Downloads</h3>
      <div class="field"><div class="l"><b>Downloaded episodes</b><span>Kept until you delete them (Downloads page). Cache cleanup and "Clear" never touch them.</span></div>
        <span class="chip">${fmtBytes(stats.downloadBytes)}</span></div>
      ${num('predownloadEpisodes', 'Episodes to pre-download', 'Opening a show caches this many episodes from where you left off (0 = off)', 0, 12)}
      <div style="margin-top:10px" class="foot"><i class="ic ic-folder" style="width:14px;height:14px"></i><span class="path">${esc(S.boot.paths.downloads)}</span></div>
    </div>
    <div class="panel">
      <h3>Buffering &amp; cache</h3>
      ${sw('downloadAhead', 'Download whole episode while playing', 'Segments are saved to disk ahead of the playhead, seeking is instant')}
      ${num('downloadConcurrency', 'Parallel downloads', 'Segments fetched at the same time', 1, 12)}
      ${sw('prefetchNextEpisode', 'Prepare next episode', 'Resolve the next episode near the end of the current one')}
      ${num('cacheLimitMb', 'Cache size limit (MB)', 'Oldest cached files are removed above this size (downloads don’t count)', 256, 524288)}
      ${num('cacheMaxAgeHours', 'Remove cached episodes after (hours)', 'Episodes you watched or that were pre-downloaded are removed this long after their last use', 1, 2160)}
      <div class="meter"><i style="width:${pct}%"></i></div>
      <div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap"><span class="chip">${fmtBytes(stats.bytes)} used · ${stats.files} files</span>
        <span class="chip">video ${fmtBytes(stats.segmentBytes + stats.fileBytes)}</span><span class="chip">images ${fmtBytes(stats.imageBytes)}</span><span class="grow"></span>
        <button class="btn small" data-cache="sweep">Clean up now</button><button class="btn small" data-cache="video" title="Downloads are kept">Clear video cache</button><button class="btn small" data-cache="all" title="Downloads are kept">Clear everything</button></div>
      <div style="margin-top:10px" class="foot">Cache: <span class="path">${esc(S.boot.paths.cache)}</span></div>
    </div>
    <div class="panel">
      <h3>Updates</h3>
      ${sw('checkUpdates', 'Check for updates automatically', 'Shortly after start and every 6 hours. An Update button appears in the sidebar when there is one')}
      <div style="margin-top:12px" data-upd-panel></div>
    </div>
    <div class="panel">
      <h3>Providers <span class="grow"></span><button class="btn small" id="prov-refresh"><i class="ic ic-refresh"></i>Update</button></h3>
      <table class="prov-table">${providers
        .map(
          (p) => `<tr><td><span class="dot ${p.loaded ? 'ok' : 'failed'}" style="display:inline-block"></span></td><td><b>${esc(p.label)}</b> <small style="color:var(--dim)">v${esc(p.version)}${p.mature ? ' · 18+' : ''}</small></td>
          <td class="e" title="${esc(p.error || '')}">${esc(p.error || [p.sub && `${p.sub} subs`, p.tier].filter(Boolean).join(' · '))}</td>
          <td style="text-align:right"><label class="switch"><input type="checkbox" data-prov="${esc(p.id)}" ${s.disabledProviders.includes(p.id) ? '' : 'checked'}><span></span></label></td></tr>`
        )
        .join('')}</table>
      ${txt('registryUrl', 'Registry URL', 'Leave empty for the official provider registry', 'https://raw.githubusercontent.com/serifpersia/dango-providers/main/registry.json')}
      ${txt('cfClearance', 'Cloudflare cookie (cf_clearance)', 'Only needed for providers behind Cloudflare, e.g. AnimePahe')}
      ${txt('cfUserAgent', 'Browser user-agent for that cookie', 'Must match the browser the cookie came from')}
    </div>
    <div class="foot">Tsukimi ${esc(S.boot.version)} · made by Akulav, inspired by dango by serifpersia · <a class="ext" href="#/about">About &amp; credits</a> · data: <span class="path">${esc(S.boot.paths.data)}</span></div>
  </div>`

  // (listeners go on the page's own element, which is replaced on every visit: none pile up on #view)
  const root = $('#settings')
  mountUpdatePanel(root, v)
  const save = debounce(async () => {
    try {
      S.settings = await invoke('set_settings', { settings: S.settings })
      toast('Saved', false, 1200)
    } catch (e) {
      toast(String(e), true)
    }
  }, 350)
  root.addEventListener('change', (e) => {
    const el = e.target
    if (el.dataset.key) {
      const k = el.dataset.key
      let val = el.type === 'checkbox' ? el.checked : el.type === 'number' ? Number(el.value) : el.value
      if ((k === 'preferredProvider' || k === 'registryUrl' || k === 'cfClearance' || k === 'cfUserAgent') && !val) val = null
      S.settings = { ...S.settings, [k]: val }
      save()
    } else if (el.dataset.prov) {
      const id = el.dataset.prov
      const set = new Set(S.settings.disabledProviders)
      el.checked ? set.delete(id) : set.add(id)
      S.settings = { ...S.settings, disabledProviders: [...set] }
      S.probes.clear()
      save()
    }
  })
  $('#prov-refresh').addEventListener('click', async (e) => {
    const b = e.currentTarget
    b.disabled = true
    try {
      await invoke('refresh_providers')
      toast('Providers updated')
      if (S.view === v) settingsView()
    } catch (err) {
      toast(String(err), true)
      b.disabled = false
    }
  })
  root.addEventListener('click', async (e) => {
    const b = e.target.closest('[data-cache]')
    if (!b) return
    const kind = b.dataset.cache
    b.disabled = true
    try {
      const r = kind === 'sweep' ? await invoke('sweep_cache') : await invoke('clear_cache', { images: kind === 'all' })
      toast(`Removed ${r.removed} files (${fmtBytes(r.freed)})`)
    } catch (err) {
      toast(String(err), true)
    }
    if (S.view === v) settingsView()
  })
}

// ---------------------------------------------------------------- router
async function route() {
  const hash = location.hash.slice(1) || '/'
  const [path, qs] = hash.split('?')
  const params = new URLSearchParams(qs || '')
  for (const a of $$('#rail a[data-nav]')) a.classList.remove('on')
  const nav = (n) => $(`#rail a[data-nav="${n}"]`)?.classList.add('on')
  try {
    if (path === '/' || path === '') {
      nav('home')
      await homeView()
    } else if (path === '/search') {
      nav('search')
      await searchView(params.get('q') || '')
    } else if (path.startsWith('/anime/')) {
      await showView(Number(path.split('/')[2]))
    } else if (path === '/library') {
      nav('library')
      await libraryView()
    } else if (path === '/downloads') {
      nav('downloads')
      await downloadsView()
    } else if (path === '/settings') {
      nav('settings')
      await settingsView()
    } else if (path === '/about') {
      nav('about')
      aboutView()
    }
  } catch (e) {
    console.error(e)
    toast(String(e), true)
  }
}
window.addEventListener('hashchange', route)

const q = $('#q')
const doSearch = debounce(() => {
  const v = q.value.trim()
  const target = `#/search?q=${encodeURIComponent(v)}`
  if (location.hash.startsWith('#/search')) history.replaceState(null, '', target)
  else history.pushState(null, '', target)
  route()
}, 220)
q.addEventListener('input', doSearch)
q.addEventListener('keydown', (e) => {
  if (e.key === 'Enter') {
    e.preventDefault()
    $('.card', view)?.focus()
  }
  if (e.key === 'Escape') q.blur()
})

view.addEventListener('click', (e) => {
  const play = e.target.closest('[data-play]')
  if (play) {
    e.preventDefault()
    getDetails(Number(play.dataset.play)).then((m) => Player.play(m, '1', {}))
    return
  }
  const resume = e.target.closest('[data-resume]')
  if (resume && !e.ctrlKey) {
    e.preventDefault()
    getDetails(Number(resume.dataset.resume)).then((m) => Player.play(m, resume.dataset.ep, {}))
  }
})

// ---------------------------------------------------------------- player
const video = $('#video')
const embed = $('#embed')
const playerEl = $('#player')

const Player = {
  open: false,
  hls: null,
  media: null,
  id: 0,
  episode: '1',
  episodes: [],
  provider: null,
  sources: [],
  srcIdx: 0,
  qIdx: 0,
  failed: new Set(),
  skip: null,
  prefetched: false,
  saveTimer: 0,
  idleTimer: 0,
  refreshed: false,
  token: 0,
  subIdx: -1,

  async play(media, episode, { provider, episodes } = {}) {
    const token = ++this.token
    this.media = media
    this.id = media.id
    this.episode = String(episode)
    this.episodes = episodes?.length ? episodes : this.episodes?.length && this.id === media.id ? this.episodes : []
    this.failed = new Set()
    this.refreshed = false
    this.prefetched = false
    this.skip = null
    this.show()
    this.teardownMedia()
    $('#p-show').textContent = titleOf(media)
    $('#p-ep').textContent = `Episode ${this.episode}`
    $('#p-srcinfo').textContent = ''
    this.message('')
    this.spinner(true)
    this.updateDisk()

    // A downloaded copy plays straight from disk; if it fails, failover asks the provider.
    const local = await invoke('local_source', { id: this.id, episode: this.episode }).catch(() => null)
    if (token !== this.token) return
    if (local) return this.start(local.provider, [{ ...local, local: true }])

    const snap = await this.waitForSources(token)
    if (token !== this.token) return
    const order = this.providerOrder(snap, provider)
    if (!order.length) {
      this.spinner(false)
      this.message('No working source found for this show. Try switching SUB/DUB or enabling more providers in Settings.')
      return
    }
    for (const p of order) {
      if (token !== this.token) return
      if (await this.tryProvider(p, token)) return
    }
    if (token !== this.token) return
    this.spinner(false)
    this.message(`Couldn't load episode ${this.episode} from any provider.`)
  },

  async waitForSources(token) {
    let snap = S.probes.get(this.id)
    if (!snap || (!snap.best && !snap.done)) {
      snap = await invoke('probe', { id: this.id }).catch(() => null)
      if (snap) S.probes.set(this.id, snap)
    }
    const started = Date.now()
    while (token === this.token && !(snap?.best || snap?.done) && Date.now() - started < 45000) {
      this.message('Finding a working source…', true)
      await new Promise((r) => setTimeout(r, 300))
      snap = S.probes.get(this.id)
    }
    this.message('')
    return snap
  },

  providerOrder(snap, preferred) {
    // Sources are already ranked; ones whose stream check failed stay as a last resort.
    const ok = (snap?.sources || []).filter((s) => s.state === 'ok' && s.playable !== false).map((s) => s.provider)
    const doubtful = (snap?.sources || []).filter((s) => s.state === 'ok' && s.playable === false).map((s) => s.provider)
    const order = [...ok, ...doubtful]
    if (preferred && order.includes(preferred)) order.splice(order.indexOf(preferred), 1), order.unshift(preferred)
    return order.filter((p) => !this.failed.has(p))
  },

  async tryProvider(provider, token, refresh = false) {
    this.message(`Loading from ${provider}…`, true)
    this.refreshed = refresh
    let sources
    try {
      sources = await invoke('streams', { id: this.id, provider, episode: this.episode, refresh })
    } catch (e) {
      console.warn(provider, e)
      this.failed.add(provider)
      return false
    }
    if (token !== this.token) return true
    // Native players first, embeds last.
    sources.sort((a, b) => (a.kind === 'iframe') - (b.kind === 'iframe'))
    this.start(provider, sources)
    return true
  },

  start(provider, sources) {
    this.provider = provider
    this.sources = sources
    store.set(`provider:${this.id}`, provider)
    if (!this.episodes.length) {
      invoke('episodes', { id: this.id, provider })
        .then((e) => (this.episodes = e))
        .catch(() => {})
    }
    this.message('')
    const resume = (this.media.progress || []).find((p) => String(p.episode) === this.episode)
    const start = resume && resume.duration && resume.position / resume.duration < 0.95 ? resume.position : 0
    this.load(0, 0, start)
  },

  // Saves the current episode to disk (kept until deleted).
  download() {
    const d = DL.forShow(this.id).get(this.episode)
    if (d?.pinned && d.state === 'done') return toast(`EP ${this.episode} is downloaded`)
    saveEpisodes(this.media, this.provider, [this.episode])
  },

  load(srcIdx, qIdx, startAt = 0) {
    this.srcIdx = srcIdx
    this.qIdx = qIdx
    const src = this.sources[srcIdx]
    const q = src.qualities[qIdx]
    this.teardownMedia()
    $('#p-srcinfo').innerHTML = `${esc(this.provider)} · ${esc(src.name)}<br>${esc(q.label)}`
    $('[data-menu="quality"]').textContent = q.label
    playerEl.classList.toggle('embed', src.kind === 'iframe')
    this.spinner(src.kind !== 'iframe')
    if (src.kind === 'iframe') {
      video.classList.add('hidden')
      embed.classList.remove('hidden')
      embed.src = q.url
      return
    }
    embed.classList.add('hidden')
    video.classList.remove('hidden')
    // Remember what is playing so the episode shows in Downloads and replays from disk.
    if (!src.local) {
      const m = this.media
      invoke('track_playback', {
        id: this.id,
        provider: this.provider,
        episode: this.episode,
        url: q.url,
        subtitles: src.subtitles,
        title: titleOf(m),
        cover: coverOf(m),
      }).catch(() => {})
    }
    for (const t of src.subtitles) {
      const tr = document.createElement('track')
      tr.kind = 'subtitles'
      tr.label = t.label
      tr.srclang = (t.lang || 'en').slice(0, 2)
      tr.src = t.url
      video.append(tr)
    }
    const pickSub = () => {
      const tracks = [...video.textTracks]
      if (!tracks.length) return
      let idx = this.subIdx >= 0 && this.subIdx < tracks.length ? this.subIdx : tracks.findIndex((t) => /^en|english/i.test(t.label || t.language))
      tracks.forEach((t, i) => (t.mode = i === idx ? 'showing' : 'disabled'))
      this.subIdx = idx
    }
    if (src.kind === 'hls' && window.Hls && Hls.isSupported()) {
      const hls = new Hls({
        enableWorker: true,
        startPosition: startAt || -1,
        maxBufferLength: 90,
        maxMaxBufferLength: 600,
        backBufferLength: 120,
        fragLoadingMaxRetry: 4,
        manifestLoadingMaxRetry: 2,
        levelLoadingMaxRetry: 3,
        capLevelToPlayerSize: false,
      })
      this.hls = hls
      let recovered = false
      hls.on(Hls.Events.ERROR, (_, data) => {
        if (!data.fatal) return
        if (data.type === Hls.ErrorTypes.MEDIA_ERROR && !recovered) {
          recovered = true
          hls.recoverMediaError()
        } else this.failover(`${data.type}: ${data.details}`)
      })
      hls.on(Hls.Events.MANIFEST_PARSED, (_, data) => {
        // Pin the best variant: it is what background downloads fetch, and with the disk
        // cache there is no need for adaptive bitrate.
        if (data.levels.length > 1) hls.currentLevel = data.levels.length - 1
        video.play().catch(() => {})
        pickSub()
      })
      hls.loadSource(q.url)
      hls.attachMedia(video)
    } else {
      video.src = q.url
      video.addEventListener(
        'loadedmetadata',
        () => {
          if (startAt) video.currentTime = startAt
          video.play().catch(() => {})
          pickSub()
        },
        { once: true }
      )
    }
  },

  failover(reason) {
    console.warn('playback failed:', reason)
    const src = this.sources[this.srcIdx]
    const pos = video.currentTime || 0
    if (src && this.qIdx + 1 < src.qualities.length) {
      toast(`Quality ${src.qualities[this.qIdx].label} failed, trying ${src.qualities[this.qIdx + 1].label}`)
      return this.load(this.srcIdx, this.qIdx + 1, pos)
    }
    if (this.srcIdx + 1 < this.sources.length) {
      toast(`Switching to ${this.sources[this.srcIdx + 1].name}`)
      return this.load(this.srcIdx + 1, 0, pos)
    }
    // Cached stream URLs may have expired: ask the provider once more before moving on.
    if (!this.refreshed) {
      const token = ++this.token
      this.tryProvider(this.provider, token, true).then((ok) => !ok && this.failover('refresh failed'))
      return
    }
    this.failed.add(this.provider)
    const next = this.providerOrder(S.probes.get(this.id))[0]
    if (next) {
      toast(`${this.provider} failed, switching to ${next}`)
      const token = ++this.token
      this.tryProvider(next, token).then((ok) => !ok && this.failover('provider failed'))
      return
    }
    this.spinner(false)
    this.message('All sources failed for this episode.')
  },

  teardownMedia() {
    if (this.hls) {
      this.hls.destroy()
      this.hls = null
    }
    video.pause()
    video.removeAttribute('src')
    for (const t of $$('track', video)) t.remove()
    video.load()
    embed.src = 'about:blank'
  },

  show() {
    if (this.open) return
    this.open = true
    playerEl.classList.remove('hidden')
    playerEl.focus()
    this.saveTimer = setInterval(() => this.save(), 5000)
    this.poke()
  },

  async close() {
    if (!this.open) return
    this.token++
    await this.save()
    this.open = false
    clearInterval(this.saveTimer)
    this.teardownMedia()
    playerEl.classList.add('hidden')
    $('#menu').classList.add('hidden')
    $('#drawer').classList.add('hidden')
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {})
    // Downloads keep running in the background; the janitor expires them later.
    // Refresh progress on the page underneath.
    S.details.delete(this.id)
    if (S.view?.id === this.id) showView(this.id)
    else if (location.hash === '' || location.hash === '#/') homeView()
  },

  async save() {
    if (!this.open || !video.duration || !isFinite(video.duration) || video.currentTime < 5) return
    const progress = {
      anilistId: this.id,
      episode: this.episode,
      position: video.currentTime,
      duration: video.duration,
      updatedAt: 0,
      title: titleOf(this.media),
      cover: coverOf(this.media),
      provider: this.provider,
    }
    const list = (this.media.progress ||= [])
    const i = list.findIndex((p) => String(p.episode) === this.episode)
    if (i >= 0) list[i] = progress
    else list.push(progress)
    await invoke('save_progress', { progress }).catch(() => {})
  },

  // How much of this episode is on disk (the green part of the seek bar).
  updateDisk() {
    const d = this.open ? DL.forShow(this.id).get(this.episode) : null
    $('[data-act="download"]').classList.toggle('on', !!d?.pinned)
    if (!d || !d.total || this.sources[this.srcIdx]?.kind === 'iframe') {
      $('#disk-note').textContent = ''
      $('#bar-disk').style.width = '0'
      return
    }
    const pct = DL.pct(d)
    $('#bar-disk').style.width = `${pct}%`
    $('#disk-note').innerHTML = `<i class="ic ic-disk" style="width:14px;height:14px"></i>${
      d.state === 'done' ? (d.pinned ? 'downloaded' : 'on disk') : `${pct.toFixed(0)}% on disk`
    } · ${fmtBytes(d.bytes)}`
  },

  epIndex() {
    return this.episodes.findIndex((e) => String(e.number) === this.episode)
  },

  go(delta) {
    const i = this.epIndex()
    const next = this.episodes[i + delta]
    if (next) this.play(this.media, next.number, { provider: this.provider, episodes: this.episodes })
    else toast(delta > 0 ? 'This is the latest episode' : 'This is the first episode')
  },

  onProbe() {
    if (!$('#menu').classList.contains('hidden') && $('#menu').dataset.kind === 'sources') this.menu('sources')
  },

  spinner(on) {
    $('#p-spinner').classList.toggle('hidden', !on)
  },

  message(text, spinner = false) {
    const el = $('#p-msg')
    el.textContent = text
    el.classList.toggle('hidden', !text || spinner)
    if (spinner) this.spinner(true)
  },

  poke() {
    playerEl.classList.remove('idle')
    clearTimeout(this.idleTimer)
    this.idleTimer = setTimeout(() => {
      if (!video.paused && $('#menu').classList.contains('hidden') && $('#drawer').classList.contains('hidden')) playerEl.classList.add('idle')
    }, 2600)
  },

  menu(kind) {
    const m = $('#menu')
    if (!m.classList.contains('hidden') && m.dataset.kind === kind && !this._refresh) {
      m.classList.add('hidden')
      return
    }
    m.dataset.kind = kind
    const src = this.sources[this.srcIdx]
    let html = ''
    if (kind === 'quality' && src) {
      html = `<div class="mh">Quality</div>${src.qualities
        .map((q, i) => `<button data-q="${i}" class="${i === this.qIdx ? 'on' : ''}">${esc(q.label)}</button>`)
        .join('')}`
    } else if (kind === 'sources') {
      const snap = S.probes.get(this.id)
      html = `<div class="mh">${esc(this.provider)} streams</div>${this.sources
        .map((s, i) => `<button data-s="${i}" class="${i === this.srcIdx ? 'on' : ''}">${esc(s.name)}<small>${esc(s.kind)}</small></button>`)
        .join('')}<div class="mh">Providers</div>${(snap?.sources || [])
        .filter((s) => s.state === 'ok')
        .map(
          (s) =>
            `<button data-p="${esc(s.provider)}" class="${s.provider === this.provider ? 'on' : ''}"><span class="dot ${s.playable === false ? 'failed' : 'ok'}"></span>${esc(s.label)}<small>${s.latencyMs ?? ''} ms</small></button>`
        )
        .join('')}`
    } else if (kind === 'subs') {
      const tracks = [...video.textTracks]
      html = `<div class="mh">Subtitles</div><button data-sub="-1" class="${this.subIdx < 0 ? 'on' : ''}">Off</button>${tracks
        .map((t, i) => `<button data-sub="${i}" class="${i === this.subIdx ? 'on' : ''}">${esc(t.label || t.language || `Track ${i + 1}`)}</button>`)
        .join('')}${tracks.length ? '' : '<div class="mh">None available (may be burned in)</div>'}`
    } else if (kind === 'speed') {
      html = `<div class="mh">Speed</div>${[0.5, 0.75, 1, 1.25, 1.5, 2]
        .map((r) => `<button data-rate="${r}" class="${video.playbackRate === r ? 'on' : ''}">${r}×</button>`)
        .join('')}`
    }
    m.innerHTML = html
    m.classList.remove('hidden')
  },

  drawer() {
    const d = $('#drawer')
    if (!d.classList.contains('hidden')) return d.classList.add('hidden')
    const prog = new Map((this.media.progress || []).map((p) => [String(p.episode), p]))
    d.innerHTML = `<h3>Episodes</h3><div class="eps">${this.episodes
      .map((e) => {
        const p = prog.get(String(e.number))
        const pct = p && p.duration ? Math.min(100, (p.position / p.duration) * 100) : 0
        return `<div class="ep ${String(e.number) === this.episode ? 'current' : ''} ${pct > 90 ? 'watched' : ''}" data-ep="${esc(e.number)}" style="grid-template-columns:48px 1fr">
        <div class="th">${esc(e.number)}</div><div><div class="tt">${esc(e.title || `Episode ${e.number}`)}</div></div>${pct ? `<span class="bar" style="width:${pct}%"></span>` : ''}</div>`
      })
      .join('')}</div>`
    d.classList.remove('hidden')
    $('.ep.current', d)?.scrollIntoView({ block: 'center' })
  },

  async fullscreen() {
    try {
      const w = T.window.getCurrentWindow()
      await w.setFullscreen(!(await w.isFullscreen()))
    } catch {
      if (document.fullscreenElement) document.exitFullscreen()
      else playerEl.requestFullscreen().catch(() => {})
    }
  },

  async loadSkip() {
    this.skip = null
    const mal = this.media.idMal
    if (!mal || !video.duration) return
    try {
      const r = await invoke('skip_times', { malId: mal, episode: this.episode, duration: video.duration })
      this.skip = (r.results || []).map((x) => ({ type: x.skipType, start: x.interval.startTime, end: x.interval.endTime }))
    } catch {}
  },
}

// player wiring
const playIcon = $('#ic-play')
video.addEventListener('play', () => (playIcon.className = 'ic ic-pause'))
video.addEventListener('pause', () => {
  playIcon.className = 'ic ic-play'
  Player.poke()
  Player.save()
})
video.addEventListener('waiting', () => Player.spinner(true))
video.addEventListener('playing', () => Player.spinner(false))
video.addEventListener('canplay', () => Player.spinner(false))
video.addEventListener('loadedmetadata', () => Player.loadSkip())
video.addEventListener('error', () => {
  if (Player.open && !Player.hls && video.getAttribute('src')) Player.failover(video.error?.message || 'media error')
})
video.addEventListener('ended', () => {
  Player.save()
  if (S.settings.autoplayNext) Player.go(1)
})
video.addEventListener('timeupdate', () => {
  const d = video.duration || 0
  const t = video.currentTime
  $('#t-cur').textContent = fmtTime(t)
  $('#t-dur').textContent = fmtTime(d)
  const pct = d ? (t / d) * 100 : 0
  $('#bar-played').style.width = `${pct}%`
  $('#knob').style.left = `${pct}%`
  if (video.buffered.length && d) {
    let end = 0
    for (let i = 0; i < video.buffered.length; i++) if (video.buffered.start(i) <= t + 1) end = Math.max(end, video.buffered.end(i))
    $('#bar-buf').style.width = `${(end / d) * 100}%`
  }
  const seg = Player.skip?.find((s) => t >= s.start && t < s.end - 1)
  const btn = $('#p-skip')
  if (seg && S.settings.autoSkipIntro && seg.type === 'op') video.currentTime = seg.end
  btn.classList.toggle('hidden', !seg)
  if (seg) btn.textContent = seg.type === 'ed' ? 'Skip outro' : 'Skip intro'
  btn.onclick = seg ? () => (video.currentTime = seg.end) : null
  if (!Player.prefetched && d > 60 && t > d * 0.8) {
    Player.prefetched = true
    const next = Player.episodes[Player.epIndex() + 1]
    if (next) invoke('prefetch_next', { id: Player.id, provider: Player.provider, episode: String(next.number) }).catch(() => {})
  }
})

const volume = $('#volume')
volume.value = store.get('volume', 1)
video.volume = Number(volume.value)
volume.addEventListener('input', () => {
  video.volume = Number(volume.value)
  video.muted = false
  store.set('volume', video.volume)
})
video.addEventListener('volumechange', () => ($('#ic-vol').className = `ic ${video.muted || video.volume === 0 ? 'ic-mute' : 'ic-vol'}`))

const seek = $('#seek')
const seekAt = (e) => {
  const r = seek.getBoundingClientRect()
  return Math.max(0, Math.min(1, (e.clientX - r.left) / r.width))
}
seek.addEventListener('mousemove', (e) => {
  const f = seekAt(e)
  $('#seek-tip').style.left = `${f * 100}%`
  $('#seek-tip').textContent = fmtTime(f * (video.duration || 0))
})
seek.addEventListener('mousedown', (e) => {
  const move = (ev) => video.duration && (video.currentTime = seekAt(ev) * video.duration)
  move(e)
  const up = () => (window.removeEventListener('mousemove', move), window.removeEventListener('mouseup', up))
  window.addEventListener('mousemove', move)
  window.addEventListener('mouseup', up)
})

playerEl.addEventListener('mousemove', () => Player.poke())
playerEl.addEventListener('click', (e) => {
  const act = e.target.closest('[data-act]')?.dataset.act
  const menu = e.target.closest('[data-menu]')?.dataset.menu
  const m = $('#menu')
  if (menu) return Player.menu(menu)
  const item = e.target.closest('#menu button')
  if (item) {
    const d = item.dataset
    if (d.q != null) Player.load(Player.srcIdx, Number(d.q), video.currentTime)
    else if (d.s != null) Player.load(Number(d.s), 0, video.currentTime)
    else if (d.p) {
      const token = ++Player.token
      Player.failed.delete(d.p)
      Player.tryProvider(d.p, token)
    } else if (d.sub != null) {
      Player.subIdx = Number(d.sub)
      ;[...video.textTracks].forEach((t, i) => (t.mode = i === Player.subIdx ? 'showing' : 'disabled'))
    } else if (d.rate) {
      video.playbackRate = Number(d.rate)
      $('[data-menu="speed"]').textContent = `${d.rate}×`
    }
    m.classList.add('hidden')
    return
  }
  const ep = e.target.closest('#drawer .ep')
  if (ep) return Player.play(Player.media, ep.dataset.ep, { provider: Player.provider, episodes: Player.episodes })
  if (!e.target.closest('.menu')) m.classList.add('hidden')
  switch (act) {
    case 'close':
      return Player.close()
    case 'play':
      return video.paused ? video.play() : video.pause()
    case 'next':
      return Player.go(1)
    case 'prev':
      return Player.go(-1)
    case 'mute':
      video.muted = !video.muted
      return
    case 'fullscreen':
      return Player.fullscreen()
    case 'episodes':
      return Player.drawer()
    case 'download':
      return Player.download()
  }
  if (e.target === video) video.paused ? video.play() : video.pause()
})
video.addEventListener('dblclick', () => Player.fullscreen())

// ---------------------------------------------------------------- keyboard
document.addEventListener('keydown', (e) => {
  const typing = ['INPUT', 'TEXTAREA', 'SELECT'].includes(document.activeElement?.tagName)
  if (Player.open) {
    if (typing && e.key !== 'Escape') return
    Player.poke()
    const k = e.key.toLowerCase()
    const handled = {
      ' ': () => (video.paused ? video.play() : video.pause()),
      k: () => (video.paused ? video.play() : video.pause()),
      arrowright: () => (video.currentTime += e.shiftKey ? 30 : 5),
      arrowleft: () => (video.currentTime -= e.shiftKey ? 30 : 5),
      l: () => (video.currentTime += 10),
      j: () => (video.currentTime -= 10),
      arrowup: () => ((video.volume = Math.min(1, video.volume + 0.05)), (volume.value = video.volume)),
      arrowdown: () => ((video.volume = Math.max(0, video.volume - 0.05)), (volume.value = video.volume)),
      m: () => (video.muted = !video.muted),
      f: () => Player.fullscreen(),
      n: () => Player.go(1),
      p: () => Player.go(-1),
      e: () => Player.drawer(),
      c: () => Player.menu('subs'),
      s: () => $('#p-skip').click(),
      d: () => Player.download(),
      escape: async () => {
        if (!$('#menu').classList.contains('hidden')) return $('#menu').classList.add('hidden')
        if (!$('#drawer').classList.contains('hidden')) return $('#drawer').classList.add('hidden')
        try {
          const w = T.window.getCurrentWindow()
          if (await w.isFullscreen()) return w.setFullscreen(false)
        } catch {}
        Player.close()
      },
    }[k]
    if (handled) {
      e.preventDefault()
      handled()
    } else if (/^[0-9]$/.test(k) && video.duration) {
      video.currentTime = (Number(k) / 10) * video.duration
    }
    return
  }
  if (typing) return
  if (e.key === '/' || (e.key === 'k' && e.ctrlKey)) {
    e.preventDefault()
    q.focus()
    q.select()
  } else if (e.key === 'Backspace' && location.hash.startsWith('#/anime/')) {
    history.back()
  }
})

// ---------------------------------------------------------------- boot
;(async () => {
  try {
    S.boot = await invoke('boot')
    S.settings = S.boot.settings
  } catch (e) {
    document.body.innerHTML = `<div class="empty">Failed to start: ${esc(e)}</div>`
    return
  }
  Upd.set(S.boot.update)
  route()
  pollDownloads()
})()
