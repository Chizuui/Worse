import { bottomNav } from '/assets/settings.js';

const AGENT_CLASS = [
  [/claude/i, 'a-claude'],
  [/deepseek/i, 'a-deepseek'],
  [/codex|openai|gpt/i, 'a-codex'],
  [/antigravity|agy/i, 'a-agy'],
];

const FALLBACK_GROUP = '';

let root = null;
let api = null;
let listNode = null;
let connDot = null;
let connText = null;
let tunnelChip = null;
let searchInput = null;
let filterBtn = null;
let filterText = null;
let fabBtn = null;

let model = { sessions: [], statusText: '', live: false, remote: null };
let query = '';
let agentFilter = '';
let collapsed = new Set();
let createBlocked = false;
let creating = false;

function agentClass(id) {
  const key = String(id || '');
  for (const [re, cls] of AGENT_CLASS) if (re.test(key)) return cls;
  return 'a-other';
}

function groupKey(session) {
  const parts = String(session.workspacePath || '').split(/[\\/]+/).filter(Boolean);
  if (parts.length >= 2) return parts.slice(-2).join(' / ');
  return parts.length ? parts[0] : FALLBACK_GROUP;
}

function groupLabel(key) {
  return key ? key.toUpperCase() : 'OTHER';
}

function matches(session) {
  if (agentFilter && session.activeAgent !== agentFilter) return false;
  if (!query) return true;
  const hay = [session.title, session.workspacePath, api.agentName(session.activeAgent)]
    .join('\n')
    .toLowerCase();
  return hay.includes(query);
}

function groupSessions(sessions) {
  const pinned = [];
  const folders = new Map();
  for (const session of sessions) {
    if (session.pinned) {
      pinned.push(session);
      continue;
    }
    const key = groupKey(session);
    if (!folders.has(key)) folders.set(key, []);
    folders.get(key).push(session);
  }
  const byNewest = (a, b) => (b.updatedAt || 0) - (a.updatedAt || 0);
  for (const items of folders.values()) items.sort(byNewest);
  const groups = [];
  if (pinned.length) groups.push({ key: null, label: 'Pinned', pinned: true, sessions: pinned });
  const ordered = [...folders.entries()].sort((a, b) => {
    const emptyA = a[0] === FALLBACK_GROUP ? 1 : 0;
    const emptyB = b[0] === FALLBACK_GROUP ? 1 : 0;
    if (emptyA !== emptyB) return emptyA - emptyB;
    return byNewest(a[1][0], b[1][0]);
  });
  for (const [key, items] of ordered) {
    groups.push({ key, label: groupLabel(key), pinned: false, sessions: items });
  }
  return groups;
}

function currentWorkspace() {
  const source = model.sessions.find((s) => s.workspacePath && !s.parentId);
  return source ? source.workspacePath : '';
}

function row(session, active) {
  const a = api.el('a', 'hl-row ' + agentClass(session.activeAgent) + (active ? ' is-active' : ''));
  a.href = api.sessionHash(session.id);
  if (session.unread) a.classList.add('is-unread');
  a.appendChild(api.el('span', 'hl-dot-agent'));

  const text = api.el('span', 'hl-row-txt');
  text.appendChild(
    api.el('span', 'hl-row-title', session.title || api.baseName(session.workspacePath) || 'Untitled'),
  );
  const when = api.fmtWhen(session.updatedAt);
  const meta = [api.agentName(session.activeAgent), when].filter(Boolean).join(' · ');
  text.appendChild(api.el('span', 'hl-row-meta', meta));
  a.appendChild(text);

  const flags = api.el('span', 'hl-row-flags');
  if (session.running) {
    const run = api.el('span', 'hl-flag', '⏵');
    run.title = 'Running';
    run.setAttribute('aria-label', 'Running');
    flags.appendChild(run);
  }
  if (session.unread) {
    const dot = api.el('span', 'hl-unread');
    dot.title = 'Unread';
    dot.setAttribute('aria-label', 'Unread');
    flags.appendChild(dot);
  }
  if (flags.childNodes.length) a.appendChild(flags);

  const li = api.el('li');
  li.appendChild(a);
  return li;
}

function groupNode(group, activeId) {
  const wrap = api.el('div', 'hl-group');
  const rows = api.el('ul', 'hl-rows');

  if (group.pinned) {
    wrap.appendChild(api.el('p', 'hl-label', group.label));
  } else {
    const closed = collapsed.has(group.key);
    const head = api.el('button', 'hl-gh');
    head.type = 'button';
    head.setAttribute('aria-expanded', closed ? 'false' : 'true');
    const left = api.el('span', 'hl-gh-l');
    const chev = api.el('span', 'hl-chev' + (closed ? ' is-closed' : ''), '▾');
    chev.setAttribute('aria-hidden', 'true');
    left.appendChild(chev);
    left.appendChild(api.el('span', 'hl-gh-label', group.label));
    head.appendChild(left);
    if (group.key !== FALLBACK_GROUP) {
      head.appendChild(api.el('span', 'hl-count', String(group.sessions.length)));
    }
    head.addEventListener('click', () => {
      if (collapsed.has(group.key)) collapsed.delete(group.key);
      else collapsed.add(group.key);
      paint();
    });
    wrap.appendChild(head);
    if (closed) {
      rows.hidden = true;
    }
  }

  for (const session of group.sessions) rows.appendChild(row(session, session.id === activeId));
  wrap.appendChild(rows);
  return wrap;
}

function updateFab() {
  if (!fabBtn) return;
  fabBtn.hidden = createBlocked || !currentWorkspace();
}

function paint() {
  if (!listNode) return;
  listNode.textContent = '';

  if (model.statusText) listNode.appendChild(api.el('p', 'hl-status', model.statusText));

  const visible = (Array.isArray(model.sessions) ? model.sessions : []).filter(matches);
  if (!visible.length) {
    if (!model.statusText) listNode.appendChild(api.el('p', 'hl-empty', query || agentFilter ? 'No matching sessions' : 'No sessions yet'));
    updateFab();
    return;
  }

  const groups = groupSessions(visible);
  const activeId = groups.length && groups[0].sessions.length ? groups[0].sessions[0].id : null;
  for (const group of groups) listNode.appendChild(groupNode(group, activeId));
  updateFab();
}

function refreshFilter() {
  if (!filterBtn || !api) return;
  const names = new Map();
  for (const session of Array.isArray(model.sessions) ? model.sessions : []) {
    const id = session.activeAgent;
    if (id && !names.has(id)) names.set(id, api.agentName(id));
  }
  if (agentFilter && !names.has(agentFilter)) agentFilter = '';
  filterText.textContent = (agentFilter ? names.get(agentFilter) : 'All agents') + ' ▾';
  filterBtn.classList.toggle('is-on', Boolean(agentFilter));
  filterBtn.dataset.order = [...names.keys()].join(',');
}

function cycleFilter() {
  const order = filterBtn && filterBtn.dataset.order ? filterBtn.dataset.order.split(',') : [];
  const options = ['', ...order];
  const index = options.indexOf(agentFilter);
  agentFilter = options[(index + 1) % options.length];
  paint();
  refreshFilter();
}

function setConnection() {
  if (!connDot || !connText) return;
  const live = Boolean(model.live);
  connDot.classList.toggle('is-off', !live);
  let text = live ? 'Connected' : 'Reconnecting…';
  const version = model.remote && model.remote.version;
  if (live && version) text += ' · v' + version;
  connText.textContent = text;
  const tunnel =
    (model.remote && (model.remote.tunnel || model.remote.via_tunnel)) ||
    /trycloudflare\.com$/i.test(location.hostname);
  tunnelChip.hidden = !tunnel;
}

async function createNew() {
  const workspace = currentWorkspace();
  if (!workspace || creating || createBlocked) return;
  creating = true;
  fabBtn.disabled = true;
  try {
    await api.createSession({ workspacePath: workspace });
  } catch (err) {
    createBlocked = true;
    fabBtn.hidden = true;
    api.toast(err && err.message ? err.message : 'Could not start a session');
  } finally {
    creating = false;
    if (fabBtn) fabBtn.disabled = false;
  }
}

function build(node) {
  node.textContent = '';

  const head = api.el('header', 'hl-head');
  const left = api.el('div', 'hl-head-l');
  left.appendChild(api.el('h1', 'hl-title', 'Sessions'));
  const conn = api.el('div', 'hl-conn');
  connDot = api.el('span', 'hl-conn-dot');
  connDot.setAttribute('aria-hidden', 'true');
  connText = api.el('span', 'hl-conn-text', 'Connecting…');
  tunnelChip = api.el('span', 'hl-tunnel', 'via tunnel');
  tunnelChip.hidden = true;
  conn.appendChild(connDot);
  conn.appendChild(connText);
  conn.appendChild(tunnelChip);
  left.appendChild(conn);
  head.appendChild(left);

  const avatar = api.el('button', 'hl-avatar', 'S');
  avatar.type = 'button';
  avatar.setAttribute('aria-label', 'Settings');
  avatar.addEventListener('click', () => api.openSettings(''));
  head.appendChild(avatar);
  node.appendChild(head);

  const search = api.el('div', 'hl-search');
  search.setAttribute('role', 'search');
  const icon = api.el('span', 'hl-search-icon', '⌕');
  icon.setAttribute('aria-hidden', 'true');
  search.appendChild(icon);
  searchInput = api.el('input', 'hl-input');
  searchInput.type = 'search';
  searchInput.placeholder = 'Search sessions';
  searchInput.setAttribute('aria-label', 'Search sessions');
  searchInput.autocomplete = 'off';
  searchInput.spellcheck = false;
  searchInput.addEventListener('input', () => {
    query = searchInput.value.trim().toLowerCase();
    paint();
  });
  searchInput.addEventListener('search', () => {
    query = searchInput.value.trim().toLowerCase();
    paint();
  });
  search.appendChild(searchInput);
  filterBtn = api.el('button', 'hl-filter');
  filterBtn.type = 'button';
  filterBtn.setAttribute('aria-label', 'Filter by agent');
  filterText = api.el('span', 'hl-filter-text', 'All agents ▾');
  filterBtn.appendChild(filterText);
  filterBtn.addEventListener('click', cycleFilter);
  search.appendChild(filterBtn);
  node.appendChild(search);

  listNode = api.el('div', 'hl-list');
  node.appendChild(listNode);

  fabBtn = api.el('button', 'hl-fab', '+');
  fabBtn.type = 'button';
  fabBtn.setAttribute('aria-label', 'New session');
  fabBtn.hidden = true;
  fabBtn.addEventListener('click', () => {
    createNew();
  });
  node.appendChild(fabBtn);
  node.appendChild(bottomNav(api, 'chats'));
}

export function initSessions(node, viewApi) {
  if (!node || !viewApi) return;
  root = node;
  api = viewApi;
  build(node);
  setConnection();
}

export function renderSessions(next) {
  if (!root || !api) return;
  model = next || { sessions: [], statusText: '', live: false, remote: null };
  if (!Array.isArray(model.sessions)) model.sessions = [];
  setConnection();
  refreshFilter();
  paint();
}
