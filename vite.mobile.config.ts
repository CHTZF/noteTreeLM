import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import path from 'path'

// Mobile PWA build — separate from the Tauri desktop build.
// Output goes to src-service/mobile-dist/ so Axum can serve it as static files.
export default defineConfig({
  plugins: [react()],
  root: '.',
  // Use mobile.html as the entry
  build: {
    outDir: 'src-service/mobile-dist',
    emptyOutDir: true,
    rollupOptions: {
      input: { index: path.resolve(__dirname, 'mobile.html') },
      output: { entryFileNames: 'assets/[name]-[hash].js' },
    },
  },
  server: {
    port: 1421,
    strictPort: true,
    proxy: {
      // Proxy API calls to the service in dev mode
      '/api': 'http://127.0.0.1:7787',
      '/health': 'http://127.0.0.1:7787',
    },
  },
  resolve: {
    alias: { '@': path.resolve(__dirname, 'src') },
  },
})
