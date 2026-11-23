import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'
import { dejaViewer } from './viewer/vite-plugin.ts'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react(), dejaViewer()],
})
