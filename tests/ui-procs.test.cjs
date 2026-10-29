const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

// The Processes page, run from its own source: sizes, sorting and filtering, the rows it
// draws for an agent tree and for other programs, and the confirmation before a signal.
const procs = fs.readFileSync(path.join(__dirname, '../ui/procs.js'), 'utf8');
const app = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
function piece(start) {
  const at = app.indexOf(start);
  assert.ok(at >= 0, `app.js no longer has ${start}`);
  const end = start.startsWith('function') ? app.indexOf('\n}\n', at) + 2 : app.indexOf('\n', at);
  return app.slice(at, end);
}

const SNAPSHOT = {
  available: true, hook_seen: true,
  system: { load1: 7.2, num_cpu: 12, cpu_percent: 37.5, mem_used: 16 * 2 ** 30, mem_total: 18 * 2 ** 30, alerts: null },
  agents: [
    { key: 'claude:10', kind: 'claude', pid: 10, name: 'fusebox', cwd: '/Users/me/github/Fusebox', status: 'busy', tool: 'Bash: cargo build', tool_since: '2026-10-29T03:00:00Z',
      subagents: [{ id: 'a7097ec1', type: 'Explore', since: '2026-10-29T03:00:30Z', tool: 'Grep' }], tree_cpu: 184, tree_rss: 1.2 * 2 ** 30, started: '2026-10-29T01:00:00Z', pids: [10, 11, 12],
      procs: [{ pid: 11, name: 'zsh', cmd: 'zsh -c cargo build', cpu: 0, rss: 2 ** 20, depth: 0, stopped: false }, { pid: 12, name: 'rustc', cmd: 'rustc --crate-name fusebox', cpu: 180, rss: 2 ** 30, depth: 1, stopped: false }], alerts: ['CPU 184% for 45s'] },
    { key: 'hcodex:20', kind: 'hcodex', pid: 20, name: 'augur', cwd: '/Users/me/augur', status: 'idle', tool: '', subagents: [], tree_cpu: 0.5, tree_rss: 3.2 * 2 ** 30, started: '2026-10-29T02:00:00Z', pids: [20], procs: [], paused: true, alerts: [] },
  ],
  others: [
    { name: 'Arc.app', count: 54, cpu: 32, rss: 4 * 2 ** 30, pids: [100, 101], exe: '/Applications/Arc.app/Contents/MacOS/Arc', system: false },
    { name: 'trustd', count: 3, cpu: 0.1, rss: 10 * 2 ** 20, pids: [7], exe: '/usr/libexec/trustd', system: true },
    { name: 'fusebox', count: 1, cpu: 1, rss: 50 * 2 ** 20, pids: [999], exe: '/x/fusebox', system: true, self: true },
  ],
};

function load() {
  const context = { S: { private: false, route: 'procs', locked: null }, ICON: { search: '' }, console };
  vm.createContext(context);
  const helpers = [piece('const esc = (v) =>'), piece('function fmt(n) {'), piece('const plural = (n, word')].join('\n');
  vm.runInContext(`${helpers}\n${procs}`, context);
  // Values cross back through JSON, so they compare like this realm's own.
  return (code) => {
    const value = vm.runInContext(code, context);
    return value !== null && typeof value === 'object' ? JSON.parse(JSON.stringify(value)) : value;
  };
}

test('sizes, percentages and uptimes read the way top shows them', () => {
  const run = load();
  assert.equal(run('procsBytes(1.5 * 2 ** 30)'), '1.5G');
  assert.equal(run('procsBytes(300 * 2 ** 20)'), '300M');
  assert.equal(run('procsBytes(512 * 1024)'), '512K');
  assert.equal(run('procsPct(184.4)'), '184%');
  assert.equal(run('procsPct(12.6)'), '13%');
  assert.equal(run('procsPct(0.55)'), '0.6%');
  const now = Date.parse('2026-10-29T03:05:00Z');
  assert.equal(run(`procsUp('2026-10-29T03:04:20Z', ${now})`), '40s');
  assert.equal(run(`procsUp('2026-10-29T01:00:00Z', ${now})`), '2h05');
  assert.equal(run(`procsUp('2026-10-27T01:00:00Z', ${now})`), '2d2h');
  assert.equal(run('procsUp(null)'), '');
});

test('agents sort by CPU, RAM or start, and the filter reaches directories and commands', () => {
  const run = load();
  run(`Procs.data = ${JSON.stringify(SNAPSHOT)}`);
  assert.deepEqual(run('procsSorted(Procs.data.agents, "cpu").map((a) => a.key)'), ['claude:10', 'hcodex:20']);
  assert.deepEqual(run('procsSorted(Procs.data.agents, "ram").map((a) => a.key)'), ['hcodex:20', 'claude:10']);
  assert.deepEqual(run('procsSorted(Procs.data.agents, "start").map((a) => a.key)'), ['hcodex:20', 'claude:10']);
  assert.deepEqual(run('procsSorted(Procs.data.agents, "cpu", "augur").map((a) => a.key)'), ['hcodex:20']);
  assert.deepEqual(run('procsSorted(Procs.data.agents, "cpu", "rustc").map((a) => a.key)'), ['claude:10'], 'a child command matches');
  assert.deepEqual(run('procsSorted(Procs.data.agents, "cpu", "zzz")'), []);
});

test('the page shows the machine, each agent tree and the other programs', () => {
  const run = load();
  run(`Procs.data = ${JSON.stringify(SNAPSHOT)}`);
  const html = run('procsHTML()');
  assert.match(html, /2 agents · 1 busy · 4 processes in their trees/);
  assert.match(html, /38%<\/b><span>CPU busy/);
  assert.match(html, /16\.0G <span class="meta">\/ 18\.0G<\/span><\/b><span>memory, 89% used/);
  assert.match(html, /fusebox <span class="meta mono">10<\/span>/);
  assert.match(html, /⚠ CPU 184% for 45s/, 'an alert replaces the tool line');
  assert.match(html, /data-act="procs-ask" data-id="claude:10" data-action="kill"/);
  assert.match(html, /data-action="resume"/, 'a paused tree offers Resume');
  assert.match(html, /procs-other system/);
  assert.match(html, /title="Part of macOS">system/, 'OS daemons have no buttons');
  assert.doesNotMatch(html, /data-id="other:trustd"/);
  assert.match(html, />this server</, 'Fusebox itself is named, with no buttons');
  assert.doesNotMatch(html, /data-id="other:fusebox"/);
  assert.match(html, /data-id="other:Arc.app" data-action="term"/);
  assert.doesNotMatch(html, /rustc --crate-name/, 'children are folded by default');
  run('Procs.open.add("claude:10")');
  const open = run('procsAgentsHTML()');
  assert.match(open, /└ rustc --crate-name fusebox/);
  assert.match(open, /◆ Explore <span class="meta mono">a7097ec1/);
  assert.match(open, /data-id="claude:10\/12" data-action="kill" data-pids="12"/, 'one child can be killed alone');
});

test('privacy hides names, directories and paths', () => {
  const run = load();
  run(`Procs.data = ${JSON.stringify(SNAPSHOT)}; S.private = true`);
  const html = run('procsHTML()');
  assert.doesNotMatch(html, /github\/Fusebox/);
  assert.doesNotMatch(html, /Applications\/Arc/);
  assert.match(html, /••••••/);
});

test('a signal needs a confirmation that names the tree and how many processes', () => {
  const run = load();
  run(`Procs.data = ${JSON.stringify(SNAPSHOT)}`);
  run('procsAsk({ dataset: { id: "claude:10", action: "kill", label: "fusebox" } })');
  assert.deepEqual(run('Procs.confirm'), { key: 'claude:10', action: 'kill', pids: [10, 11, 12], label: 'fusebox' });
  const html = run('procsAgentsHTML()');
  assert.match(html, /Kill \(SIGKILL\) fusebox · 3 processes · ends every process/);
  assert.match(html, /data-act="procs-do"/);
  assert.match(html, /data-act="procs-cancel"/);
  run('procsAsk({ dataset: { id: "other:Arc.app", action: "term", label: "Arc.app" } })');
  assert.deepEqual(run('Procs.confirm.pids'), [100, 101]);
  run('procsAsk({ dataset: { id: "claude:10/12", action: "term", label: "rustc", pids: "12" } })');
  assert.deepEqual(run('Procs.confirm.pids'), [12]);
});

test('when the table cannot be read the page says why', () => {
  const run = load();
  run('Procs.data = { available: false, error: "The process table is only read on macOS." }');
  const html = run('procsHTML()');
  assert.match(html, /The process table is only read on macOS\./);
  assert.match(html, /The process table could not be read\./);
  assert.doesNotMatch(html, /Other programs/);
});
