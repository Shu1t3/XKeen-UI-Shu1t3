import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import path from 'path'
import { defineConfig, loadEnv } from 'vite'
import { compression } from 'vite-plugin-compression2'

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '')
  const backendUrl = env.XKEEN_BACKEND_URL || 'http://127.0.0.1:11000'
  const websocketUrl = backendUrl.replace(/^http/, 'ws')
  return {
    plugins: [react(), tailwindcss(), compression({ exclude: /index.html$/, deleteOriginalAssets: true, algorithms: ['gzip'] })],
    resolve: {
      alias: { '@': path.resolve(import.meta.dirname, './src') },
    },
    server: {
      proxy: {
        '/api': {
          target: backendUrl,
          changeOrigin: true,
          ws: true,
        },
        '/ws': { target: websocketUrl, ws: true },
        '/clash': {
          target: backendUrl,
          changeOrigin: true,
        },
        '/clash-ws': {
          target: websocketUrl,
          ws: true,
          changeOrigin: true,
        },
      },
    },
    build: {
      chunkSizeWarningLimit: 1000,
      rollupOptions: {
        output: {
          manualChunks(id) {
            if (id.includes('@codemirror') || id.includes('/codemirror/')) return 'codemirror'
            if (id.includes('prettier')) return 'prettier'
            if (id.includes('@radix-ui')) return 'radix'
            if (id.includes('@tabler/icons-react')) return 'icons'
            if (id.includes('framer-motion')) return 'motion'
          },
        },
      },
    },
  }
})
