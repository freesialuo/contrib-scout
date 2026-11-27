import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  archiveName, expectedAssetNames, formatChecksums, planRelease, sameAssets,
} from './github-release-decisions.mjs';

const tag = 'v1.2.3';
const sha = character => character.repeat(64);
const local = [
  { name: 'SHA256SUMS', sha256: sha('a') },
  { name: 'contrib-scout-v1.2.3-linux-x64.tar.gz', sha256: sha('b') },
  { name: 'freesialuo-contrib-scout-1.2.3.tgz', sha256: sha('c') },
];

test('names archives by tag and target, zipping only Windows', () => {
  assert.equal(archiveName(tag, 'linux-x64'), 'contrib-scout-v1.2.3-linux-x64.tar.gz');
  assert.equal(archiveName('v1.2.3-rc.1', 'darwin-arm64'), 'contrib-scout-v1.2.3-rc.1-darwin-arm64.tar.gz');
  assert.equal(archiveName(tag, 'win32-x64'), 'contrib-scout-v1.2.3-win32-x64.zip');
  assert.deepEqual(
    expectedAssetNames(tag, { 'win32-x64': 'contrib-scout.exe', 'linux-x64': 'contrib-scout' }, 'freesialuo-contrib-scout-1.2.3.tgz'),
    ['SHA256SUMS', 'contrib-scout-v1.2.3-linux-x64.tar.gz', 'contrib-scout-v1.2.3-win32-x64.zip', 'freesialuo-contrib-scout-1.2.3.tgz'],
  );
});

test('formats sorted sha256sum lines and leaves the checksum file out', () => {
  assert.equal(
    formatChecksums([local[2], local[0], local[1]]),
    `${sha('b')}  contrib-scout-v1.2.3-linux-x64.tar.gz\n${sha('c')}  freesialuo-contrib-scout-1.2.3.tgz\n`,
  );
  assert.throws(() => formatChecksums([{ name: 'a b', sha256: sha('a') }]), /Invalid checksum entry/);
  assert.throws(() => formatChecksums([{ name: 'x', sha256: 'sha256:abc' }]), /Invalid checksum entry/);
});

test('compares asset sets by name and digest', () => {
  assert.ok(sameAssets([...local].reverse(), local));
  assert.ok(!sameAssets(local.slice(1), local));
  assert.ok(!sameAssets([...local.slice(1), { name: 'SHA256SUMS', sha256: sha('f') }], local));
});

test('creates a draft when the tag has no release', () => {
  assert.deepEqual(planRelease({ tag, releases: [], local }), { action: 'create' });
});

test('reuses a draft or a published release that already holds the tested assets', () => {
  assert.deepEqual(planRelease({ tag, releases: [{ draft: true, assets: local }], local }), { action: 'none', draft: true });
  assert.deepEqual(planRelease({ tag, releases: [{ draft: false, assets: local }], local }), { action: 'none', draft: false });
});

test('repairs a private draft: uploads missing or changed assets and removes strays', () => {
  const assets = [local[0], { ...local[1], sha256: sha('f') }, { name: 'stray.txt', sha256: sha('d') }];
  assert.deepEqual(planRelease({ tag, releases: [{ draft: true, assets }], local }), {
    action: 'sync',
    upload: ['contrib-scout-v1.2.3-linux-x64.tar.gz', 'freesialuo-contrib-scout-1.2.3.tgz'],
    remove: ['stray.txt'],
  });
});

test('never mutates a published release that differs', () => {
  for (const assets of [local.slice(1), [...local, { name: 'stray.txt', sha256: sha('d') }], [{ ...local[0], sha256: sha('f') }, ...local.slice(1)]]) {
    assert.throws(() => planRelease({ tag, releases: [{ draft: false, assets }], local }), /Published GitHub Release v1\.2\.3 differs/);
  }
});

test('rejects ambiguous releases and duplicate asset names', () => {
  assert.throws(() => planRelease({ tag, releases: [{ draft: true, assets: [] }, { draft: true, assets: [] }], local }), /Found 2 GitHub Releases/);
  assert.throws(() => planRelease({ tag, releases: [{ draft: true, assets: [...local, local[0]] }], local }), /duplicate asset names/);
});
