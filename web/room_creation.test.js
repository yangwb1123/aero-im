import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

test('generic room creation offers group and channel but not direct', async () => {
  const html = await readFile(new URL('./index.html', import.meta.url), 'utf8');
  const form = html.match(/<form id="form-new-room"[\s\S]*?<\/form>/)?.[0];
  assert.ok(form, 'new-room form exists');
  assert.match(form, /<option value="group">/);
  assert.match(form, /<option value="channel">/);
  assert.doesNotMatch(
    form,
    /<option value="direct">/,
    '1:1 conversations must use the dedicated DM flow',
  );
});
