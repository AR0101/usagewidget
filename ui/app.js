/* The panel, ported from PanelView.swift.
 *
 * Rust owns the data and the timers and pushes a `stats` event; this file only
 * draws. The one-second tick here is for the clock and the "3분 전" label, which
 * would otherwise sit still between refreshes and look frozen. */

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const appWindow = window.__TAURI__.window.getCurrentWindow();

const panel = document.getElementById('panel');
const grip = document.querySelector('.grip');

let prefs = null;
let stats = null;
let contextLimit = 1_000_000;
let source = null;
let now = Date.now() / 1000;

// MARK: - formatting (Fmt in the Swift build)

/** 184_800_000 -> "184.8M". Keeps the widget readable at a glance. */
function tokens(n) {
  if (n < 1e3) return String(n);
  if (n < 1e6) return (n / 1e3).toFixed(1) + 'K';
  if (n < 1e9) return (n / 1e6).toFixed(1) + 'M';
  return (n / 1e9).toFixed(2) + 'B';
}

/** "2시간 12분 후" — coarse by design; the widget is glanced at, not read. */
function until(epoch) {
  const s = Math.floor(epoch - now);
  if (s <= 0) return '곧 리셋';
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  if (d > 0) return h > 0 ? `${d}일 ${h}시간 후` : `${d}일 후`;
  if (h > 0) return m > 0 ? `${h}시간 ${m}분 후` : `${h}시간 후`;
  return `${Math.max(m, 1)}분 후`;
}

function ago(epoch) {
  const s = Math.floor(now - epoch);
  if (s < 60) return '방금';
  if (s < 3600) return `${Math.floor(s / 60)}분 전`;
  if (s < 86400) return `${Math.floor(s / 3600)}시간 전`;
  return `${Math.floor(s / 86400)}일 전`;
}

const clockFmt = new Intl.DateTimeFormat('ko-KR', { hour: '2-digit', minute: '2-digit', hour12: false });
const stampFmt = new Intl.DateTimeFormat('ko-KR', { month: 'numeric', day: 'numeric', hour: '2-digit', minute: '2-digit', hour12: false });

const total = (b) => b.input + b.output + b.cacheWrite + b.cacheRead;
const fresh = (b) => b.input + b.output + b.cacheWrite;
const cacheShare = (b) => (total(b) > 0 ? b.cacheRead / total(b) : 0);

// MARK: - tiny DOM helpers

function el(tag, cls, text) {
  const n = document.createElement(tag);
  if (cls) n.className = cls;
  if (text !== undefined) n.textContent = text;
  return n;
}

function add(parent, ...kids) {
  for (const k of kids) if (k) parent.appendChild(k);
  return parent;
}

/** Mirrors LimitWindow.severity in the Swift build. */
function severityColour(percent, accent) {
  if (percent < 50) return accent;
  if (percent < 80) return 'var(--warn)';
  return 'var(--critical)';
}

/** Tint the context bar by how close the session is to auto-compaction. */
function contextColour(share) {
  if (share < 0.5) return 'var(--context)';
  if (share < 0.75) return 'var(--warn)';
  return 'var(--critical)';
}

function miniBar(ratio, colour) {
  const wrap = el('div', 'minibar');
  const fill = el('span');
  fill.style.setProperty('--bar', colour);
  // Zero draws as an empty track, not a stub, so an idle session is visibly
  // different from one that has barely started.
  fill.style.width = ratio > 0 ? `max(2px, ${Math.min(ratio, 1) * 100}%)` : '0';
  return add(wrap, fill);
}

// MARK: - sections

function titleBar() {
  const bar = el('div', 'titlebar');
  const dots = el('div', 'grabber');
  for (let i = 0; i < 3; i++) dots.appendChild(el('i'));
  const row = el('div', 'titlerow');
  add(row, el('span', 'brand', 'USAGE'), el('span', 'clock', clockFmt.format(now * 1000)));
  return add(bar, dots, row);
}

/** Claude Code only rewrites its cached limits when you run `/usage`, so the
 *  panel says how old they are rather than pretending they are live. */
function staleNote() {
  const c = stats.claude;
  if (c.liveError) return `한도 캐시 · ${c.liveError}`;
  if (c.limitsAreLive) return null;
  if (!c.limitsFetchedAt) return null;
  if (now - c.limitsFetchedAt <= 2 * 3600) return null;
  return `한도 ${ago(c.limitsFetchedAt)} 기준`;
}

function footBar(showClock) {
  const bar = el('div', 'footbar');
  if (showClock) add(bar, el('span', null, clockFmt.format(now * 1000)), el('span', null, '·'));
  add(bar, el('span', null, stats.generatedAt ? `${ago(stats.generatedAt)} 갱신` : '읽는 중…'));
  add(bar, el('span', 'spacer'));
  add(bar, el('span', null, staleNote() || source || '우클릭 · 설정'));
  return bar;
}

function meter(limit, accent, compact) {
  // A window whose reset time has already passed means the cached number
  // describes a period that is over — say so rather than show a stale bar.
  const expired = limit.resetsAt != null && limit.resetsAt < now;
  const tint = severityColour(limit.percent, accent);

  const box = el('div', 'meter');
  box.style.setProperty('--tint', tint);

  const row = el('div', 'row');
  add(row, el('span', 'label', limit.label),
      el('span', expired ? 'pct expired' : 'pct', expired ? '—' : `${Math.round(limit.percent)}%`));

  const track = el('div', 'track');
  if (!expired) {
    const fill = el('div', 'fill');
    fill.style.width = `max(3px, ${Math.min(limit.percent, 100)}%)`;
    track.appendChild(fill);
  }
  add(box, row, track);

  if (expired) {
    add(box, el('div', 'reset', '리셋 후 미갱신'));
  } else if (limit.resetsAt != null) {
    const t = until(limit.resetsAt);
    add(box, el('div', 'reset', compact ? t : `${t} · ${stampFmt.format(limit.resetsAt * 1000)}`));
  }
  return box;
}

function cacheNote(b, accent) {
  if (total(b) <= 0) return null;
  const box = el('div', 'cachenote');
  const bar = el('div', 'bar');
  const f = el('div', 'fresh');
  f.style.width = `${(fresh(b) / total(b)) * 100}%`;
  add(bar, f, el('div', 'cached'));
  add(box, bar, el('div', 'text',
    `실제 생성 ${tokens(fresh(b))} · 캐시 재사용 ${Math.round(cacheShare(b) * 100)}%`));
  return box;
}

function tokenRow(label, value) {
  return add(el('div', 'trow'), el('span', 'k', label), el('span', 'v', value));
}

function inlineRow(pairs) {
  const row = el('div', 'inline');
  pairs.forEach(([k, v], i) => {
    if (i > 0) add(row, el('span', 'sep', '·'));
    add(row, el('span', 'k', k), el('span', 'v', v));
  });
  return row;
}

function breakdown(title, legend, rows) {
  const box = el('div', 'breakdown');
  if (title || legend.length) {
    const head = el('div', 'bhead');
    if (title) add(head, el('span', 'title', title));
    add(head, el('span', 'spacer'));
    for (const [name, colour] of legend) {
      const g = el('span', 'legend');
      const sw = el('i', 'swatch');
      sw.style.setProperty('--c', colour);
      add(g, sw, el('span', null, name));
      add(head, g);
    }
    add(box, head);
  }
  rows.forEach((r) => add(box, r));
  return box;
}

/** One live session: what it has spent this window, and how full its context is
 *  right now. Two unrelated quantities, so two bars and two colours rather than
 *  one shared scale. */
function sessionRow(s, max) {
  const idle = s.tokens === 0;
  const share = s.contextTokens > 0 ? s.contextTokens / contextLimit : 0;
  const ctint = contextColour(share);

  const row = el('div', idle ? 'srow idle' : 'srow');
  const label = el('div', 'label', s.label);
  label.dataset.full = s.label;

  const bars = el('div', 'bars');
  add(bars, miniBar(max > 0 ? s.tokens / max : 0, 'var(--accent)'), miniBar(share, ctint));

  const nums = el('div', 'nums');
  nums.style.setProperty('--ctint', s.contextTokens > 0 ? ctint : 'var(--dim2)');
  add(nums,
      el('span', idle ? 'usage zero' : 'usage', idle ? '유휴' : tokens(s.tokens)),
      el('span', 'ctx', s.contextTokens > 0 ? tokens(s.contextTokens) : '—'));

  return add(row, label, bars, nums);
}

function modelRow(m, max) {
  const row = el('div', 'mrow');
  const label = el('div', 'label', m.name);
  return add(row, label, miniBar(max > 0 ? m.tokens / max : 0, 'var(--accent)'),
             el('span', 'v', tokens(m.tokens)));
}

function providerSection(title, accent, p, opts) {
  const { detail = 'none', rows = 5, showRecent = false, compact = false } = opts || {};
  const sec = el('section', 'provider');
  sec.style.setProperty('--accent', accent);

  const head = el('div', 'phead');
  add(head, el('i', 'dot'), el('span', 'name', title), el('span', 'spacer'));
  if (p.limitsAreLive) {
    const live = el('i', 'livedot');
    live.title = '계정에서 실시간 조회';
    add(head, live);
  }
  if (p.plan) add(head, el('span', 'plan', p.plan.toUpperCase()));
  add(sec, head);

  if (p.unavailable) {
    add(sec, el('div', 'muted', p.unavailable));
    return sec;
  }

  p.limits.forEach((l) => add(sec, meter(l, accent, compact)));
  if (!p.limits.length) add(sec, el('div', 'muted', '한도 정보 없음'));

  if (compact) {
    if (showRecent) add(sec, inlineRow([['최근 5시간', tokens(total(p.recent))]]));
    add(sec, inlineRow([['오늘', tokens(total(p.today))], ['주', tokens(total(p.week))]]));
  } else {
    if (showRecent) add(sec, tokenRow('최근 5시간', tokens(total(p.recent))));
    add(sec, tokenRow('오늘', tokens(total(p.today))));
    add(sec, tokenRow('이번 주', tokens(total(p.week))));
  }
  add(sec, cacheNote(showRecent ? p.recent : p.today, accent));

  const showsSessions = detail === 'sessions' || detail === 'both';
  const showsModels = detail === 'models' || detail === 'both';

  if (showsSessions && p.sessions.length) {
    const cap = compact ? Math.max(2, rows - 2) : rows;
    const list = p.sessions.slice(0, cap).map((s) => sessionRow(s, p.sessions[0].tokens));
    if (p.sessions.length > cap) {
      list.push(el('div', 'more', `+ ${p.sessions.length - cap}개 더`));
    }
    add(sec, breakdown('실행 중인 세션',
        [['사용량', accent], ['컨텍스트', 'var(--context)']], list));
  }

  if (showsModels && p.models.length) {
    const cap = compact ? 2 : Math.min(4, rows);
    add(sec, breakdown(detail === 'both' ? '모델' : null, [],
        p.models.slice(0, cap).map((m) => modelRow(m, p.models[0].tokens))));
  }
  return sec;
}

// MARK: - render

function resolveTheme() {
  if (prefs.appearance === 'system') {
    return window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light';
  }
  return prefs.appearance;
}

function render() {
  if (!prefs || !stats) return;

  document.documentElement.dataset.theme = resolveTheme();
  const horizontal = prefs.layout === 'horizontal';
  const width = horizontal ? prefs.widthHorizontal : prefs.widthVertical;
  panel.style.setProperty('--w', `${width}px`);
  panel.style.setProperty('--scale', prefs.scale);
  panel.style.opacity = prefs.opacity;
  panel.className = horizontal ? 'horizontal compact' : '';

  // Rebuilt wholesale each frame. The panel is a few dozen nodes, and a diffing
  // layer would cost more to reason about than the redraw costs to run.
  panel.replaceChildren();

  if (horizontal) {
    const cols = el('div', 'columns');
    add(cols,
        providerSection('Claude', 'var(--claude)', stats.claude,
          { detail: prefs.detail, rows: prefs.rows, showRecent: true, compact: true }),
        el('div', 'vrule'),
        providerSection('Codex', 'var(--codex)', stats.codex, { compact: true }));
    add(panel, cols, el('hr', 'hair'), footBar(true));
  } else {
    add(panel, titleBar(),
        providerSection('Claude', 'var(--claude)', stats.claude,
          { detail: prefs.detail, rows: prefs.rows, showRecent: true }),
        el('hr', 'hair'),
        providerSection('Codex', 'var(--codex)', stats.codex),
        el('hr', 'hair'),
        footBar(false));
  }
  panel.appendChild(grip);

  document.querySelectorAll('.srow .label').forEach(middleTruncate);
  syncWindowSize();
}

/* SwiftUI had `.truncationMode(.middle)`; CSS has no equivalent, and the tail of
 * a session name is exactly what distinguishes two of them. Results are cached
 * per (text, width) so the binary search runs once, not twice a second. */
const truncCache = new Map();

function middleTruncate(node) {
  const full = node.dataset.full;
  const w = node.clientWidth;
  if (!w) return;
  const key = `${full}|${w}`;
  const hit = truncCache.get(key);
  if (hit !== undefined) {
    node.textContent = hit;
    return;
  }
  node.textContent = full;
  if (node.scrollWidth <= w) {
    truncCache.set(key, full);
    return;
  }
  let lo = 0;
  let hi = full.length;
  let best = '…';
  while (lo <= hi) {
    const keep = (lo + hi) >> 1;
    const head = Math.ceil(keep / 2);
    const tail = keep - head;
    const candidate = full.slice(0, head) + '…' + (tail ? full.slice(full.length - tail) : '');
    node.textContent = candidate;
    if (node.scrollWidth <= w) {
      best = candidate;
      lo = keep + 1;
    } else {
      hi = keep - 1;
    }
  }
  node.textContent = best;
  truncCache.set(key, best);
}

/* The window has no decorations and never scrolls, so it has to be exactly as
 * big as its content. `zoom` is applied inside the page, and engines disagree on
 * whether getBoundingClientRect reports pre- or post-zoom values — so the known
 * CSS width is used to work out which, instead of assuming. */
function syncWindowSize() {
  const rect = panel.getBoundingClientRect();
  if (!rect.width) return;
  const cssWidth = prefs.layout === 'horizontal' ? prefs.widthHorizontal : prefs.widthVertical;
  const zoomIncluded = rect.width / (cssWidth * prefs.scale);
  const factor = Math.abs(zoomIncluded - 1) < 0.05 ? 1 : prefs.scale;
  const pad = 28; // the 14px body padding that leaves room for the shadow
  invoke('set_panel_size', {
    width: cssWidth * prefs.scale + pad,
    height: rect.height * factor + pad,
  });
}

// MARK: - input

// Whole-panel dragging, matching `isMovableByWindowBackground` on macOS.
panel.addEventListener('mousedown', (e) => {
  if (e.button !== 0 || e.target.closest('.grip')) return;
  appWindow.startDragging();
});

window.addEventListener('contextmenu', (e) => {
  e.preventDefault();
  invoke('show_menu');
});

/* The grip drives both axes: dragging sideways changes how wide the panel is,
 * dragging up and down zooms the whole thing. The anchor is captured once so the
 * drag stays absolute rather than accumulating rounding error frame by frame. */
let anchor = null;

grip.addEventListener('mousedown', (e) => {
  e.preventDefault();
  e.stopPropagation();
  grip.setPointerCapture?.(e.pointerId);
  anchor = {
    x: e.screenX,
    y: e.screenY,
    width: prefs.layout === 'horizontal' ? prefs.widthHorizontal : prefs.widthVertical,
    scale: prefs.scale,
  };
});

window.addEventListener('mousemove', (e) => {
  if (!anchor) return;
  const dx = e.screenX - anchor.x;
  const dy = e.screenY - anchor.y;
  // 400px of vertical travel doubles the zoom, which feels close to the corner
  // tracking the cursor.
  const scale = Math.min(2.0, Math.max(0.7, anchor.scale * (1 + dy / 400)));
  const width = anchor.width + dx / scale;
  prefs.scale = scale;
  if (prefs.layout === 'horizontal') {
    prefs.widthHorizontal = Math.min(900, Math.max(380, width));
  } else {
    prefs.widthVertical = Math.min(520, Math.max(230, width));
  }
  render();
});

window.addEventListener('mouseup', async () => {
  if (!anchor) return;
  anchor = null;
  // Persist once at the end of the drag, not on every frame.
  await invoke('set_pref', { key: 'scale', value: prefs.scale });
  prefs = await invoke('set_pref', {
    key: 'width',
    value: prefs.layout === 'horizontal' ? prefs.widthHorizontal : prefs.widthVertical,
  });
  render();
});

// MARK: - wiring

async function reload() {
  const snap = await invoke('get_snapshot');
  prefs = snap.prefs;
  stats = snap.stats;
  source = snap.source;
  contextLimit = snap.contextLimit || contextLimit;
  render();
}

listen('stats', (e) => {
  stats = e.payload;
  render();
});

listen('prefs-changed', reload);

// The clock and the "3분 전" label would otherwise sit still between refreshes.
setInterval(() => {
  now = Date.now() / 1000;
  if (stats) render();
}, 1000);

appWindow.onMoved(() => {
  clearTimeout(appWindow._save);
  appWindow._save = setTimeout(() => invoke('store_position'), 400);
});

window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
  if (prefs?.appearance === 'system') render();
});

reload();
