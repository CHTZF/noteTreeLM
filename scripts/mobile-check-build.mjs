#!/usr/bin/env node
/**
 * Check if mobile source files are newer than the mobile build output.
 * If so, rebuild. Called automatically by `npm run service`.
 */
import { execSync } from 'child_process'
import { statSync, readdirSync, existsSync } from 'fs'
import { join } from 'path'

const ROOT = new URL('..', import.meta.url).pathname

/** Recursively find the newest mtime (ms) in a directory or single file. */
function newestMtime(target) {
  if (!existsSync(target)) return 0
  const stat = statSync(target)
  if (stat.isFile()) return stat.mtimeMs
  let max = stat.mtimeMs
  for (const entry of readdirSync(target, { withFileTypes: true })) {
    const full = join(target, entry.name)
    max = Math.max(max, newestMtime(full))
  }
  return max
}

// Source paths that affect the mobile bundle
const SOURCES = [
  'src/mobile',
  'src/lib',
  'mobile.html',
  'vite.mobile.config.ts',
]

// Build output directory
const DIST = 'src-service/mobile-dist'

const srcMtime = Math.max(...SOURCES.map(s => newestMtime(join(ROOT, s))))
const distMtime = newestMtime(join(ROOT, DIST))

if (srcMtime > distMtime) {
  console.log('[service] mobile source changed — rebuilding mobile...')
  execSync('npm run mobile:build', { stdio: 'inherit', cwd: ROOT })
  console.log('[service] mobile build done.')
} else {
  console.log('[service] mobile is up to date, skipping build.')
}
