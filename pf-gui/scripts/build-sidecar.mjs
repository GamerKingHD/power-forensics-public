// Build the power-forensics CLI engine and the privileged pf-elevated helper,
// then stage them as Tauri external binaries ("sidecars") so an installed GUI
// can locate the agent without a developer PATH or a repository checkout.
// Tauri strips the target triple from each bundled file name, leaving
// `power-forensics.exe` and `pf-elevated.exe` next to the GUI binary, which is
// exactly where `agent::locate_agent_binary` looks first.

import { execFileSync } from 'node:child_process'
import { copyFileSync, existsSync, mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const guiDir = join(here, '..')
const repoRoot = join(guiDir, '..')

function hostTriple() {
  const info = execFileSync('rustc', ['-vV'], { cwd: repoRoot, encoding: 'utf8' })
  const host = info.split(/\r?\n/).find((l) => l.startsWith('host:'))
  if (!host) throw new Error('rustc -vV did not report a host triple')
  return host.slice('host:'.length).trim()
}

const triple = process.env.PF_TARGET_TRIPLE || hostTriple()
const exe = process.platform === 'win32' ? '.exe' : ''
const outDir = join(guiDir, 'src-tauri', 'binaries')

const binaries = [
  { crate: 'power-forensics', artifact: 'power-forensics' },
  { crate: 'pf-elevated', artifact: 'pf-elevated' },
]

execFileSync(
  'cargo',
  ['build', '--release', '--locked', ...binaries.flatMap((b) => ['-p', b.crate])],
  { cwd: repoRoot, stdio: 'inherit' },
)

mkdirSync(outDir, { recursive: true })
for (const b of binaries) {
  const built = join(repoRoot, 'target', 'release', `${b.artifact}${exe}`)
  if (!existsSync(built)) {
    console.error(`sidecar build did not produce ${built}`)
    process.exit(1)
  }
  const staged = join(outDir, `${b.artifact}-${triple}${exe}`)
  copyFileSync(built, staged)
  console.log(`staged sidecar: ${staged}`)
}
