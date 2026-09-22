// Single version authority check: every workspace crate, the GUI package.json,
// and the Tauri config must agree. In release mode (`--release`) the generated
// release manifest and the exact Git tag must agree too, so a public release
// can never ship a mismatched version.
//
// The report/export manifests read the compiled crate version at runtime, so
// this script is the only place version strings are compared.

import { readFileSync, existsSync } from 'node:fs'
import { execFileSync } from 'node:child_process'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const guiDir = join(here, '..')
const repoRoot = join(guiDir, '..')
const releaseMode = process.argv.includes('--release')

function cargoPackageVersion(path) {
  const text = readFileSync(path, 'utf8')
  const m = text.match(/^\s*\[package\][\s\S]*?^\s*version\s*=\s*"([^"]+)"/m)
  if (!m) throw new Error(`no [package] version in ${path}`)
  return m[1]
}

function workspaceMembers() {
  const text = readFileSync(join(repoRoot, 'Cargo.toml'), 'utf8')
  const m = text.match(/^\s*members\s*=\s*\[([\s\S]*?)\]/m)
  if (!m) throw new Error('no workspace members in Cargo.toml')
  return [...m[1].matchAll(/"([^"]+)"/g)].map((x) => x[1])
}

function git(args) {
  try {
    return execFileSync('git', args, { cwd: repoRoot, encoding: 'utf8' }).trim()
  } catch {
    return null
  }
}

const versions = {}
for (const member of workspaceMembers()) {
  versions[`${member}/Cargo.toml`] = cargoPackageVersion(join(repoRoot, member, 'Cargo.toml'))
}
versions['pf-gui/package.json'] = JSON.parse(readFileSync(join(guiDir, 'package.json'), 'utf8')).version
versions['tauri.conf.json'] = JSON.parse(readFileSync(join(guiDir, 'src-tauri', 'tauri.conf.json'), 'utf8')).version

const unique = new Set(Object.values(versions))
for (const [file, v] of Object.entries(versions)) console.log(`${file}: ${v}`)
if (unique.size !== 1) {
  console.error(`version mismatch: ${[...unique].join(', ')}`)
  process.exit(1)
}
const version = [...unique][0]
console.log(`versions agree: ${version}`)

if (!releaseMode) process.exit(0)

// --- Release-mode checks: exact tag, and the manifest when it exists. ---
const manifestPath = join(repoRoot, 'dist-release', `v${version}`, 'release-manifest.json')
if (existsSync(manifestPath)) {
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'))
  if (manifest.version !== version) {
    console.error(`release manifest version ${manifest.version} != ${version}`)
    process.exit(1)
  }
  console.log(`release-manifest.json: ${manifest.version}`)
} else {
  console.log('release-manifest.json: not generated yet (skipped)')
}

const tag = git(['describe', '--tags', '--exact-match', 'HEAD'])
if (tag === null) {
  console.error('HEAD is not at an exact tag; tag the release commit before packaging')
  process.exit(1)
}
if (tag !== `v${version}`) {
  console.error(`git tag ${tag} != v${version}`)
  process.exit(1)
}
console.log(`git tag: ${tag}`)
