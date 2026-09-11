import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

test('root page is the SolidJS entry and does not load the legacy app', async () => {
  const html = await readFile(new URL('./index.html', import.meta.url), 'utf8');
  assert.match(html, /<script\s+type="module"\s+src="\/src\/main\.tsx"><\/script>/);
  assert.doesNotMatch(html, /src="app\.js"/);
});
