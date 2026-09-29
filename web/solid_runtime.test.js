import test from 'node:test'
import assert from 'node:assert/strict'
import { readdir, readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'

const dist = new URL('./dist/assets/', import.meta.url)

// Regression guard for the stuck-boot-screen bug.
//
// @iris-ui-kit/* are `link:` dependencies resolving to the sibling iris-ui
// workspace, which carries its own pnpm store and therefore its own physical
// copy of solid-js. Vite used to bundle both copies into one chunk, so the
// app's `onMount` registered its effect in one Solid runtime while iris-ui's
// compiled components (createComponent / createContext, imported from
// solid-js/web) ran under the other. The owner chain broke across the package
// boundary and a descendant <Show> never re-evaluated: the UI sat on
// "正在恢复会话…" forever with no console error.
//
// Asserting on the built bundle is deliberate. Every other test in this package
// imports from `src`, which is a different module graph than what consumers
// load, so it cannot see a bundling-level defect at all.

/** Count standalone definitions of a minified Solid internal by a unique marker. */
async function countRuntimeCopies(marker) {
  const files = (await readdir(dist)).filter((f) => f.endsWith('.js'))
  let hits = 0
  for (const file of files) {
    const source = await readFile(new URL(file, dist), 'utf8')
    hits += source.split(marker).length - 1
  }
  return hits
}

test('the built bundle contains exactly one Solid runtime', async () => {
  // `observers:null` appears once per compiled createSignal. Two copies means
  // two independent Owner/Updates graphs.
  const createSignals = await countRuntimeCopies('observers:null')
  assert.equal(
    createSignals,
    1,
    `expected 1 Solid runtime in dist/, found ${createSignals} createSignal definitions — ` +
      'a duplicated solid-js copy breaks reactive ownership across the ' +
      '@iris-ui-kit/solid boundary. Check resolve.dedupe in vite.config.ts.',
  )
})

test('vite dedupes solid-js so linked workspaces share one runtime', async () => {
  const config = await readFile(fileURLToPath(new URL('./vite.config.ts', import.meta.url)), 'utf8')
  assert.match(config, /dedupe:\s*\[[^\]]*'solid-js'/, 'vite.config.ts must pin resolve.dedupe for solid-js')
})
