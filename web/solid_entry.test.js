import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile, readdir } from 'node:fs/promises';

test('root page is the SolidJS entry and does not load the legacy app', async () => {
  const html = await readFile(new URL('./index.html', import.meta.url), 'utf8');
  const moduleEntries = [...html.matchAll(/<script\b[^>]*type="module"[^>]*src="([^"]+)"[^>]*><\/script>/g)]
    .map((match) => match[1]);
  assert.deepEqual(moduleEntries, ['/src/main.tsx']);
  assert.doesNotMatch(html, /src="app\.js"/);
  const entries = await readdir(new URL('.', import.meta.url));
  assert.equal(entries.includes('solid'), false, 'the app must stay in web/src without a parallel web/solid tree');
});
