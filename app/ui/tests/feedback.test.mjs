// Unit tests for the pure helpers in feedback.js. Run with: node --test app/ui/tests/*.test.mjs
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

const src = readFileSync(new URL('../feedback.js', import.meta.url), 'utf8').replace(/\r\n/g, '\n');
const grab = name => { const s = src.indexOf(`const ${name} =`); assert.ok(s >= 0, name); return src.slice(s, src.indexOf(';\n', s) + 1); };
const { feedbackPayload, feedbackReady } = new Function(['feedbackPayload', 'feedbackReady'].map(grab).join('\n') + '\nreturn { feedbackPayload, feedbackReady };')();

test('send is allowed with stars, words or both', () => {
  assert.equal(feedbackReady(0, '   '), false);
  assert.equal(feedbackReady(4, ''), true);
  assert.equal(feedbackReady(0, 'Add a schedule'), true);
});

test('payload trims text and leaves out empty fields', () => {
  assert.deepEqual(feedbackPayload(0, 'issue', '  Crash  ', ' '), { rating: null, category: 'issue', message: 'Crash', email: null });
  assert.deepEqual(feedbackPayload(5, 'praise', '', 'a@b.co'), { rating: 5, category: 'praise', message: '', email: 'a@b.co' });
});
