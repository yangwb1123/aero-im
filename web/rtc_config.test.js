import assert from 'node:assert/strict';
import test from 'node:test';

import { browserRtcConfig } from './rtc_config.js';

test('RTC config maps snake_case TURN relay policy into browser fields', () => {
  const turn = {
    urls: ['turn:127.0.0.1:3478'],
    username: 'local-user',
    credential: 'local-password',
  };
  assert.deepEqual(browserRtcConfig({
    ice_servers: [turn],
    ice_transport_policy: 'relay',
  }), {
    iceServers: [turn],
    iceTransportPolicy: 'relay',
  });
});

test('RTC config accepts shared camelCase shape and rejects invalid policy', () => {
  assert.deepEqual(browserRtcConfig({
    iceServers: [{ urls: ['stun:127.0.0.1:3478'] }],
    iceTransportPolicy: 'invalid',
  }), {
    iceServers: [{ urls: ['stun:127.0.0.1:3478'] }],
  });
});

test('RTC config has a usable fallback when the endpoint is unavailable', () => {
  assert.deepEqual(browserRtcConfig(null), {
    iceServers: [{ urls: 'stun:stun.l.google.com:19302' }],
  });
});
