'use strict';

// The Processes page: every AI agent on this machine (Claude Code, Codex, Kiro, desktop
// apps, local model runtimes…) with its subagents and child processes, the heaviest other
// programs, and CPU and RAM for each, read from agents-monitor through /api/procs. Any tree
// can be paused, resumed, ended or killed, after a confirmation, so a build can have the
// machine. Fusebox reads the process table itself (src/procs); polled every few seconds
// while the page shows.

const Procs = {
  data: null, // the last /api/procs answer
  error: null,
  loading: false,
  at: 0,
  sort: 'cpu', // 'cpu' | 'ram' | 'start'
  filter: '',
  open: new Set(), // agent keys whose child processes show
  confirm: null, // { key, action, pids, label }
  busy: null, // key of the tree a signal is on its way to
  msg: null, // the last action's outcome
  others: 12,
};

const PROCS_EVERY = 3000;
const PROC_ACTIONS = {
  pause: ['Pause', 'Pause (SIGSTOP)', 'stops the whole tree until resumed; nothing is lost'],
  resume: ['Resume', 'Resume (SIGCONT)', 'lets a paused tree continue'],
  term: ['End', 'End (SIGTERM)', 'asks every process in the tree to quit; it may save state first'],
  kill: ['Kill', 'Kill (SIGKILL)', 'ends every process in the tree at once, no questions asked'],
};

function procsBytes(b) {
  b = Number(b) || 0;
  if (b >= 1 << 30) return `${(b / (1 << 30)).toFixed(1)}G`;
  if (b >= 1 << 20) return `${Math.round(b / (1 << 20))}M`;
  return `${Math.round(b / 1024)}K`;
}
function procsPct(v) {
  v = Number(v) || 0;
  return `${v >= 100 ? Math.round(v) : v >= 10 ? v.toFixed(0) : v.toFixed(1)}%`;
}
function procsUp(iso, now = Date.now()) {
  if (!iso) return '';
  const s = Math.max(0, Math.round((now - new Date(iso).getTime()) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h${String(Math.floor((s % 3600) / 60)).padStart(2, '0')}`;
  return `${Math.floor(s / 86400)}d${Math.floor((s % 86400) / 3600)}h`;
}

// Agents by CPU, RAM or start time; the filter matches kind, name, directory, status,
// tool or pid.
function procsSorted(agents, sort = Procs.sort, filter = Procs.filter) {
  const q = filter.trim().toLowerCase();
  const list = agents.filter((a) => !q || [a.kind, a.name, a.cwd, a.status, a.tool, String(a.pid), ...(a.procs || []).map((p) => p.cmd)].some((v) => String(v || '').toLowerCase().includes(q)));
  const by = { cpu: (a, b) => b.tree_cpu - a.tree_cpu || b.tree_rss - a.tree_rss, ram: (a, b) => b.tree_rss - a.tree_rss || b.tree_cpu - a.tree_cpu, start: (a, b) => new Date(b.started) - new Date(a.started) };
  return list.sort(by[sort] || by.cpu);
}

function procsStatusClass(a) {
  if (a.alerts?.length) return 'error';
  if (a.paused || a.status === 'paused') return 'disabled';
  if (a.status === 'busy' || a.status === 'running') return 'serving';
  if (a.status === 'waiting') return 'cooling';
  return '';
}

function procsHTML() {
  const d = Procs.data;
  const search = `<div class="searchbox" style="flex:0 1 260px">${ICON.search}<label class="sr-only" for="procs-filter">Find a process</label><input id="procs-filter" type="text" placeholder="Find a process" value="${esc(Procs.filter)}" autocomplete="off" spellcheck="false">${Procs.filter ? '<button class="clear" type="button" data-act="procs-clear-filter" aria-label="Clear">×</button>' : ''}</div>`;
  const sorts = ['cpu', 'ram', 'start'].map((s) => `<button type="button" data-act="procs-sort" data-id="${s}" aria-pressed="${Procs.sort === s}">${{ cpu: 'CPU', ram: 'RAM', start: 'Newest' }[s]}</button>`).join('');
  return `<div class="page-head"><div><h1 class="h-page">Processes</h1><p class="meta" id="procs-count">${procsCountText()}</p></div><span class="grow"></span><div class="seg" role="group" aria-label="Sort">${sorts}</div>${search}</div>
    <div id="procs-sys">${procsSystemHTML()}</div>
    <div id="procs-msg">${procsMsgHTML()}</div>
    <div id="procs-agents">${procsAgentsHTML()}</div>
    <div id="procs-others">${procsOthersHTML()}</div>
    ${d?.available ? '<p class="meta">Read from the process table every few seconds. CPU is percent of one core, summed over each tree. RAM is resident memory.</p>' : ''}`;
}

function procsCountText() {
  const d = Procs.data;
  if (!d) return Procs.error ? 'Could not read the process table.' : 'Reading the process table…';
  if (!d.available) return 'The process table could not be read.';
  const busy = d.agents.filter((a) => a.status === 'busy').length;
  return `${plural(d.agents.length, 'agent')} · ${busy} busy · ${plural(d.agents.reduce((n, a) => n + (a.pids?.length || 0), 0), 'process', 'processes')} in their trees`;
}

function procsSystemHTML() {
  const d = Procs.data;
  if (!d?.available) {
    if (!d && !Procs.error) return '<div class="card pad"><p class="meta">Reading the process table…</p></div>';
    const reason = d?.error || Procs.error || '';
    return `<div class="card pad empty" style="border-top:1px solid var(--line)"><h3>No process data</h3><p>${esc(reason)}</p></div>`;
  }
  const s = d.system || {};
  const memPct = s.mem_total ? (s.mem_used / s.mem_total) * 100 : 0;
  const agentCpu = d.agents.reduce((n, a) => n + (a.tree_cpu || 0), 0);
  const agentRss = d.agents.reduce((n, a) => n + (a.tree_rss || 0), 0);
  const fig = (v, label, cls = '') => `<div><b class="${cls}">${v}</b><span>${label}</span></div>`;
  const alerts = (s.alerts || []).map((t) => `<span class="err">⚠ ${esc(t)}</span>`).join(' ');
  return `<div class="card pad"><div class="figs procs-figs">
    ${fig(procsPct(s.cpu_percent), 'CPU busy', s.cpu_percent > 80 ? 'warn' : '')}
    ${fig(`${(s.load1 || 0).toFixed(1)} <span class="meta">/ ${s.num_cpu || '?'}</span>`, 'load, cores', s.num_cpu && s.load1 / s.num_cpu > 1.5 ? 'warn' : '')}
    ${fig(`${procsBytes(s.mem_used)} <span class="meta">/ ${procsBytes(s.mem_total)}</span>`, `memory, ${memPct.toFixed(0)}% used`, memPct > 90 ? 'err' : memPct > 80 ? 'warn' : '')}
    ${fig(procsPct(agentCpu), 'agent trees, CPU of one core')}
    ${fig(procsBytes(agentRss), 'agent trees, RAM')}
    ${fig(String(d.agents.length), d.hook_seen ? 'agents · hooks on' : 'agents · no hooks')}
  </div>${alerts ? `<p class="small" style="margin:10px 0 0">${alerts}</p>` : ''}</div>`;
}

function procsMsgHTML() {
  const m = Procs.msg;
  if (!m) return '';
  return `<div class="notice ${m.err ? 'err' : ''}" role="status"><span class="grow ${m.err ? 'err' : ''}">${esc(m.text)}</span><button class="btn sm ghost" type="button" data-act="procs-dismiss">Dismiss</button></div>`;
}

// One tree's buttons: pause or resume, end, kill. While a confirmation is open for it,
// the row shows what will happen and to how many processes instead.
function procsActionsHTML(key, pids, paused, label, system = false, self = false) {
  const c = Procs.confirm;
  if (c && c.key === key) {
    const [, verb, what] = PROC_ACTIONS[c.action];
    return `<span class="confirm procs-confirm"><span class="small">${esc(verb)} ${esc(label)} · ${plural(c.pids.length, 'process', 'processes')} · ${esc(what)}</span><button class="btn sm ghost" type="button" data-act="procs-cancel">Keep</button><button class="btn sm ${c.action === 'kill' || c.action === 'term' ? 'danger' : ''}" type="button" data-act="procs-do" aria-label="Confirm: ${esc(verb)} ${esc(label)}">${esc(PROC_ACTIONS[c.action][0])}</button></span>`;
  }
  if (self) return '<span class="meta procs-sys-note" title="Fusebox itself, serving this page">this server</span>';
  if (system) return '<span class="meta procs-sys-note" title="Part of macOS">system</span>';
  const busy = Procs.busy === key;
  const btn = (action, cls = '') => `<button class="btn sm ${cls}" type="button" data-act="procs-ask" data-id="${esc(key)}" data-action="${action}" data-label="${esc(label)}" title="${esc(PROC_ACTIONS[action][1])}" ${busy ? 'disabled' : ''}>${PROC_ACTIONS[action][0]}</button>`;
  return `<span class="procs-actions">${paused ? btn('resume') : btn('pause', 'ghost')}${btn('term')}${btn('kill', 'danger')}</span>`;
}

function procsAgentsHTML() {
  const d = Procs.data;
  if (!d?.available) return '';
  const list = procsSorted(d.agents);
  const head = '<div class="th"><span>Kind</span><span>Name</span><span>Status</span><span class="r">Sub</span><span class="r">CPU</span><span class="r">RAM</span><span class="r">Up</span><span></span></div>';
  const rows = list.map((a) => procsAgentRowHTML(a)).join('');
  return `<div class="sec-head"><h2 class="h-sec">Agents</h2><span class="meta">CLI agents, desktop AI apps and local model runtimes, each with everything it spawned</span></div>
    <div class="card table procs-table" role="table" aria-label="Agents">${head}${rows || `<div class="empty-line">${Procs.filter ? 'Nothing matches.' : 'No agent is running.'}</div>`}</div>`;
}

function procsAgentRowHTML(a) {
  const open = Procs.open.has(a.key);
  const name = S.private && a.cwd ? '••••••' : a.name && a.name !== '/' ? a.name : a.kind;
  const cwd = S.private || a.cwd === '/' ? '' : a.cwd || '';
  const tool = a.tool ? `${a.tool}${a.tool_since ? ` ${procsUp(a.tool_since)}` : ''}` : '';
  const alerts = (a.alerts || []).map((t) => `⚠ ${t}`).join(' · ');
  const status = `<span class="status ${procsStatusClass(a)}"><span class="dot s7 ${a.alerts?.length ? 'err' : a.paused ? 'idle' : a.status === 'busy' || a.status === 'running' ? 'ok' : a.status === 'waiting' ? 'warn' : 'idle'}"></span><span class="cell2"><span>${esc(a.paused ? 'paused' : a.status)}${a.subagents?.length ? '' : ''}</span>${tool || alerts ? `<span class="sub">${esc(alerts || tool)}</span>` : ''}</span></span>`;
  const subs = a.subagents?.length || 0;
  const kids = a.procs?.length || 0;
  const fold = kids || subs ? `<button class="procs-fold" type="button" data-act="procs-fold" data-id="${esc(a.key)}" aria-expanded="${open}" aria-label="${open ? 'Hide' : 'Show'} ${plural(kids, 'child process', 'child processes')}">${open ? '▾' : '▸'} ${kids}</button>` : '<span class="meta">—</span>';
  let out = `<div class="tr procs-row${a.alerts?.length ? ' alert' : ''}" data-key="${esc(a.key)}">
    <span class="idchip static procs-kind" title="${esc(a.bundle || a.kind)}">${esc(a.kind)}</span>
    <span class="cell2"><span class="procs-name">${esc(name)} <span class="meta mono">${a.pid}</span></span>${cwd ? `<span class="sub mono" title="${esc(cwd)}">${esc(cwd)}</span>` : ''}</span>
    ${status}
    <span class="r n">${subs || '—'}</span>
    <span class="r n ${a.tree_cpu >= 150 ? 'warn' : ''}">${procsPct(a.tree_cpu)}</span>
    <span class="r n ${a.tree_rss >= 2 * (1 << 30) ? 'warn' : ''}">${procsBytes(a.tree_rss)}</span>
    <span class="r n">${procsUp(a.started)}</span>
    <span class="r procs-cell-actions">${fold}${procsActionsHTML(a.key, a.pids || [], !!a.paused, a.name || a.kind)}</span></div>`;
  if (open) {
    for (const s of a.subagents || []) {
      out += `<div class="tr procs-child procs-sub"><span></span><span class="cell2" style="grid-column:2 / span 2"><span>◆ ${esc(s.type || 'subagent')} <span class="meta mono">${esc(String(s.id || '').slice(0, 8))}</span></span>${s.tool ? `<span class="sub">${esc(s.tool)}</span>` : ''}</span><span></span><span></span><span></span><span class="r n">${procsUp(s.since)}</span><span></span></div>`;
    }
    for (const p of a.procs || []) {
      const cmd = S.private ? p.name : p.cmd || p.name;
      out += `<div class="tr procs-child" style="--depth:${p.depth || 0}"><span></span><span class="cell2" style="grid-column:2 / span 2"><span class="mono procs-cmd" title="${esc(cmd)}">${'└ '}${esc(cmd)}</span><span class="sub mono">${p.pid}${p.stopped ? ' · stopped' : ''}</span></span><span></span><span class="r n">${procsPct(p.cpu)}</span><span class="r n">${procsBytes(p.rss)}</span><span></span><span class="r procs-cell-actions">${procsChildActionsHTML(a.key, p)}</span></div>`;
    }
  }
  return out;
}

function procsChildActionsHTML(agentKey, p) {
  const key = `${agentKey}/${p.pid}`;
  const c = Procs.confirm;
  if (c && c.key === key) return procsActionsHTML(key, [p.pid], p.stopped, p.name);
  return `<button class="btn sm ghost" type="button" data-act="procs-ask" data-id="${esc(key)}" data-action="term" data-pids="${p.pid}" data-label="${esc(p.name)}" title="End only this process (SIGTERM)">End</button><button class="btn sm danger" type="button" data-act="procs-ask" data-id="${esc(key)}" data-action="kill" data-pids="${p.pid}" data-label="${esc(p.name)}" title="Kill only this process (SIGKILL)">Kill</button>`;
}

function procsOthersHTML() {
  const d = Procs.data;
  if (!d?.available) return '';
  const q = Procs.filter.trim().toLowerCase();
  const list = d.others.filter((o) => !q || o.name.toLowerCase().includes(q));
  const head = '<div class="th"><span>Program</span><span class="r">Procs</span><span class="r">CPU</span><span class="r">RAM</span><span></span></div>';
  const rows = list.map((o) => `<div class="tr procs-other${o.system ? ' system' : ''}">
    <span class="cell2"><span>${esc(o.name)}</span>${o.exe && !S.private ? `<span class="sub mono" title="${esc(o.exe)}">${esc(o.exe)}</span>` : ''}</span>
    <span class="r n">${o.count}</span><span class="r n">${procsPct(o.cpu)}</span><span class="r n">${procsBytes(o.rss)}</span>
    <span class="r procs-cell-actions">${procsActionsHTML(`other:${o.name}`, o.pids || [], false, o.name, o.system, o.self)}</span></div>`).join('');
  const more = [6, 12, 25, 50].map((n) => `<button type="button" data-act="procs-others" data-id="${n}" aria-pressed="${Procs.others === n}">${n}</button>`).join('');
  return `<div class="sec-head"><h2 class="h-sec">Other programs</h2><span class="meta">Everything outside the agent trees, heaviest first</span><span class="grow"></span><div class="seg" role="group" aria-label="How many">${more}</div></div>
    <div class="card table procs-table others" role="table" aria-label="Other programs">${head}${rows || '<div class="empty-line">Nothing matches.</div>'}</div>`;
}

// ---------------------------------------------------------------- data

let procsTimer = 0;
let procsInflight = null;

function bindProcs() {
  procsLoad();
  clearInterval(procsTimer);
  procsTimer = setInterval(() => { if (S.route === 'procs' && !document.hidden) procsLoad(); }, PROCS_EVERY);
}

function procsLoad() {
  if (procsInflight || S.locked) return procsInflight;
  Procs.loading = true;
  procsInflight = api(`/procs?others=${Procs.others}`).then((d) => {
    Procs.data = d;
    Procs.error = null;
    Procs.at = Date.now();
  }).catch((e) => {
    Procs.error = e.message || 'Could not read the process table.';
  }).finally(() => {
    Procs.loading = false;
    procsInflight = null;
    if (S.route === 'procs') procsPaint();
  });
  return procsInflight;
}

function procsPaint() {
  if (S.route !== 'procs' || typeof patch !== 'function') return;
  // Not while a confirmation is open, so the buttons stay under the pointer.
  if (Procs.confirm) return;
  const count = document.getElementById('procs-count');
  if (count) count.textContent = procsCountText();
  patch('procs-sys', procsSystemHTML);
  patch('procs-msg', procsMsgHTML);
  patch('procs-agents', procsAgentsHTML);
  patch('procs-others', procsOthersHTML);
}

function procsAsk(el) {
  const { id: key, action, label } = el.dataset;
  const pids = el.dataset.pids ? el.dataset.pids.split(',').map(Number) : procsPidsOf(key);
  if (!pids.length) return;
  Procs.confirm = { key, action, pids, label };
  procsRepaintRows();
}

function procsPidsOf(key) {
  const d = Procs.data;
  if (!d) return [];
  if (key.startsWith('other:')) return d.others.find((o) => `other:${o.name}` === key)?.pids || [];
  return d.agents.find((a) => a.key === key)?.pids || [];
}

async function procsDo() {
  const c = Procs.confirm;
  if (!c) return;
  Procs.confirm = null;
  Procs.busy = c.key;
  procsRepaintRows();
  try {
    const r = await api('/procs/signal', { method: 'POST', body: JSON.stringify({ action: c.action, pids: c.pids }) });
    Procs.msg = { text: `${PROC_ACTIONS[c.action][1]} sent to ${c.label}: ${plural(r.sent ?? c.pids.length, 'process', 'processes')}.` };
    toast(`${PROC_ACTIONS[c.action][0]}: ${c.label}`);
  } catch (e) {
    Procs.msg = { err: true, text: `${PROC_ACTIONS[c.action][1]} ${c.label} failed: ${e.message}` };
  }
  Procs.busy = null;
  // Give the signal a moment to land, then read the table again.
  setTimeout(() => { Procs.busy = null; procsLoad(); }, 400);
  procsPaint();
}

function procsRepaintRows() {
  if (S.route !== 'procs' || typeof patch !== 'function') return;
  patch('procs-agents', procsAgentsHTML);
  patch('procs-others', procsOthersHTML);
}

if (typeof document !== 'undefined') {
  document.addEventListener('click', (e) => {
    const el = e.target.closest('[data-act^="procs-"]');
    if (!el) return;
    const { act, id } = el.dataset;
    switch (act) {
      case 'procs-ask': return procsAsk(el);
      case 'procs-cancel': Procs.confirm = null; return procsRepaintRows();
      case 'procs-do': return procsDo();
      case 'procs-dismiss': Procs.msg = null; return patch('procs-msg', procsMsgHTML);
      case 'procs-fold': Procs.open.has(id) ? Procs.open.delete(id) : Procs.open.add(id); return patch('procs-agents', procsAgentsHTML);
      case 'procs-sort': Procs.sort = id; return patch('procs-agents', procsAgentsHTML);
      case 'procs-others': Procs.others = Number(id); procsLoad(); return patch('procs-others', procsOthersHTML);
      case 'procs-clear-filter': Procs.filter = ''; { const input = document.getElementById('procs-filter'); if (input) input.value = ''; } return procsRepaintRows();
      default: return undefined;
    }
  });
  document.addEventListener('input', (e) => {
    if (e.target.id !== 'procs-filter') return;
    Procs.filter = e.target.value;
    Procs.confirm = null;
    procsRepaintRows();
  });
}
