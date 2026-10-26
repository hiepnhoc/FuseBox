const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

// The model picker of the "Add an API key" form: the server's list becomes chips,
// clicking one adds or removes it in the Models field, and aliases are kept.
const app = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
function piece(start) {
  const at = app.indexOf(start);
  assert.ok(at >= 0, `app.js no longer has ${start}`);
  const end = start.startsWith('function') ? app.indexOf('\n}\n', at) + 2 : app.indexOf('\n', at);
  return app.slice(at, end);
}

function load(models, list) {
  const input = { value: models };
  const chips = [];
  const context = {
    S: { keyModels: { status: 'done', list, host: 'api.example', filter: '' } },
    $: (sel) => (sel.includes('name="models"') ? input : null),
    $$: () => chips,
    input, chips,
  };
  vm.createContext(context);
  const code = [piece('const esc = (v) =>'), piece('function keyModelsHTML() {'), piece('function keyModelChipsHTML() {'),
    piece('const keyModelEntries = () =>'), piece('const upstreamOf = (entry) =>'), piece('const pickedKeyModels = () =>'),
    piece('function pickKeyModel(id) {'), piece('function syncKeyModelChips() {')].join('\n');
  vm.runInContext(code, context);
  return (s) => vm.runInContext(s, context);
}

test('chips mark the models already in the field, upstream names of aliases included', () => {
  const run = load('kimi=moonshotai/kimi-k3, mercury-2', ['mercury-2', 'mercury-3', 'moonshotai/kimi-k3']);
  const html = run('keyModelChipsHTML()');
  assert.match(html, /data-id="mercury-2" aria-pressed="true"/);
  assert.match(html, /data-id="mercury-3" aria-pressed="false"/);
  assert.match(html, /data-id="moonshotai\/kimi-k3" aria-pressed="true"/);
});

test('the filter narrows the chips and says when nothing matches', () => {
  const run = load('', ['mercury-2', 'mercury-3', 'llama3:8b']);
  run('S.keyModels.filter = "MERC"');
  const html = run('keyModelChipsHTML()');
  assert.equal((html.match(/data-act="pick-model"/g) || []).length, 2);
  assert.doesNotMatch(html, /llama3/);
  run('S.keyModels.filter = "zzz"');
  assert.match(run('keyModelChipsHTML()'), /No model matches/);
});

test('picking a chip adds the model, picking it again removes it, aliases survive', () => {
  const run = load('kimi=moonshotai/kimi-k3', ['mercury-2', 'moonshotai/kimi-k3']);
  run('pickKeyModel("mercury-2")');
  assert.equal(run('input.value'), 'kimi=moonshotai/kimi-k3, mercury-2');
  run('pickKeyModel("mercury-2")');
  assert.equal(run('input.value'), 'kimi=moonshotai/kimi-k3');
  run('pickKeyModel("moonshotai/kimi-k3")');
  assert.equal(run('input.value'), '', 'removing by upstream name drops the aliased entry too');
});

test('the list header and loading state render', () => {
  const run = load('', Array.from({ length: 13 }, (_, i) => `m${i}`));
  const html = run('keyModelsHTML()');
  assert.match(html, /api\.example/);
  assert.match(html, /13 models/);
  assert.match(html, /name="key-model-filter"/, 'a filter box appears once the list is long');
  run('S.keyModels = { status: "loading" }');
  assert.match(run('keyModelsHTML()'), /Asking the server/);
  run('S.keyModels = null');
  assert.equal(run('keyModelsHTML()'), '');
});
