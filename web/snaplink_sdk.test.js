import test from 'node:test';
import assert from 'node:assert/strict';
import { SSOClient, SSOError } from './snaplink_sdk.js';

test('Snaplink SDK adapter preserves generated operation paths and JSON bodies', async () => {
  const calls = [];
  const client = new SSOClient({
    baseUrl: 'https://sso.example.test/',
    clientId: 'im-demo',
    fetch: async (url, options) => {
      calls.push({ url, options });
      return new Response(JSON.stringify({ code: 'ac_test' }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      });
    },
  });

  const body = { provider: 'password', client_id: 'im-demo', response_type: 'code' };
  assert.deepEqual(await client.postLogin(body), { code: 'ac_test' });
  assert.equal(calls[0].url, 'https://sso.example.test/auth/login');
  assert.equal(calls[0].options.method, 'POST');
  assert.deepEqual(JSON.parse(calls[0].options.body), body);
  assert.equal(calls[0].options.credentials, 'omit');
});

test('generated login convenience operation sends a direct password request', async () => {
  const calls = [];
  const client = new SSOClient({
    baseUrl: 'https://sso.example.test',
    clientId: 'im-demo',
    fetch: async (url, options) => {
      calls.push({ url, options });
      return new Response(JSON.stringify({ access_token: 'at_test', id_token: 'id_test' }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      });
    },
  });
  const response = await client.login('alice@example.test', 'password', {
    scope: ['openid', 'profile', 'email'],
  });
  assert.equal(response.id_token, 'id_test');
  assert.equal(client.isLoggedIn, true);
  assert.equal(client.accessToken, 'at_test');
  assert.deepEqual(JSON.parse(calls[0].options.body), {
    provider: 'password',
    client_id: 'im-demo',
    scope: ['openid', 'profile', 'email'],
    credential: { username: 'alice@example.test', password: 'password' },
  });
});

test('Snaplink SDK adapter turns error responses into SSOError', async () => {
  const client = new SSOClient({
    baseUrl: 'https://sso.example.test',
    fetch: async () => new Response(JSON.stringify({
      error: 'invalid_credentials',
      error_description: 'invalid credentials',
    }), {
      status: 401,
      headers: { 'content-type': 'application/json' },
    }),
  });
  await assert.rejects(
    () => client.postLogin({ provider: 'password' }),
    (error) => error instanceof SSOError
      && error.status === 401
      && error.error === 'invalid_credentials',
  );
});
