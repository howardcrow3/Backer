// Unit tests for the pure helpers in feedback.js. Run with: node --test app/ui/tests/*.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const src = readFileSync(new URL('../feedback.js', import.meta.url), 'utf8').replace(/\r\n/g, '\n');
const grab = name => { const s = src.indexOf(`const ${name} =`); assert.ok(s >= 0, name); return src.slice(s, src.indexOf(';\n', s) + 1); };
const { feedbackPayload, feedbackReady, feedbackEmailOk } = new Function(['feedbackPayload', 'feedbackReady', 'feedbackEmailOk'].map(grab).join('\n') + '\nreturn { feedbackPayload, feedbackReady, feedbackEmailOk };')();

test('send is allowed with stars, words or both', () => {
  assert.equal(feedbackReady(0, '   '), false);
  assert.equal(feedbackReady(4, ''), true);
  assert.equal(feedbackReady(0, 'Add a schedule'), true);
});

test('payload trims text and leaves out empty fields', () => {
  assert.deepEqual(feedbackPayload(0, 'issue', '  Crash  ', ' '), { rating: null, category: 'issue', message: 'Crash', email: null, anonymous: false });
  assert.deepEqual(feedbackPayload(5, 'praise', '', 'a@b.co'), { rating: 5, category: 'praise', message: '', email: 'a@b.co', anonymous: false });
});

test('anonymous leaves the email out', () => {
  assert.deepEqual(feedbackPayload(3, 'other', 'hi', 'me@example.com', true), { rating: 3, category: 'other', message: 'hi', email: null, anonymous: true });
});

test('email format check matches the service', () => {
  for (const ok of ['', '  ', 'a@b.co', 'first.last+tag@mail.example.com']) assert.equal(feedbackEmailOk(ok), true, ok);
  for (const bad of ['nope', 'a@b', 'a@b.c', 'a b@c.co', 'a@b..co', '.a@b.co', 'a..b@c.co', 'a.@b.co', 'a@-b.co', 'a@b@c.co']) assert.equal(feedbackEmailOk(bad), false, bad);
});
