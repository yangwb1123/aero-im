import test from 'node:test';
import assert from 'node:assert/strict';

import {
  buildGiphyCard,
  isSafeGiphyMediaUrl,
  isSafeGiphyPageUrl,
  normalizeGiphyPayload,
  renderMessage,
} from './render.js';

class FakeNode {
  constructor(tagName) {
    this.tagName = tagName;
    this.attributes = new Map();
    this.children = [];
    this.className = '';
    this.dataset = {};
    this.textContent = '';
  }

  appendChild(child) {
    this.children.push(child);
    return child;
  }

  setAttribute(name, value) {
    this.attributes.set(name, value);
  }
}

function withFakeDocument(run) {
  const previous = globalThis.document;
  globalThis.document = {
    createElement: (tag) => new FakeNode(tag),
    createDocumentFragment: () => new FakeNode('#fragment'),
  };
  try {
    return run();
  } finally {
    globalThis.document = previous;
  }
}

function allText(node) {
  return [node.textContent, ...node.children.map(allText)].join(' ');
}

test('GIPHY media URLs require HTTPS, an official host, and an image rendition', () => {
  assert.equal(
    isSafeGiphyMediaUrl('https://media2.giphy.com/media/abc/200.gif?cid=aero'),
    true,
  );
  assert.equal(isSafeGiphyMediaUrl('https://i.giphy.com/abc.webp'), true);
  for (const url of [
    'http://media.giphy.com/a.gif',
    'https://giphy.com.evil.test/a.gif',
    'https://user:pass@media.giphy.com/a.gif',
    'https://media.giphy.com/a.svg',
    'javascript:alert(1)',
  ]) {
    assert.equal(isSafeGiphyMediaUrl(url), false, url);
  }
});

test('GIPHY page links use the same origin and credential boundary', () => {
  assert.equal(isSafeGiphyPageUrl('https://giphy.com/gifs/dancing-cat-abc'), true);
  assert.equal(isSafeGiphyPageUrl('https://www.giphy.com/gifs/abc'), true);
  assert.equal(isSafeGiphyPageUrl('https://giphy.com.evil.test/gifs/abc'), false);
  assert.equal(isSafeGiphyPageUrl('https://user@giphy.com/gifs/abc'), false);
});

test('GIPHY payload normalization is total for malformed JSON shapes', () => {
  const expected = {
    title: 'GIF',
    attribution: 'Powered by GIPHY',
    imageUrl: '',
    sourceUrl: '',
    width: null,
    height: null,
  };
  const malformed = [
    null,
    [],
    'not-an-object',
    42,
    true,
    {
      title: [],
      query: { nested: true },
      attribution: false,
      image_url: ['https://media.giphy.com/a.gif'],
      source_url: { href: 'https://giphy.com/gifs/a' },
      width: {},
      height: Symbol('bad'),
    },
  ];
  for (const payload of malformed) {
    assert.deepEqual(normalizeGiphyPayload(payload), expected);
  }

  const { proxy, revoke } = Proxy.revocable({}, {});
  revoke();
  assert.deepEqual(normalizeGiphyPayload(proxy), expected);
});

test('GIPHY payload normalization preserves a valid bounded card', () => {
  assert.deepEqual(
    normalizeGiphyPayload({
      title: 'Dancing cat',
      image_url: 'https://media.giphy.com/media/abc/200.gif',
      source_url: 'https://giphy.com/gifs/abc',
      width: '320',
      height: 180,
      attribution: 'Via GIPHY',
    }),
    {
      title: 'Dancing cat',
      attribution: 'Via GIPHY',
      imageUrl: 'https://media.giphy.com/media/abc/200.gif',
      sourceUrl: 'https://giphy.com/gifs/abc',
      width: 320,
      height: 180,
    },
  );
});

test('GIPHY DOM rendering degrades malformed cards without throwing', () => {
  withFakeDocument(() => {
    for (const payload of [null, [], 'bad', { title: {}, image_url: {} }]) {
      const card = buildGiphyCard(payload);
      assert.match(allText(card), /GIF unavailable/);

      const message = {
        id: 'message-1',
        sender_id: 'participant-1',
        created_at: null,
        blocks: [{ type: 'card', schema: 'giphy', payload }],
      };
      const rendered = renderMessage(
        message,
        'participant-2',
        new Map([['participant-1', { display_name: 'Sender' }]]),
      );
      assert.match(allText(rendered), /GIF unavailable/);
    }
  });
});
