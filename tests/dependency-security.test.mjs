import assert from 'node:assert/strict'
import fs from 'node:fs'
import test from 'node:test'

const packageManifest = JSON.parse(fs.readFileSync(new URL('../package.json', import.meta.url), 'utf8'))
const packageLock = JSON.parse(fs.readFileSync(new URL('../package-lock.json', import.meta.url), 'utf8'))
const desktopLock = fs.readFileSync(new URL('../src-tauri/Cargo.lock', import.meta.url), 'utf8')

function assertVersionAtLeast(actual, minimum, dependency) {
  const actualParts = actual.split('.').map(Number)
  const minimumParts = minimum.split('.').map(Number)

  assert.equal(actualParts.length, 3, `${dependency} must use a three-part version`)
  assert.equal(actualParts.every(Number.isInteger), true, `${dependency} must use a numeric version`)

  for (let index = 0; index < minimumParts.length; index += 1) {
    if (actualParts[index] > minimumParts[index]) return
    if (actualParts[index] < minimumParts[index]) {
      assert.fail(`${dependency} ${actual} is below the secure minimum ${minimum}`)
    }
  }
}

test('transitive frontend build dependencies stay above advisory floors', () => {
  const dependencyFloors = {
    nanoid: '3.3.18',
    postcss: '8.5.23',
    'source-map-js': '1.2.2',
  }

  for (const [dependency, minimum] of Object.entries(dependencyFloors)) {
    const override = packageManifest.overrides?.[dependency]
    const locked = packageLock.packages?.[`node_modules/${dependency}`]?.version

    assert.equal(typeof override, 'string', `${dependency} must remain explicitly overridden`)
    assert.equal(typeof locked, 'string', `${dependency} must remain present in package-lock.json`)
    assertVersionAtLeast(override, minimum, `${dependency} override`)
    assertVersionAtLeast(locked, minimum, `${dependency} lock entry`)
  }
})

test('Vite stays above the Windows editor command-injection advisory floor', () => {
  const minimum = '6.4.4'
  const requested = packageManifest.devDependencies?.vite
  assert.match(requested, /^\^6\.\d+\.\d+$/, 'keep the compatible Vite 6 update range')
  assertVersionAtLeast(requested.slice(1), minimum, 'Vite manifest minimum')
  assertVersionAtLeast(packageLock.packages['node_modules/vite'].version, minimum, 'Vite lock entry')
})

test('desktop TLS and optional transport dependencies stay above advisory floors', () => {
  const packages = desktopLock.split('[[package]]').slice(1).map(block => ({
    name: block.match(/^name = "([^"]+)"$/m)?.[1],
    version: block.match(/^version = "([^"]+)"$/m)?.[1],
  }))
  const floors = {
    rustls: '0.23.46',
    'rustls-webpki': '0.103.16',
    'quinn-proto': '0.11.18',
    'event-listener': '5.4.2',
    'webpki-root-certs': '1.0.10',
  }
  for (const [name, minimum] of Object.entries(floors)) {
    const locked = packages.filter(dependency => dependency.name === name)
    assert.notEqual(locked.length, 0, `${name} must remain covered by the dependency review`)
    for (const dependency of locked) assertVersionAtLeast(dependency.version, minimum, name)
  }
})
