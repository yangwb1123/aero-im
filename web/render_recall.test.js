// render_recall.test.js — recall (撤回) UI surface: placeholder rendering,
// terminal-state suppression of actions, and author-only recall affordance.
import test from 'node:test';
import assert from 'node:assert/strict';

import { renderMessage } from './render.js';

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

function collectButtons(node) {
  const out = [];
  if (node.tagName === 'button') out.push(node);
  for (const child of node.children) out.push(...collectButtons(child));
  return out;
}

const baseMessage = {
  id: 'message-1',
  room_id: 'room-a',
  sender_id: 'participant-1',
  created_at: '2026-08-06T00:00:00Z',
  blocks: [{ type: 'text', content: 'original body' }],
  version: 1,
};

test('a recalled message renders the placeholder with a badge and no actions', () => {
  withFakeDocument(() => {
    const recalled = {
      ...baseMessage,
      recalled_at: '2026-08-06T00:01:00Z',
      recalled_by: 'participant-1',
      version: 2,
      blocks: [{ type: 'text', content: '[此消息已被撤回]' }],
    };
    const rendered = renderMessage(recalled, 'participant-1', new Map());
    assert.match(rendered.className, /\brecalled\b/, 'recalled bubble class');

    const text = allText(rendered);
    assert.match(text, /· 已撤回/, 'recalled badge');
    assert.match(text, /\[此消息已被撤回\]/, 'placeholder body rendered');
    assert.doesNotMatch(text, /original body/, 'original content is gone');
    assert.doesNotMatch(
      text,
      /消息已删除/,
      'recall is a placeholder, not a tombstone',
    );

    const actions = collectButtons(rendered).map((b) => b.dataset.action);
    assert.deepEqual(
      actions,
      [],
      'recalled messages are terminal: no react/reply/edit/recall/delete',
    );
  });
});

test('the recall action is exposed for the author only', () => {
  withFakeDocument(() => {
    const self = renderMessage(baseMessage, 'participant-1', new Map());
    const selfActions = collectButtons(self).map((b) => b.dataset.action);
    assert.ok(selfActions.includes('recall'), 'author sees the recall button');
    assert.ok(selfActions.includes('edit'));

    const other = renderMessage(baseMessage, 'participant-2', new Map());
    const otherActions = collectButtons(other).map((b) => b.dataset.action);
    assert.ok(
      !otherActions.includes('recall'),
      'non-author has no recall affordance',
    );
    assert.ok(!otherActions.includes('edit'));
    assert.ok(otherActions.includes('react'), 'non-authors keep passive actions');
  });
});
