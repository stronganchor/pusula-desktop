import assert from 'node:assert/strict'
import fs from 'node:fs'
import test from 'node:test'

const packageManifest = JSON.parse(fs.readFileSync(new URL('../package.json', import.meta.url), 'utf8'))
const packageLock = JSON.parse(fs.readFileSync(new URL('../package-lock.json', import.meta.url), 'utf8'))

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
