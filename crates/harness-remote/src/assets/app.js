/*
 * Harness phone client. Plain ES module, no build step. All untrusted text goes through
 * textContent / createTextNode; innerHTML is never used.
 *
 * Wire mapping (docs/plans/remote-contract.md + crates/harness-core-api/src/{types,events,chat,options}.rs,
 * every struct is #[serde(rename_all = "camelCase")]):
 *   POST /api/v1/pair    {token} | {code, name}           -> {ok, device_id} + cookie; 401 bad, 429 rate limited
 *   GET  /api/v1/status                                   -> {version, api_version, device_id, hub_ready}
 *   POST /api/v1/rpc     {method, params}                 -> {ok:true, value} | {ok:false, error}
 *   POST /api/v1/upload  raw bytes + X-File-Name          -> {ok:true, path} (saved on the desktop)
 *   WS   /api/v1/events  {type:"hello",seq} | {type:"event",event:HubEvent} | {type:"resync"}
 * RPC params:
 *   list_agents {} -> AgentInfo[] {id, displayName, availability, quota, runtime}
 *   list_sessions {workspaceKey?, query?, limit?} -> SessionSummary[]
 *   open_session / stop / list_modes / list_models {sessionId}
 *   send_message {sessionId, text, agent:null, clientMessageId, context:[ContextItem]}
 * ContextItem (chat.rs, #[serde(tag="type", rename_all="camelCase")]):
 *   {type:"image", path:null, dataBase64, mime}  pasted/picked image, downscaled on the phone
 *   {type:"file", path}                          uploaded first through POST /api/v1/upload
 *   limits (chat.rs): 10 items per turn, 5 MB per image, 20 MB per turn, non-empty text.
 *   cancel_queued {sessionId, turnId}
 *   respond_permission {sessionId, requestId, optionId}
 *   set_mode {sessionId, modeId}   set_model {sessionId, modelId}
 * SessionSummary {id, title, workspacePath, workspaceKey, activeAgent, createdAt, updatedAt, turnCount,
 *   agentsUsed, pinned, unread, parentId?}
 * SessionDetail {summary, turns: TurnView[], queue: TurnId[], pendingPermissions, plan, contextUsage,
 *   eventSeq, subagents: SubagentView[], cost, backgroundTasks}
 * TurnView {id, seq, userText, agent, assistantText, status, stopReason, error, toolCalls: ToolCallView[],
 *   createdAt, finishedAt, segments: ({type:"text",text}|{type:"tool",toolCallId}|{type:"steer",text})[]}
 *   status: queued|running|paused|completed|cancelled|failed|interrupted
 * ToolCallView {toolCallId, title, kind?, status?: pending|inProgress|completed|failed, rawInput?,
 *   content: ({type:"text",text}|{type:"diff",path,oldText?,newText}|{type:"terminal",terminalId})[],
 *   locations: {path, line?}[], rawOutput?}
 * PermissionRequestView {requestId, turnId, agent, tool: ToolCallView,
 *   options: {optionId, name, kind: allow_once|allow_always|reject_once|reject_always}[]}
 * SubagentView {sessionId, parentId, agent, title, status: running|needsApproval|done|failed|cancelled|
 *   interrupted, activity, startedAt, finishedAt?, resultPreview}
 * SessionModes/SessionModels {agent, current?, available: {id, name, description?}[], known}
 * HubEvent {seq, sessionId?, kind}; kind is #[serde(tag = "type", rename_all = "camelCase")]:
 *   sessionCreated|sessionUpdated {summary}  sessionsChanged  turnQueued {turnId, seq, userText, position, background}
 *   turnStarted {turnId, agent, attempt}  messageDelta|thoughtDelta {turnId, agent, text}
 *   toolCall|toolCallUpdated {turnId, agent, tool}  permissionRequested {request}
 *   permissionResolved {turnId, requestId, decision}
 *   turnFinished {turnId, agent?, outcome: {outcome:"completed",stopReason}|{outcome:"cancelled"}|{outcome:"failed",error,class}}
 *   turnPaused {turnId, reason}  turnFailure {turnId, agent, failure: {kind, message, ...}}
 *   agentSwitched {from?, to, reason, message}  sessionOptionsChanged {agent}  turnsRemoved {fromSeq}
 *   queueChanged {queue: {turnId, userText}[]}  steerInjected {turnId, steeredTurnId, text}  subagentUpdated {subagent}
 */

const $ = (sel) => document.querySelector(sel);

function el(tag, cls, text) {
  const node = document.createElement(tag);
  if (cls) node.className = cls;
  if (text !== undefined && text !== null) node.textContent = String(text);
  return node;
}

class Unauthorized extends Error {}

const BACKOFF_MIN = 500;
const BACKOFF_MAX = 15000;
const BUSY = new Set(['running', 'paused']);

const state = {
  authed: false,
  agents: new Map(),
  sessions: [],
  view: 'pair',
  sessionId: null,
  summary: null,
  eventSeq: 0,
  opening: null,
  buffer: [],
  turns: new Map(),
  order: [],
  queue: [],
  perms: new Map(),
  subs: new Map(),
  attachments: [],
  openTools: new Set(),
  optimistic: [],
  stack: [],
  socket: null,
  backoff: BACKOFF_MIN,
  retryTimer: 0,
  stale: false,
};

/* ---------- formatting ---------- */

const timeFmt = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });
const dateFmt = new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' });

function fmtWhen(ms) {
  if (!ms) return '';
  const diff = Date.now() - ms;
  if (diff < 60000) return 'now';
  if (diff < 3600000) return Math.floor(diff / 60000) + 'm ago';
  if (diff < 86400000) return timeFmt.format(new Date(ms));
  return dateFmt.format(new Date(ms));
}

function fmtElapsed(start, end) {
  if (!start) return '';
  const secs = Math.max(0, Math.round(((end || Date.now()) - start) / 1000));
  if (secs < 60) return secs + 's';
  const mins = Math.floor(secs / 60);
  if (mins < 60) return mins + 'm ' + (secs % 60) + 's';
  return Math.floor(mins / 60) + 'h ' + (mins % 60) + 'm';
}

function baseName(path) {
  const parts = String(path || '').split(/[\\/]+/).filter(Boolean);
  return parts.length ? parts[parts.length - 1] : '';
}

function clip(text, max) {
  const value = String(text == null ? '' : text);
  return value.length > max ? value.slice(0, max) + '…' : value;
}

function agentName(id) {
  if (!id) return '—';
  const hit = state.agents.get(id);
  return hit && hit.displayName ? hit.displayName : id;
}

let toastTimer = 0;
function toast(text) {
  const node = $('#toast');
  node.textContent = text;
  node.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    node.hidden = true;
  }, 2800);
}

/* ---------- HTTP / RPC ---------- */

async function postJson(path, body) {
  return fetch(path, {
    method: 'POST',
    credentials: 'same-origin',
    cache: 'no-store',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
}

async function rpc(method, params) {
  let res;
  try {
    res = await postJson('/api/v1/rpc', { method, params: params || {} });
  } catch (err) {
    throw new Error('network error');
  }
  if (res.status === 401) {
    onUnauthorized();
    throw new Unauthorized('not paired');
  }
  let data = null;
  try {
    data = await res.json();
  } catch (err) {
    data = null;
  }
  if (!data || data.ok !== true) {
    throw new Error(String((data && data.error) || 'HTTP ' + res.status));
  }
  return data.value;
}

async function rpcSoft(method, params, fallback) {
  try {
    return await rpc(method, params);
  } catch (err) {
    if (err instanceof Unauthorized) throw err;
    return fallback;
  }
}

async function getStatus() {
  try {
    const res = await fetch('/api/v1/status', { credentials: 'same-origin', cache: 'no-store' });
    if (res.status === 401) return { unauthorized: true };
    if (!res.ok) return null;
    return await res.json();
  } catch (err) {
    return null;
  }
}

function onUnauthorized() {
  state.authed = false;
  if (state.socket) {
    try {
      state.socket.close();
    } catch (err) {
      /* closed */
    }
  }
  clearTimeout(state.retryTimer);
  setOffline(false);
  show('pair');
}

/* ---------- pairing ---------- */

function deviceName() {
  const ua = navigator.userAgent || '';
  const os = /Android/i.test(ua)
    ? 'Android'
    : /iPhone|iPad|iPod/i.test(ua)
      ? 'iOS'
      : /Windows/i.test(ua)
        ? 'Windows'
        : /Mac OS X/i.test(ua)
          ? 'macOS'
          : /Linux/i.test(ua)
            ? 'Linux'
            : 'Phone';
  const browser = /EdgA?\//.test(ua)
    ? 'Edge'
    : /Firefox\//.test(ua)
      ? 'Firefox'
      : /Chrome\//.test(ua)
        ? 'Chrome'
        : /Safari\//.test(ua)
          ? 'Safari'
          : 'WebView';
  return (os + ' ' + browser).slice(0, 48);
}

async function pair(body) {
  let res;
  try {
    res = await postJson('/api/v1/pair', body);
  } catch (err) {
    throw new Error('Cannot reach the Harness hub.');
  }
  if (res.ok) return;
  if (res.status === 429) throw new Error('Too many attempts. Wait a minute, then try again.');
  if (res.status === 401) throw new Error('That code or link is invalid or has expired.');
  throw new Error('Pairing failed (HTTP ' + res.status + ').');
}

function showPairError(text) {
  const box = $('#pair-error');
  box.textContent = text || '';
  box.hidden = !text;
  $('#pair-retry').hidden = !text;
}

async function afterPaired() {
  const status = await getStatus();
  if (!status) {
    showPairError('Cannot reach the Harness hub.');
    show('pair');
    return;
  }
  if (status.unauthorized) {
    state.authed = false;
    show('pair');
    return;
  }
  state.authed = true;
  showPairError('');
  connect();
  await route();
}

/* ---------- views / routing ---------- */

const VIEWS = { pair: '#view-pair', list: '#view-list', session: '#view-session' };

function show(name) {
  state.view = name;
  for (const [key, sel] of Object.entries(VIEWS)) $(sel).hidden = key !== name;
}

function sessionHash(id) {
  return '#/s/' + encodeURIComponent(id);
}

async function route() {
  if (!state.authed) {
    show('pair');
    return;
  }
  const hit = /^#\/s\/(.+)$/.exec(location.hash);
  if (hit) {
    await openSession(decodeURIComponent(hit[1]));
  } else {
    leaveSession();
    await loadList();
  }
}

function leaveSession() {
  state.sessionId = null;
  state.summary = null;
  state.opening = null;
  state.buffer = [];
  state.stack = [];
  state.attachments = [];
  renderAttachments();
}

/* ---------- sessions list ---------- */

function setListStatus(text) {
  const node = $('#list-status');
  node.textContent = text || '';
  node.hidden = !text;
}

async function loadAgents() {
  const agents = await rpcSoft('list_agents', {}, null);
  if (Array.isArray(agents)) state.agents = new Map(agents.map((a) => [a.id, a]));
}

async function loadList() {
  show('list');
  if (!state.sessions.length) setListStatus('Loading…');
  try {
    const [sessions] = await Promise.all([rpc('list_sessions', {}), loadAgents()]);
    state.sessions = (Array.isArray(sessions) ? sessions : []).filter((s) => !s.parentId);
    state.sessions.sort((a, b) => (b.pinned - a.pinned) || (b.updatedAt || 0) - (a.updatedAt || 0));
    setListStatus('');
    renderList();
  } catch (err) {
    if (err instanceof Unauthorized) return;
    setListStatus('Could not load sessions: ' + err.message);
  }
}

function renderList() {
  const list = $('#list');
  list.textContent = '';
  $('#list-empty').hidden = state.sessions.length > 0;
  for (const s of state.sessions) {
    const li = el('li');
    const link = el('a');
    link.href = sessionHash(s.id);
    const row1 = el('div', 'row1');
    row1.appendChild(el('span', 'name', s.title || baseName(s.workspacePath) || 'Untitled'));
    if (s.unread) row1.appendChild(el('span', 'chip unread', 'new'));
    if (s.pinned) row1.appendChild(el('span', 'chip', 'pinned'));
    link.appendChild(row1);
    const meta = el('div', 'meta');
    meta.appendChild(el('span', '', agentName(s.activeAgent)));
    const folder = baseName(s.workspacePath);
    if (folder) meta.appendChild(el('span', '', folder));
    meta.appendChild(el('span', '', (s.turnCount || 0) + (s.turnCount === 1 ? ' turn' : ' turns')));
    meta.appendChild(el('span', '', fmtWhen(s.updatedAt)));
    link.appendChild(meta);
    li.appendChild(link);
    list.appendChild(li);
  }
}

let listTimer = 0;
function scheduleListRefresh() {
  if (state.view !== 'list') return;
  clearTimeout(listTimer);
  listTimer = setTimeout(() => {
    loadList().catch(() => undefined);
  }, 300);
}

/* ---------- markdown-lite ---------- */

const INLINE_RE =
  /(!\[[^\]\n]*\]\((?:https?:\/\/)[^)\s]+\))|(`[^`\n]+`)|(\*\*[^*\n]+\*\*)|(\[[^\]\n]+\]\((?:https?:\/\/|mailto:)[^)\s]+\))|(https?:\/\/[^\s<>()]+[^\s<>().,;:!?'"])/g;

function link(label, href) {
  const a = el('a', null, label);
  a.href = href;
  a.target = '_blank';
  a.rel = 'noopener noreferrer';
  return a;
}

function image(src, alt) {
  const img = el('img');
  img.src = src;
  img.alt = alt || '';
  img.loading = 'lazy';
  img.decoding = 'async';
  return img;
}

function inlineInto(parent, text) {
  let last = 0;
  for (const m of text.matchAll(INLINE_RE)) {
    if (m.index > last) parent.appendChild(document.createTextNode(text.slice(last, m.index)));
    const token = m[0];
    if (m[1]) {
      const cut = token.indexOf('](');
      parent.appendChild(image(token.slice(cut + 2, -1), token.slice(2, cut)));
    } else if (m[2]) {
      parent.appendChild(el('code', null, token.slice(1, -1)));
    } else if (m[3]) {
      parent.appendChild(el('strong', null, token.slice(2, -2)));
    } else if (m[4]) {
      const cut = token.indexOf('](');
      parent.appendChild(link(token.slice(1, cut), token.slice(cut + 2, -1)));
    } else {
      parent.appendChild(link(token, token));
    }
    last = m.index + token.length;
  }
  if (last < text.length) parent.appendChild(document.createTextNode(text.slice(last)));
}

function tableCells(line) {
  return line
    .trim()
    .replace(/^\|/, '')
    .replace(/\|$/, '')
    .split('|')
    .map((cell) => cell.trim());
}

function isDivider(line) {
  return line.includes('-') && /^\s*\|?[\s:|-]+\|[\s:|-]*$/.test(line);
}

/* Wide tables scroll inside their wrapper, never the page. */
function tableInto(parent, rows) {
  const wrap = el('div', 'table-wrap');
  const table = el('table');
  rows.forEach((row, index) => {
    const tr = el('tr');
    for (const cell of row) {
      const td = el(index === 0 ? 'th' : 'td');
      inlineInto(td, cell);
      tr.appendChild(td);
    }
    table.appendChild(tr);
  });
  wrap.appendChild(table);
  parent.appendChild(wrap);
}

function textInto(parent, text) {
  const lines = String(text).replace(/\s+$/, '').split('\n');
  let buf = [];
  const flush = () => {
    if (!buf.length) return;
    const chunk = buf.join('\n');
    buf = [];
    if (chunk.trim()) inlineInto(parent, chunk);
  };
  let i = 0;
  while (i < lines.length) {
    if (lines[i].includes('|') && i + 1 < lines.length && isDivider(lines[i + 1])) {
      const rows = [tableCells(lines[i])];
      i += 2;
      while (i < lines.length && lines[i].includes('|') && lines[i].trim()) {
        rows.push(tableCells(lines[i]));
        i += 1;
      }
      flush();
      tableInto(parent, rows);
      continue;
    }
    buf.push(lines[i]);
    i += 1;
  }
  flush();
}

function mdInto(parent, text) {
  const parts = String(text == null ? '' : text).split('```');
  parts.forEach((part, i) => {
    if (i % 2 === 1) {
      const nl = part.indexOf('\n');
      const code = nl >= 0 && /^[\w+#.-]*$/.test(part.slice(0, nl).trim()) ? part.slice(nl + 1) : part;
      const pre = el('pre');
      pre.appendChild(el('code', null, code.replace(/\n+$/, '')));
      parent.appendChild(pre);
    } else if (part) {
      textInto(parent, i > 0 ? part.replace(/^\n/, '') : part);
    }
  });
}

/* ---------- transcript model ---------- */

function newTurn(id) {
  return {
    id,
    userText: '',
    status: 'queued',
    error: '',
    notes: [],
    segments: [],
    tools: new Map(),
    article: null,
    dirty: false,
  };
}

function turnFromView(view) {
  const turn = newTurn(view.id);
  turn.userText = view.userText || '';
  turn.status = view.status || 'completed';
  turn.error = view.error || '';
  for (const tool of view.toolCalls || []) turn.tools.set(tool.toolCallId, tool);
  if (Array.isArray(view.segments) && view.segments.length) {
    for (const seg of view.segments) {
      if (seg.type === 'text') turn.segments.push({ type: 'text', text: seg.text || '' });
      else if (seg.type === 'tool') turn.segments.push({ type: 'tool', id: seg.toolCallId });
      else if (seg.type === 'steer') turn.segments.push({ type: 'steer', text: seg.text || '' });
    }
    for (const id of turn.tools.keys()) {
      if (!turn.segments.some((s) => s.type === 'tool' && s.id === id)) turn.segments.push({ type: 'tool', id });
    }
  } else {
    if (view.assistantText) turn.segments.push({ type: 'text', text: view.assistantText });
    for (const id of turn.tools.keys()) turn.segments.push({ type: 'tool', id });
  }
  return turn;
}

function getTurn(id) {
  let turn = state.turns.get(id);
  if (!turn) {
    turn = newTurn(id);
    state.turns.set(id, turn);
    state.order.push(id);
  }
  return turn;
}

let renderQueued = false;
function markDirty(turn) {
  turn.dirty = true;
  if (renderQueued) return;
  renderQueued = true;
  requestAnimationFrame(() => {
    renderQueued = false;
    flushTurns();
  });
}

function flushTurns() {
  const box = $('#turns');
  const nearBottom = box.scrollHeight - box.scrollTop - box.clientHeight < 160;
  for (const id of state.order) {
    const turn = state.turns.get(id);
    if (!turn) continue;
    const visible = turn.status !== 'queued' || turn.segments.length > 0;
    if (!visible) {
      if (turn.article) {
        turn.article.remove();
        turn.article = null;
      }
      continue;
    }
    if (!turn.article || turn.dirty) {
      const article = renderTurn(turn);
      if (turn.article) turn.article.replaceWith(article);
      turn.article = article;
      turn.dirty = false;
    }
  }
  let anchor = null;
  for (const id of state.order) {
    const article = state.turns.get(id) && state.turns.get(id).article;
    if (!article) continue;
    const want = anchor ? anchor.nextSibling : box.firstChild;
    if (want !== article) box.insertBefore(article, want);
    anchor = article;
  }
  for (const pending of state.optimistic) {
    if (pending.node.parentNode !== box || pending.node !== box.lastChild) box.appendChild(pending.node);
  }
  if (nearBottom) box.scrollTop = box.scrollHeight;
  updateBusy();
}

function userBubble(text, attachments) {
  const bubble = el('div', 'bubble user');
  const body = el('div', 'msg-body');
  mdInto(body, text);
  bubble.appendChild(body);
  if (attachments && attachments.length) {
    const row = el('div', 'msg-attach');
    for (const item of attachments) row.appendChild(el('span', null, clip(item.name, 40)));
    bubble.appendChild(row);
  }
  return bubble;
}

function renderTurn(turn) {
  const article = el('article', 'turn');
  article.dataset.turn = turn.id;
  if (turn.userText) article.appendChild(userBubble(turn.userText));
  let textBuf = '';
  const flushText = () => {
    if (!textBuf) return;
    const bubble = el('div', 'bubble agent');
    const body = el('div', 'msg-body');
    mdInto(body, textBuf);
    bubble.appendChild(body);
    article.appendChild(bubble);
    textBuf = '';
  };
  for (const seg of turn.segments) {
    if (seg.type === 'text') {
      textBuf += seg.text;
    } else if (seg.type === 'tool') {
      flushText();
      const tool = turn.tools.get(seg.id);
      if (tool) article.appendChild(toolCard(tool));
    } else if (seg.type === 'steer') {
      flushText();
      article.appendChild(userBubble(seg.text));
    }
  }
  flushText();
  if (turn.status === 'running' && !turn.segments.length) article.appendChild(el('div', 'note', 'working…'));
  for (const note of turn.notes) article.appendChild(el('div', 'note' + (note.err ? ' err' : ''), note.text));
  if (turn.error) article.appendChild(el('div', 'note err', turn.error));
  else if (turn.status === 'failed' && !turn.notes.some((n) => n.err)) article.appendChild(el('div', 'note err', 'Turn failed'));
  else if (turn.status === 'cancelled') article.appendChild(el('div', 'note', 'Stopped'));
  else if (turn.status === 'interrupted') article.appendChild(el('div', 'note', 'Interrupted'));
  else if (turn.status === 'paused') article.appendChild(el('div', 'note', 'Paused: every agent is out of quota'));
  return article;
}

const TOOL_STATUS = { pending: 'pending', inProgress: 'running', completed: 'done', failed: 'failed' };

function prettyJson(text) {
  if (!text) return '';
  try {
    return JSON.stringify(JSON.parse(text), null, 2);
  } catch (err) {
    return String(text);
  }
}

function toolCard(tool) {
  const card = el('details', 'tool');
  card.dataset.status = tool.status || 'pending';
  if (state.openTools.has(tool.toolCallId)) card.open = true;
  card.addEventListener('toggle', () => {
    if (card.open) state.openTools.add(tool.toolCallId);
    else state.openTools.delete(tool.toolCallId);
  });
  const summary = el('summary');
  summary.appendChild(el('span', 't-title', tool.title || tool.kind || 'Tool call'));
  summary.appendChild(el('span', 't-status', TOOL_STATUS[tool.status] || 'pending'));
  card.appendChild(summary);
  const body = el('div', 't-body');
  for (const loc of tool.locations || []) body.appendChild(el('pre', null, loc.path + (loc.line ? ':' + loc.line : '')));
  const input = prettyJson(tool.rawInput);
  if (input) body.appendChild(el('pre', null, clip(input, 4000)));
  for (const item of tool.content || []) {
    if (item.type === 'text' && item.text) body.appendChild(el('pre', null, clip(item.text, 6000)));
    else if (item.type === 'diff') body.appendChild(el('pre', null, clip(diffText(item), 6000)));
    else if (item.type === 'terminal') body.appendChild(el('pre', null, 'terminal ' + item.terminalId));
  }
  const output = prettyJson(tool.rawOutput);
  if (output) body.appendChild(el('pre', null, clip(output, 6000)));
  card.appendChild(body);
  return card;
}

function diffText(item) {
  const lines = [item.path || ''];
  for (const line of String(item.oldText || '').split('\n').slice(0, 80)) if (item.oldText) lines.push('- ' + line);
  for (const line of String(item.newText || '').split('\n').slice(0, 120)) lines.push('+ ' + line);
  return lines.join('\n');
}

function updateBusy() {
  let busy = false;
  for (const turn of state.turns.values()) if (BUSY.has(turn.status)) busy = true;
  $('#stop').hidden = !busy;
}

/* ---------- open session ---------- */

async function openSession(id) {
  if (!id) return;
  const isSame = state.sessionId === id;
  show('session');
  if (!isSame) {
    $('#s-title').textContent = 'Loading…';
    $('#turns').textContent = '';
    $('#perms').textContent = '';
    $('#input').value = '';
    autoGrow($('#input'));
    // attachments belong to the composer of the session they were picked in
    state.attachments = [];
    renderAttachments();
  }
  state.sessionId = id;
  state.opening = id;
  state.buffer = [];
  if (!state.agents.size) await loadAgents().catch(() => undefined);
  let detail;
  try {
    detail = await rpc('open_session', { sessionId: id });
  } catch (err) {
    if (state.opening === id) state.opening = null;
    if (err instanceof Unauthorized) return;
    $('#s-title').textContent = 'Session';
    toast('Could not open session: ' + err.message);
    return;
  }
  if (state.sessionId !== id) return;
  applyDetail(detail);
  const buffered = state.buffer;
  state.opening = null;
  state.buffer = [];
  for (const event of buffered) applySessionEvent(event);
  loadPickers();
}

function applyDetail(detail) {
  state.summary = detail.summary || null;
  state.eventSeq = detail.eventSeq || 0;
  state.turns = new Map();
  state.order = [];
  state.perms = new Map();
  state.subs = new Map();
  for (const pending of state.optimistic) pending.node.remove();
  state.optimistic = [];
  $('#turns').textContent = '';
  $('#perms').textContent = '';
  const turns = (detail.turns || []).slice().sort((a, b) => (a.seq || 0) - (b.seq || 0));
  for (const view of turns) {
    state.turns.set(view.id, turnFromView(view));
    state.order.push(view.id);
  }
  const queued = new Set(detail.queue || []);
  state.queue = (detail.queue || []).map((turnId) => ({
    turnId,
    userText: (state.turns.get(turnId) && state.turns.get(turnId).userText) || '',
  }));
  for (const turn of state.turns.values()) {
    if (turn.status === 'queued' && !queued.has(turn.id)) state.queue.push({ turnId: turn.id, userText: turn.userText });
  }
  for (const req of detail.pendingPermissions || []) addPermission(req);
  for (const sub of detail.subagents || []) state.subs.set(sub.sessionId, sub);
  renderHeader();
  renderQueue();
  renderSubagents();
  flushTurns();
  const box = $('#turns');
  box.scrollTop = box.scrollHeight;
}

function renderHeader() {
  const s = state.summary;
  $('#s-title').textContent = (s && (s.title || baseName(s.workspacePath))) || 'Session';
  $('#s-agent').textContent = agentName(s && s.activeAgent);
  $('#back').setAttribute('aria-label', state.stack.length ? 'Back to parent session' : 'Back to sessions');
}

/* ---------- permissions ---------- */

const ALLOW = new Set(['allow_once', 'allow_always']);

function addPermission(req) {
  if (!req || !req.requestId || state.perms.has(req.requestId)) return;
  const card = el('div', 'perm');
  const tool = req.tool || {};
  card.appendChild(el('div', 'p-head', tool.title || tool.kind || 'Permission request'));
  const sub = [agentName(req.agent)];
  if (tool.kind) sub.push(tool.kind);
  card.appendChild(el('div', 'p-sub', sub.join(' · ')));
  const input = prettyJson(tool.rawInput);
  if (input) card.appendChild(el('pre', 'p-sub', clip(input, 600)));
  const acts = el('div', 'p-acts');
  for (const option of req.options || []) {
    const allow = ALLOW.has(option.kind);
    const btn = el('button', 'btn ' + (allow ? 'allow' : 'deny'), option.name || (allow ? 'Allow' : 'Deny'));
    btn.type = 'button';
    btn.addEventListener('click', () => respondPermission(req.requestId, option.optionId, acts));
    acts.appendChild(btn);
  }
  card.appendChild(acts);
  $('#perms').appendChild(card);
  state.perms.set(req.requestId, card);
}

async function respondPermission(requestId, optionId, acts) {
  for (const btn of acts.querySelectorAll('button')) btn.disabled = true;
  try {
    await rpc('respond_permission', { sessionId: state.sessionId, requestId, optionId });
    dropPermission(requestId);
  } catch (err) {
    for (const btn of acts.querySelectorAll('button')) btn.disabled = false;
    if (!(err instanceof Unauthorized)) toast('Permission reply failed: ' + err.message);
  }
}

function dropPermission(requestId) {
  const card = state.perms.get(requestId);
  if (card) card.remove();
  state.perms.delete(requestId);
}

/* ---------- queue ---------- */

function renderQueue() {
  const list = $('#queue');
  list.textContent = '';
  $('#queue-wrap').hidden = state.queue.length === 0;
  for (const item of state.queue) {
    const li = el('li');
    li.appendChild(el('span', 'q-text', clip(item.userText || 'Queued message', 140)));
    const x = el('button', 'q-x', '✕');
    x.type = 'button';
    x.setAttribute('aria-label', 'Cancel queued message');
    x.addEventListener('click', () => cancelQueued(item.turnId, x));
    li.appendChild(x);
    list.appendChild(li);
  }
}

async function cancelQueued(turnId, btn) {
  btn.disabled = true;
  try {
    await rpc('cancel_queued', { sessionId: state.sessionId, turnId });
    state.queue = state.queue.filter((q) => q.turnId !== turnId);
    renderQueue();
  } catch (err) {
    btn.disabled = false;
    if (!(err instanceof Unauthorized)) toast('Cancel failed: ' + err.message);
  }
}

/* ---------- subagents ---------- */

const SUB_LABEL = {
  running: 'running',
  needsApproval: 'needs approval',
  done: 'done',
  failed: 'failed',
  cancelled: 'cancelled',
  interrupted: 'interrupted',
};

const SUB_ACTIVE = new Set(['running', 'needsApproval']);
const SUBS_KEY = 'harness.subs.open';

function subsOpen() {
  try {
    return localStorage.getItem(SUBS_KEY) === '1';
  } catch (err) {
    return false;
  }
}

function applySubsOpen(open) {
  $('#subs').hidden = !open;
  $('#subs-toggle').setAttribute('aria-expanded', open ? 'true' : 'false');
  $('#subs-chev').textContent = open ? '▾' : '▸';
}

function setSubsOpen(open) {
  try {
    localStorage.setItem(SUBS_KEY, open ? '1' : '0');
  } catch (err) {
    /* private mode: the bar just collapses again next time */
  }
  applySubsOpen(open);
}

function renderSubagents() {
  const subs = [...state.subs.values()].filter((s) => s.parentId === state.sessionId);
  // active first, then most recent
  subs.sort((a, b) => Number(SUB_ACTIVE.has(b.status)) - Number(SUB_ACTIVE.has(a.status)) || (b.startedAt || 0) - (a.startedAt || 0));
  $('#subs-wrap').hidden = subs.length === 0;
  const running = subs.filter((s) => SUB_ACTIVE.has(s.status)).length;
  $('#subs-count').textContent = 'Subagents · ' + subs.length + (running ? ' (' + running + ' active)' : '');
  applySubsOpen(subsOpen());
  const list = $('#subs');
  list.textContent = '';
  for (const sub of subs) {
    const li = el('li');
    const a = el('a');
    a.href = sessionHash(sub.sessionId);
    a.addEventListener('click', (event) => {
      event.preventDefault();
      openChild(sub.sessionId);
    });
    const row = el('div', 's-row');
    row.appendChild(el('span', 's-title', sub.title || agentName(sub.agent)));
    row.appendChild(el('span', 'chip ' + sub.status, SUB_LABEL[sub.status] || sub.status));
    const elapsed = el('span', 's-act', fmtElapsed(sub.startedAt, sub.finishedAt));
    if (!sub.finishedAt) elapsed.dataset.since = String(sub.startedAt || '');
    row.appendChild(elapsed);
    a.appendChild(row);
    const meta = agentName(sub.agent) + (sub.activity ? ' · ' + sub.activity : '');
    a.appendChild(el('div', 's-act', clip(meta, 160)));
    if (sub.resultPreview) a.appendChild(el('div', 's-res', clip(sub.resultPreview, 200)));
    li.appendChild(a);
    list.appendChild(li);
  }
}

function tickElapsed() {
  if (state.view !== 'session') return;
  for (const node of document.querySelectorAll('#subs [data-since]')) {
    const since = Number(node.dataset.since);
    if (since) node.textContent = fmtElapsed(since, 0);
  }
}

function openChild(childId) {
  if (state.sessionId) state.stack.push(state.sessionId);
  navigating = true;
  location.hash = sessionHash(childId);
}

/* ---------- mode / model ---------- */

function fillPicker(select, data) {
  const wrap = select.closest('.pick');
  select.textContent = '';
  const items = data && Array.isArray(data.available) ? data.available : [];
  if (!items.length) {
    wrap.hidden = true;
    return false;
  }
  for (const item of items) {
    const opt = el('option', null, item.name || item.id);
    opt.value = item.id;
    if (item.description) opt.title = item.description;
    select.appendChild(opt);
  }
  select.value = data.current || '';
  wrap.hidden = false;
  return true;
}

let pickerToken = 0;
async function loadPickers() {
  const id = state.sessionId;
  if (!id) return;
  const token = ++pickerToken;
  const [modes, models] = await Promise.all([
    rpcSoft('list_modes', { sessionId: id }, null),
    rpcSoft('list_models', { sessionId: id }, null),
  ]).catch(() => [null, null]);
  if (token !== pickerToken || state.sessionId !== id) return;
  const a = fillPicker($('#mode-sel'), modes);
  const b = fillPicker($('#model-sel'), models);
  $('#pickers').hidden = !(a || b);
}

async function changeOption(kind, value) {
  if (!value || !state.sessionId) return;
  const sel = kind === 'mode' ? $('#mode-sel') : $('#model-sel');
  sel.disabled = true;
  try {
    if (kind === 'mode') await rpc('set_mode', { sessionId: state.sessionId, modeId: value });
    else await rpc('set_model', { sessionId: state.sessionId, modelId: value });
  } catch (err) {
    if (!(err instanceof Unauthorized)) toast('Could not change ' + kind + ': ' + err.message);
    loadPickers();
  } finally {
    sel.disabled = false;
  }
}

/* ---------- composer ---------- */

const MAX_INPUT_LINES = 6;

function autoGrow(node) {
  const style = window.getComputedStyle(node);
  const line = parseFloat(style.lineHeight) || 20;
  const padding = (parseFloat(style.paddingTop) || 0) + (parseFloat(style.paddingBottom) || 0);
  const cap = Math.min(Math.round(line * MAX_INPUT_LINES + padding), Math.round(window.innerHeight * 0.4));
  node.style.height = 'auto';
  node.style.height = Math.min(node.scrollHeight, cap) + 'px';
}

function newClientId() {
  const rand = crypto && crypto.getRandomValues ? crypto.getRandomValues(new Uint32Array(2)) : [Math.random() * 1e9, Date.now()];
  return 'phone-' + Date.now().toString(36) + '-' + Array.from(rand, (n) => Math.floor(n).toString(36)).join('');
}

/* ---------- attachments ---------- */

// chat::limits (crates/harness-core-api/src/chat.rs)
const MAX_ITEMS = 10;
const MAX_IMAGE_BYTES = 5 * 1024 * 1024;
const MAX_UPLOAD_BYTES = 25 * 1024 * 1024;
const MAX_IMAGE_EDGE = 2048;
const JPEG_QUALITY = 0.85;
const SMALL_PNG_BYTES = 1024 * 1024;

function fmtBytes(bytes) {
  if (bytes >= 1024 * 1024) return (bytes / 1024 / 1024).toFixed(1) + ' MB';
  if (bytes >= 1024) return Math.round(bytes / 1024) + ' KB';
  return bytes + ' B';
}

function renderAttachments() {
  const row = $('#attach-row');
  row.textContent = '';
  row.hidden = state.attachments.length === 0;
  for (const item of state.attachments) {
    const chip = el('div', 'att');
    if (item.thumb) {
      const img = el('img');
      img.src = item.thumb;
      img.alt = '';
      chip.appendChild(img);
    }
    chip.appendChild(el('span', 'att-name', item.name));
    const x = el('button', 'att-x', '×');
    x.type = 'button';
    x.setAttribute('aria-label', 'Remove ' + item.name);
    x.addEventListener('click', () => removeAttachment(item.id));
    chip.appendChild(x);
    row.appendChild(chip);
  }
}

function removeAttachment(id) {
  state.attachments = state.attachments.filter((item) => item.id !== id);
  renderAttachments();
}

function loadImage(file) {
  return new Promise((resolve, reject) => {
    const url = URL.createObjectURL(file);
    const img = new Image();
    img.onload = () => {
      URL.revokeObjectURL(url);
      resolve(img);
    };
    img.onerror = () => {
      URL.revokeObjectURL(url);
      reject(new Error('not a readable image'));
    };
    img.src = url;
  });
}

function toBase64(buffer) {
  const bytes = new Uint8Array(buffer);
  let binary = '';
  const chunk = 0x8000;
  for (let i = 0; i < bytes.length; i += chunk) {
    binary += String.fromCharCode.apply(null, bytes.subarray(i, i + chunk));
  }
  return btoa(binary);
}

/* Downscale to MAX_IMAGE_EDGE (JPEG q0.85) so a phone photo stays small; small PNGs stay PNG. */
async function prepareImage(file) {
  if (file.type === 'image/png' && file.size <= SMALL_PNG_BYTES) {
    const dataBase64 = toBase64(await file.arrayBuffer());
    return { mime: 'image/png', dataBase64, thumb: 'data:image/png;base64,' + dataBase64 };
  }
  const img = await loadImage(file);
  const longEdge = Math.max(img.naturalWidth || 1, img.naturalHeight || 1);
  const scale = longEdge > MAX_IMAGE_EDGE ? MAX_IMAGE_EDGE / longEdge : 1;
  const canvas = document.createElement('canvas');
  canvas.width = Math.max(1, Math.round((img.naturalWidth || 1) * scale));
  canvas.height = Math.max(1, Math.round((img.naturalHeight || 1) * scale));
  const ctx = canvas.getContext('2d');
  ctx.fillStyle = '#ffffff';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  ctx.drawImage(img, 0, 0, canvas.width, canvas.height);
  const dataUrl = canvas.toDataURL('image/jpeg', JPEG_QUALITY);
  const comma = dataUrl.indexOf(',');
  return { mime: 'image/jpeg', dataBase64: dataUrl.slice(comma + 1), thumb: dataUrl };
}

async function addFiles(files) {
  // FileList is live: iterating after the input was cleared skips the remaining files
  const list = Array.from(files || []);
  for (const file of list) {
    if (state.attachments.length >= MAX_ITEMS) {
      toast('At most ' + MAX_ITEMS + ' attachments per message');
      return;
    }
    if (file.size > MAX_UPLOAD_BYTES) {
      toast((file.name || 'file') + ' is larger than ' + fmtBytes(MAX_UPLOAD_BYTES));
      continue;
    }
    const name = file.name || (file.type.startsWith('image/') ? 'image' : 'file');
    try {
      if ((file.type || '').startsWith('image/')) {
        const prepared = await prepareImage(file);
        const bytes = Math.floor((prepared.dataBase64.length * 3) / 4);
        if (bytes > MAX_IMAGE_BYTES) {
          toast(name + ' is still larger than ' + fmtBytes(MAX_IMAGE_BYTES) + ' after downscaling');
          continue;
        }
        state.attachments.push({
          id: newClientId(),
          kind: 'image',
          name,
          mime: prepared.mime,
          dataBase64: prepared.dataBase64,
          thumb: prepared.thumb,
        });
      } else {
        state.attachments.push({
          id: newClientId(),
          kind: 'file',
          name,
          mime: file.type || 'application/octet-stream',
          file,
        });
      }
      renderAttachments();
    } catch (err) {
      toast('Could not attach ' + name + ': ' + err.message);
    }
  }
}

/* Non-image files: POST /api/v1/upload, then sent as ContextItem::File{path}. */
async function uploadAttachment(item) {
  const name = item.name.replace(/[^\x20-\x7E]/g, '_').slice(0, 100) || 'file';
  let res;
  try {
    res = await fetch('/api/v1/upload', {
      method: 'POST',
      credentials: 'same-origin',
      cache: 'no-store',
      headers: { 'content-type': item.mime || 'application/octet-stream', 'x-file-name': name },
      body: item.file,
    });
  } catch (err) {
    throw new Error('network error while uploading ' + item.name);
  }
  if (res.status === 401) {
    onUnauthorized();
    throw new Unauthorized('not paired');
  }
  if (res.status === 413) throw new Error(item.name + ' is larger than ' + fmtBytes(MAX_UPLOAD_BYTES));
  const data = await res.json().catch(() => null);
  if (!data || data.ok !== true || !data.path) {
    throw new Error(String((data && data.error) || 'upload failed (HTTP ' + res.status + ')'));
  }
  return data.path;
}

async function buildContext(items) {
  const context = [];
  for (const item of items) {
    if (item.kind === 'image') {
      context.push({ type: 'image', path: null, dataBase64: item.dataBase64, mime: item.mime });
    } else {
      context.push({ type: 'file', path: await uploadAttachment(item) });
    }
  }
  return context;
}

async function submitComposer() {
  const input = $('#input');
  const text = input.value.trim();
  const items = state.attachments.slice();
  if (!state.sessionId || (!text && !items.length)) return;
  // the core rejects empty text, so an attachment-only message gets a generated caption
  const caption = text || 'Attached: ' + items.map((item) => item.name).join(', ');
  input.value = '';
  autoGrow(input);
  const sent = await sendMessage(caption, items);
  if (sent) {
    state.attachments = state.attachments.filter((item) => !items.includes(item));
    renderAttachments();
  }
}

async function sendMessage(text, items) {
  const sessionId = state.sessionId;
  const clientMessageId = newClientId();
  const node = el('article', 'turn');
  node.appendChild(userBubble(text, items));
  const pending = { clientMessageId, text, node };
  state.optimistic.push(pending);
  const box = $('#turns');
  box.appendChild(node);
  box.scrollTop = box.scrollHeight;
  try {
    const context = await buildContext(items || []);
    await rpc('send_message', { sessionId, text, agent: null, clientMessageId, context });
    if (!wsLive() && state.sessionId === sessionId) setTimeout(() => openSession(sessionId), 500);
    return true;
  } catch (err) {
    node.appendChild(el('div', 'note err', 'Not sent: ' + err.message));
    state.optimistic = state.optimistic.filter((p) => p !== pending);
    if (!(err instanceof Unauthorized)) toast('Send failed: ' + err.message);
    return false;
  }
}

function claimOptimistic(userText) {
  if (!state.optimistic.length) return;
  let index = state.optimistic.findIndex((p) => p.text === userText);
  if (index < 0) index = 0;
  const [pending] = state.optimistic.splice(index, 1);
  pending.node.remove();
}

async function stopTurn() {
  if (!state.sessionId) return;
  const btn = $('#stop');
  btn.disabled = true;
  try {
    await rpc('stop', { sessionId: state.sessionId });
  } catch (err) {
    if (!(err instanceof Unauthorized)) toast('Stop failed: ' + err.message);
  } finally {
    btn.disabled = false;
  }
}

/* ---------- websocket ---------- */

function wsLive() {
  return !!state.socket && state.socket.readyState === WebSocket.OPEN;
}

function setConn(live) {
  const dot = $('#conn');
  dot.className = 'dot ' + (live ? 'live' : 'dead');
  dot.title = live ? 'live' : 'reconnecting';
}

function setOffline(offline) {
  $('#offline').hidden = !offline;
}

function connect() {
  if (!state.authed) return;
  const cur = state.socket;
  if (cur && (cur.readyState === WebSocket.OPEN || cur.readyState === WebSocket.CONNECTING)) return;
  clearTimeout(state.retryTimer);
  let ws;
  try {
    ws = new WebSocket((location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/api/v1/events');
  } catch (err) {
    scheduleReconnect();
    return;
  }
  state.socket = ws;
  ws.addEventListener('open', () => {
    state.backoff = BACKOFF_MIN;
    setConn(true);
    setOffline(false);
    if (state.stale) {
      state.stale = false;
      resyncView();
    }
  });
  ws.addEventListener('message', (msg) => {
    let frame;
    try {
      frame = JSON.parse(msg.data);
    } catch (err) {
      return;
    }
    if (!frame || typeof frame !== 'object') return;
    if (frame.type === 'event') onEvent(frame.event);
    else if (frame.type === 'resync') resyncView();
  });
  ws.addEventListener('close', () => {
    if (state.socket === ws) state.socket = null;
    state.stale = true;
    setConn(false);
    scheduleReconnect();
  });
  ws.addEventListener('error', () => {
    try {
      ws.close();
    } catch (err) {
      /* closed */
    }
  });
}

function scheduleReconnect() {
  if (!state.authed) return;
  clearTimeout(state.retryTimer);
  const delay = state.backoff;
  state.backoff = Math.min(state.backoff * 2, BACKOFF_MAX);
  if (!navigator.onLine || delay >= 2000) setOffline(true);
  state.retryTimer = setTimeout(async () => {
    const status = await getStatus();
    if (status && status.unauthorized) {
      onUnauthorized();
      return;
    }
    connect();
  }, delay);
}

function resyncView() {
  if (state.view === 'session' && state.sessionId) openSession(state.sessionId);
  else if (state.view === 'list') scheduleListRefresh();
}

/* ---------- events ---------- */

function onEvent(event) {
  if (!event || !event.kind) return;
  const kind = event.kind;
  const type = kind.type;
  if (type === 'sessionCreated' || type === 'sessionsChanged') scheduleListRefresh();
  if (type === 'sessionUpdated' && kind.summary) {
    const idx = state.sessions.findIndex((s) => s.id === kind.summary.id);
    if (idx >= 0 && state.view === 'list') {
      state.sessions[idx] = kind.summary;
      scheduleListRefresh();
    }
    if (kind.summary.id === state.sessionId) {
      state.summary = kind.summary;
      renderHeader();
    }
  }
  if (type === 'subagentUpdated' && kind.subagent && kind.subagent.parentId === state.sessionId && state.sessionId) {
    state.subs.set(kind.subagent.sessionId, kind.subagent);
    renderSubagents();
    return;
  }
  if (!state.sessionId || event.sessionId !== state.sessionId) return;
  if (state.opening === state.sessionId) {
    state.buffer.push(event);
    return;
  }
  applySessionEvent(event);
}

function applySessionEvent(event) {
  if (typeof event.seq === 'number' && event.seq <= state.eventSeq) return;
  if (typeof event.seq === 'number') state.eventSeq = event.seq;
  const k = event.kind;
  switch (k.type) {
    case 'turnQueued': {
      const turn = getTurn(k.turnId);
      if (!turn.userText) turn.userText = k.userText || '';
      if (!k.background) claimOptimistic(k.userText);
      if (!state.queue.some((q) => q.turnId === k.turnId) && turn.status === 'queued') {
        state.queue.push({ turnId: k.turnId, userText: k.userText || '' });
        renderQueue();
      }
      markDirty(turn);
      break;
    }
    case 'turnStarted': {
      const turn = getTurn(k.turnId);
      turn.status = 'running';
      state.queue = state.queue.filter((q) => q.turnId !== k.turnId);
      renderQueue();
      markDirty(turn);
      break;
    }
    case 'messageDelta': {
      const turn = getTurn(k.turnId);
      if (turn.status === 'queued') turn.status = 'running';
      const last = turn.segments[turn.segments.length - 1];
      if (last && last.type === 'text') last.text += k.text || '';
      else turn.segments.push({ type: 'text', text: k.text || '' });
      markDirty(turn);
      break;
    }
    case 'toolCall':
    case 'toolCallUpdated': {
      if (!k.tool) break;
      const turn = getTurn(k.turnId);
      const id = k.tool.toolCallId;
      turn.tools.set(id, k.tool);
      if (!turn.segments.some((s) => s.type === 'tool' && s.id === id)) turn.segments.push({ type: 'tool', id });
      markDirty(turn);
      break;
    }
    case 'permissionRequested':
      addPermission(k.request);
      break;
    case 'permissionResolved':
      dropPermission(k.requestId);
      break;
    case 'turnFinished': {
      const turn = getTurn(k.turnId);
      const outcome = (k.outcome && k.outcome.outcome) || 'completed';
      turn.status = outcome;
      if (outcome === 'failed' && k.outcome.error) turn.error = k.outcome.error;
      state.queue = state.queue.filter((q) => q.turnId !== k.turnId);
      renderQueue();
      markDirty(turn);
      break;
    }
    case 'turnPaused': {
      const turn = getTurn(k.turnId);
      turn.status = 'paused';
      markDirty(turn);
      break;
    }
    case 'turnFailure': {
      const turn = getTurn(k.turnId);
      if (k.failure && k.failure.message) turn.notes.push({ text: k.failure.message, err: true });
      markDirty(turn);
      break;
    }
    case 'steerInjected': {
      const turn = getTurn(k.turnId);
      turn.segments.push({ type: 'steer', text: k.text || '' });
      state.queue = state.queue.filter((q) => q.turnId !== k.steeredTurnId);
      const steered = state.turns.get(k.steeredTurnId);
      if (steered && steered.status === 'queued') {
        state.turns.delete(k.steeredTurnId);
        state.order = state.order.filter((id) => id !== k.steeredTurnId);
        if (steered.article) steered.article.remove();
      }
      renderQueue();
      markDirty(turn);
      break;
    }
    case 'queueChanged':
      state.queue = (k.queue || []).map((q) => ({ turnId: q.turnId, userText: q.userText || '' }));
      for (const q of state.queue) {
        const turn = state.turns.get(q.turnId);
        if (turn && !turn.userText) turn.userText = q.userText;
      }
      renderQueue();
      break;
    case 'agentSwitched':
      if (state.summary) state.summary.activeAgent = k.to;
      renderHeader();
      if (k.message) toast(k.message);
      loadPickers();
      break;
    case 'sessionOptionsChanged':
      loadPickers();
      break;
    case 'turnsRemoved':
      openSession(state.sessionId);
      break;
    default:
      break;
  }
}

/* ---------- wiring ---------- */

let navigating = false;

function bindUi() {
  $('#pair-form').addEventListener('submit', async (event) => {
    event.preventDefault();
    const input = $('#pair-code');
    const code = (input.value || '').replace(/[\s-]/g, '').toUpperCase();
    if (code.length !== 8) {
      showPairError('Enter the 8-character code.');
      return;
    }
    const btn = event.target.querySelector('button[type="submit"]');
    btn.disabled = true;
    showPairError('');
    try {
      await pair({ code, name: deviceName() });
      input.value = '';
      history.replaceState(null, '', '/');
      await afterPaired();
    } catch (err) {
      showPairError(err.message);
    } finally {
      btn.disabled = false;
    }
  });
  $('#pair-code').addEventListener('input', (event) => {
    const node = event.target;
    node.value = node.value.toUpperCase().replace(/[^A-Z0-9]/g, '').slice(0, 8);
  });
  $('#pair-retry').addEventListener('click', () => {
    showPairError('');
    afterPaired().catch(() => undefined);
  });
  $('#back').addEventListener('click', () => {
    const parent = state.stack.pop();
    navigating = true;
    if (parent) {
      location.hash = sessionHash(parent);
    } else {
      navigating = false;
      history.replaceState(null, '', '/');
      leaveSession();
      loadList().catch(() => undefined);
    }
  });
  $('#reload').addEventListener('click', () => loadList().catch(() => undefined));
  $('#refresh').addEventListener('click', () => {
    if (state.sessionId) openSession(state.sessionId);
  });
  $('#subs-toggle').addEventListener('click', () => {
    setSubsOpen(!subsOpen());
  });
  applySubsOpen(subsOpen());
  $('#composer').addEventListener('submit', (event) => {
    event.preventDefault();
    submitComposer().catch(() => undefined);
  });
  $('#attach').addEventListener('click', () => $('#file-input').click());
  $('#file-input').addEventListener('change', (event) => {
    const input = event.target;
    const files = Array.from(input.files || []);
    input.value = '';
    addFiles(files).catch(() => undefined);
  });
  $('#input').addEventListener('input', (event) => autoGrow(event.target));
  $('#input').addEventListener('paste', (event) => {
    const items = event.clipboardData && event.clipboardData.items;
    if (!items) return;
    const files = [];
    for (const item of items) {
      if (item.kind !== 'file') continue;
      const file = item.getAsFile();
      if (file) files.push(file);
    }
    if (!files.length) return;
    event.preventDefault();
    addFiles(files).catch(() => undefined);
  });
  $('#input').addEventListener('keydown', (event) => {
    const coarse = window.matchMedia && window.matchMedia('(pointer: coarse)').matches;
    // on a phone Enter is a newline; only a hardware keyboard sends
    if (event.key === 'Enter' && !event.shiftKey && !event.isComposing && !coarse) {
      event.preventDefault();
      $('#composer').requestSubmit();
    }
  });
  $('#stop').addEventListener('click', stopTurn);
  $('#mode-sel').addEventListener('change', (event) => changeOption('mode', event.target.value));
  $('#model-sel').addEventListener('change', (event) => changeOption('model', event.target.value));
  window.addEventListener('hashchange', () => {
    if (!navigating) state.stack = [];
    navigating = false;
    route().catch(() => undefined);
  });
  window.addEventListener('online', () => {
    state.backoff = BACKOFF_MIN;
    connect();
  });
  window.addEventListener('offline', () => {
    setOffline(true);
    setConn(false);
  });
  document.addEventListener('visibilitychange', () => {
    if (document.visibilityState !== 'visible' || !state.authed) return;
    if (!wsLive()) {
      state.backoff = BACKOFF_MIN;
      connect();
    }
  });
  setInterval(tickElapsed, 1000);
}

async function boot() {
  bindUi();
  setOffline(false);
  const fragment = new URLSearchParams(location.hash.replace(/^#/, ''));
  const token = fragment.get('t');
  if (token) {
    history.replaceState(null, '', '/');
    try {
      await pair({ token, name: deviceName() });
    } catch (err) {
      show('pair');
      showPairError(err.message);
      return;
    }
  }
  const status = await getStatus();
  if (!status) {
    show('pair');
    showPairError('Cannot reach the Harness hub.');
    return;
  }
  if (status.unauthorized) {
    show('pair');
    return;
  }
  state.authed = true;
  if (location.pathname === '/p') history.replaceState(null, '', '/' + location.hash);
  connect();
  await route();
}

if ('serviceWorker' in navigator) {
  window.addEventListener('load', () => {
    navigator.serviceWorker.register('/sw.js', { scope: '/' }).catch(() => undefined);
  });
}

boot().catch((err) => {
  show('pair');
  showPairError('Startup failed: ' + err.message);
});
