// Unit tests for the schedule text and wizard schedule helpers in app.js. Run with: node --test app/ui/tests/*.test.mjs
// app.js is a browser script, so the pure helpers are pulled out by name and evaluated on their own.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const src = readFileSync(new URL('../app.js', import.meta.url), 'utf8');
const grab = name => {
  const start = src.indexOf(`const ${name} =`);
  assert.ok(start >= 0, `${name} not found in app.js`);
  const end = src.indexOf(';\n', start);
  return src.slice(start, end + 1);
};
const { schedText, wizardSchedule } = new Function(
  ['hhmm', 'ordinal', 'monthDayText', 'schedText', 'wizardSchedule'].map(grab).join('\n') + '\nreturn { schedText, wizardSchedule };')();

test('schedule text for every kind', () => {
  assert.equal(schedText({ kind: 'manual' }), 'Run now only');
  assert.equal(schedText({ kind: 'every_minutes', minutes: 15 }), 'Every 15 min');
  assert.equal(schedText({ kind: 'every_hours', hours: 6 }), 'Every 6 h');
  assert.equal(schedText({ kind: 'daily', hour: 2, minute: 5 }), 'Daily at 02:05');
  assert.equal(schedText({ kind: 'weekly', days: [1], hour: 23, minute: 0 }), 'Weekly at 23:00');
  assert.equal(schedText({ kind: 'monthly', day: 1, hour: 3, minute: 0 }), 'Monthly on the 1st at 03:00');
  assert.equal(schedText({ kind: 'monthly', day: 2, hour: 3, minute: 0 }), 'Monthly on the 2nd at 03:00');
  assert.equal(schedText({ kind: 'monthly', day: 13, hour: 3, minute: 0 }), 'Monthly on the 13th at 03:00');
  assert.equal(schedText({ kind: 'monthly', day: 22, hour: 3, minute: 0 }), 'Monthly on the 22nd at 03:00');
  assert.equal(schedText({ kind: 'monthly', day: 31, hour: 23, minute: 30 }), 'Monthly on the 31st (or the last day) at 23:30');
});

test('unknown kinds do not crash the job list', () => {
  assert.equal(schedText({ kind: 'something_new' }), 'something_new');
});

test('wizard builds the JSON the agent expects', () => {
  const w = { sched: 'every_minutes', minutes: 15, hours: 6, hour: 2, minute: 0, mday: 31, days: [1, 2] };
  assert.deepEqual(wizardSchedule(w), { kind: 'every_minutes', minutes: 15 });
  assert.deepEqual(wizardSchedule({ ...w, sched: 'monthly' }), { kind: 'monthly', day: 31, hour: 2, minute: 0 });
  assert.deepEqual(wizardSchedule({ ...w, sched: 'weekly' }), { kind: 'weekly', days: [1, 2], hour: 2, minute: 0 });
  assert.deepEqual(wizardSchedule({ ...w, sched: 'manual' }), { kind: 'manual' });
});
