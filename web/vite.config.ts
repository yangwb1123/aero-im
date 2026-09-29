import { defineConfig, type Plugin } from 'vite'
import solid from 'vite-plugin-solid'
import { fileURLToPath } from 'node:url'
import { readFileSync } from 'node:fs'

const root = fileURLToPath(new URL('.', import.meta.url))
const outDir = fileURLToPath(new URL('./dist', import.meta.url))

/**
 * The server-side OIDC callback returns an HTML page that loads
 * `/oidc_callback.js` to move the session into localStorage. That file lives at
 * the package root, not under `public/`, so vite would never emit it and the
 * browser would 404 mid-login — the user would authenticate with Snaplink and
 * still land on an empty session. Emit it explicitly and fail the build if it
 * is missing, so a rename cannot silently break the hand-off.
 */
function emitOidcCallback(): Plugin {
  const name = 'oidc_callback.js'
  return {
    name: 'aero-im:emit-oidc-callback',
    buildStart() {
      this.addWatchFile(fileURLToPath(new URL('./oidc_callback.js', import.meta.url)))
    },
    generateBundle() {
      const source = readFileSync(fileURLToPath(new URL('./oidc_callback.js', import.meta.url)), 'utf8')
      this.emitFile({ type: 'asset', fileName: name, source })
    },
  }
}

export default defineConfig({
  root,
  plugins: [solid(), emitOidcCallback()],
  resolve: {
    // @iris-ui-kit/* are `link:` dependencies that resolve to the sibling
    // iris-ui workspace, which has its OWN pnpm store and therefore its own
    // physical copy of solid-js. Without dedupe Vite bundles two Solid
    // runtimes: this app's `onMount` registers its effect in one, while
    // iris-ui's compiled components (createComponent / createContext, imported
    // from solid-js/web) run under the other. The owner chain is then broken
    // across the package boundary, so a descendant <Show> never re-evaluates —
    // the symptom is a permanently stuck boot screen with no console error.
    dedupe: ['solid-js', 'solid-js/web', 'solid-js/store', 'solid-js/h', 'solid-js/universal'],
  },
  server: {
    host: '127.0.0.1',
    port: 5177,
    strictPort: true,
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:3030',
        changeOrigin: true,
      },
      '/ws': {
        target: 'ws://127.0.0.1:3030',
        ws: true,
      },
      '/hls': {
        target: 'http://127.0.0.1:3030',
        changeOrigin: true,
      },
    },
  },
  preview: {
    host: '127.0.0.1',
    port: 4190,
    strictPort: true,
  },
  build: {
    outDir,
    emptyOutDir: true,
    sourcemap: true,
    target: 'es2020',
  },
})
