// Unit tests for the integrity card in integrity.js. Run with: node --test app/ui/tests/*.test.mjs
// integrity.js is a browser script, so it is evaluated here with the few globals it uses stubbed.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const src = readFileSync(new URL('../integrity.js', import.meta.url), 'utf8').replace(/\r\n/g, '\n');
const stubs = `
  const esc = s => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
  const fmtN = n => Number(n || 0).toLocaleString('en-US');
  const fmtBytes = n => n + ' B';
  const fmtTime = s => 'T' + s;
  const document = { addEventListener() {}, querySelector() { return null; } };
  const setInterval = () => 0;
  const call = async () => ({}), render = () => {}, toast = () => {}, $ = () => null;`;
const ui = new Function(`${stubs}\n${src}\nreturn integrityUi;`)();

const report = o => ({ total: 10, checked: 10, ok: 10, bytes: 1000, damaged: [], damaged_total: 0, missing: [], missing_total: 0, unreadable: [], unreadable_total: 0, cancelled: false, duration_ms: 1500, ...o });
const job = o => ({ schedule: { kind: 'manual' }, next: null, last: null, running: false, files: 10, ...o });

test('schedule choices map to what the agent stores and back', () => {
  assert.equal(ui.choiceOf({ kind: 'manual' }), 'off');
  assert.equal(ui.choiceOf(ui.SCHEDULES.weekly), 'weekly');
  assert.equal(ui.choiceOf(ui.SCHEDULES.monthly), 'monthly');
  assert.equal(ui.choiceOf({ kind: 'daily', hour: 1, minute: 0 }), 'other');
  assert.equal(ui.choiceOf({ kind: 'weekly', days: [1, 7], hour: 3, minute: 0 }), 'other');
  // The interval schedules are refused by the agent, so the page never offers one.
  assert.ok(Object.values(ui.SCHEDULES).every(s => s.kind !== 'every_minutes'));
});

test('a job that was never checked says so and offers the check', () => {
  const h = ui.card(job(), 7);
  assert.match(h, /Not checked yet/);
  assert.match(h, /data-icheck="7"/);
  assert.doesNotMatch(h, /data-idetail/);
});

test('a job with nothing backed up yet cannot be checked', () => {
  assert.match(ui.card(job({ files: 0 }), 1), /data-icheck="1" disabled/);
});

test('a healthy result reads as good news and a damaged one says what is wrong', () => {
  const ok = ui.card(job({ last: { status: 'ok', started: 5, trigger: 'schedule', report: report() } }), 1);
  assert.match(ok, /All intact/);
  assert.match(ok, /10 files read back from the NAS and every one matches/);
  assert.match(ok, /scheduled/);
  const bad = ui.card(job({ last: { status: 'problems', started: 5, trigger: 'manual', report: report({ ok: 6, damaged_total: 3, missing_total: 1 }) } }), 1);
  assert.match(bad, /Problems found/);
  assert.match(bad, /3 damaged, 1 missing of 10 files checked/);
  assert.match(bad, /data-idetail="1"/);
});

test('a check that is running shows progress and Stop, and cannot be changed or started again', () => {
  const h = ui.card(job({ running: true }), 2);
  assert.match(h, /data-iprog/);
  assert.match(h, /data-icancel/);
  assert.doesNotMatch(h, /data-icheck/);
  assert.match(h, /id="isched"[^>]*disabled/);
});

test('the detail dialog names files, escapes them and says the NAS was not changed', () => {
  const html = ui.detailHtml({ status: 'problems', message: null, report: report({ damaged: ['mac/<b>.txt'], damaged_total: 1, missing: ['mac/gone'], missing_total: 3 }) });
  assert.match(html, /1 damaged copy/);
  assert.match(html, /mac\/&lt;b&gt;\.txt/);
  assert.match(html, /and 2 more/);
  assert.match(html, /Nothing on the NAS was changed/);
});

test('a failed check shows why instead of a clean result', () => {
  const h = ui.card(job({ last: { status: 'failed', started: 5, trigger: 'manual', message: 'destination not ready: marker missing', report: report({ ok: 0, checked: 0 }) } }), 1);
  assert.match(h, /Could not finish/);
  assert.match(h, /destination not ready: marker missing/);
  assert.doesNotMatch(h, /All intact/);
});
