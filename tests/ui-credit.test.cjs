const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');
const vm = require('node:vm');

const source = fs.readFileSync(path.join(__dirname, '../ui/app.js'), 'utf8');
function piece(start) {
  const at = source.indexOf(start);
  assert.ok(at >= 0, start);
  return source.slice(at, source.indexOf('\n}\n', at) + 2);
}
function dashboard() {
  const context = {
    liveLeft: (iso) => `<t>${iso}</t>`, esc: (s) => String(s ?? ''),
    SIGNIN_ERR: /unauthori[sz]ed/i,
  };
  vm.createContext(context);
  vm.runInContext(piece('function acctState('), context);
  return (a) => vm.runInContext('acctState(a)', Object.assign(context, { a }));
}

test('a key its provider says is out of credit reads so, in red, until it is tried again', () => {
  const state = dashboard();
  const until = '2026-10-29T12:00:00+00:00';
  const paused = state({ id: 'k', kind: 'api-key', cooldowns: { '*': until }, cooldown_kinds: { '*': 'credit' }, last_error: 'out of credit: Insufficient credits' });
  assert.equal(paused.cls, 'cooling');
  assert.equal(paused.dot, 'err', 'money, not a passing limit');
  assert.equal(paused.word, 'Out of credit');
  assert.equal(paused.html, `Out of credit · <t>${until}</t>`, 'how long until it is tried again');
  assert.equal(paused.kind, 'credit');
  // A rate limit on the same shape stays an amber cooldown.
  const limited = state({ id: 'k', kind: 'api-key', cooldowns: { 'gpt-6-sol': until }, cooldown_kinds: { 'gpt-6-sol': 'rate_limit' } });
  assert.deepEqual([limited.word, limited.dot, limited.kind], ['Cooling', 'warn', 'rate_limit']);
  // With the pause over, only the error would remain; here it ended with the pause.
  assert.equal(state({ id: 'k', kind: 'api-key', cooldowns: {}, cooldown_kinds: {} }).word, 'Ready');
});
