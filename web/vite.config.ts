import { defineConfig } from 'vite'
import solid from 'vite-plugin-solid'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('.', import.meta.url))
const outDir = fileURLToPath(new URL('./dist', import.meta.url))

export default defineConfig({
  root,
  plugins: [solid()],
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
