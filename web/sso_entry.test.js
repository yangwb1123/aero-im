import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

// The server's OIDC callback returns an HTML page that loads
// `/oidc_callback.js` to move the session into localStorage. It lives at the
// package root, so nothing emits it unless the build says so — and a 404 there
// strands the user after a *successful* Snaplink login.
test('the built bundle ships oidc_callback.js at the path the server serves', async () => {
  const built = await readFile(new URL('./dist/oidc_callback.js', import.meta.url), 'utf8');
  const source = await readFile(new URL('./oidc_callback.js', import.meta.url), 'utf8');
  assert.equal(built, source, 'dist/oidc_callback.js must match the source file');
  for (const key of ['aero_token', 'aero_refresh', 'aero_pid']) {
    assert.ok(built.includes(key), `callback must persist ${key}`);
  }
});

test('vite emits oidc_callback.js instead of leaving it in the package root', async () => {
  const config = await readFile(new URL('./vite.config.ts', import.meta.url), 'utf8');
  assert.match(config, /emitFile\(\{[^}]*fileName:\s*name/, 'vite.config.ts must emit oidc_callback.js');
});

test('the SPA offers the Snaplink entry point the server advertises', async () => {
  const app = await readFile(new URL('./src/App.tsx', import.meta.url), 'utf8');
  assert.match(app, /\/api\/auth\/oidc\/start/, 'must navigate to the server-side OIDC start route');
  assert.match(app, /login_page === 'snaplink'/, 'must honour PublicAuthConfig.login_page');
});
