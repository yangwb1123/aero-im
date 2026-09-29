import test from 'node:test'
import assert from 'node:assert/strict'
import {
  canvasCreateIntentStorageKey,
  clearCanvasCreateIntent,
  loadCanvasCreateIntent,
  saveCanvasCreateIntent,
} from './src/canvas_create_intents.js'

const intent = {
  title: 'Board after reload',
  clientCreateId: '0198c123-4567-7abc-8def-0123456789ab',
}

function memoryStorage() {
  const values = new Map()
  return {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => values.set(key, value),
    removeItem: (key) => values.delete(key),
  }
}

test('Canvas create intent survives reload and is only cleared by its matching operation id', () => {
  const storage = memoryStorage()
  const key = canvasCreateIntentStorageKey('participant /1', 'room #1')
  assert.notEqual(key, canvasCreateIntentStorageKey('participant /2', 'room #1'))
  assert.notEqual(key, canvasCreateIntentStorageKey('participant /1', 'room #2'))

  assert.equal(saveCanvasCreateIntent(storage, 'participant /1', 'room #1', intent), true)
  assert.deepEqual(loadCanvasCreateIntent(storage, 'participant /1', 'room #1'), intent)
  assert.equal(clearCanvasCreateIntent(storage, 'participant /1', 'room #1', 'other-operation'), false)
  assert.deepEqual(loadCanvasCreateIntent(storage, 'participant /1', 'room #1'), intent)
  assert.equal(clearCanvasCreateIntent(storage, 'participant /1', 'room #1', intent.clientCreateId), true)
  assert.equal(loadCanvasCreateIntent(storage, 'participant /1', 'room #1'), null)
})

test('Canvas create intent loader discards malformed or unsafe stored records', () => {
  const storage = memoryStorage()
  const key = canvasCreateIntentStorageKey('participant', 'room')
  for (const invalid of [
    '{broken json',
    JSON.stringify({ title: 'Board', clientCreateId: 'not-an-id' }),
    JSON.stringify({ title: '   ', clientCreateId: intent.clientCreateId }),
    JSON.stringify({ title: 'x'.repeat(513), clientCreateId: intent.clientCreateId }),
  ]) {
    storage.setItem(key, invalid)
    assert.equal(loadCanvasCreateIntent(storage, 'participant', 'room'), null)
    assert.equal(storage.getItem(key), null)
  }
})

test('Canvas create intent storage failures degrade without throwing', () => {
  const unavailable = {
    getItem() { throw new Error('denied') },
    setItem() { throw new Error('quota') },
    removeItem() { throw new Error('denied') },
  }
  assert.equal(saveCanvasCreateIntent(unavailable, 'participant', 'room', intent), false)
  assert.equal(loadCanvasCreateIntent(unavailable, 'participant', 'room'), null)
  assert.equal(clearCanvasCreateIntent(unavailable, 'participant', 'room', intent.clientCreateId), false)
  assert.equal(saveCanvasCreateIntent(null, 'participant', 'room', intent), false)

  const key = canvasCreateIntentStorageKey('participant', 'room')
  const existingIntent = JSON.stringify(intent)
  const quotaLimited = {
    getItem: (requestedKey) => requestedKey === key ? existingIntent : null,
    setItem() { throw new Error('quota exceeded') },
    removeItem() {},
  }
  assert.equal(saveCanvasCreateIntent(quotaLimited, 'participant', 'room', intent), true)
  assert.equal(saveCanvasCreateIntent(quotaLimited, 'participant', 'room', {
    ...intent,
    title: 'Different operation',
  }), false)
})
