import { execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import {
  chmodSync, copyFileSync, createReadStream, mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join, resolve } from 'node:path';
import {
  archiveName, checksumsName, expectedAssetNames, formatChecksums, planRelease, sameAssets,
} from './github-release-decisions.mjs';
import { parseRelease } from './npm-release-decisions.mjs';

const root = new URL('../', import.meta.url);
const manifest = JSON.parse(readFileSync(new URL('npm/package.json', root), 'utf8'));
const targets = JSON.parse(readFileSync(new URL('npm/targets.json', root), 'utf8'));
const tarball = `${manifest.name.replace(/^@/, '').replace('/', '-')}-${manifest.version}.tgz`;

async function sha256(file) {
  const hash = createHash('sha256');
  for await (const chunk of createReadStream(file)) hash.update(chunk);
  return hash.digest('hex');
}

function gh(args, options = {}) {
  return execFileSync('gh', args, { encoding: 'utf8', maxBuffer: 256 * 1024 * 1024, ...options });
}

async function buildAssets(tag, binaries, npmTarball, out) {
  if (basename(npmTarball) !== tarball) throw new Error(`Unexpected npm tarball filename: ${npmTarball}`);
  mkdirSync(out);
  const work = mkdtempSync(join(tmpdir(), 'contrib-scout-release-'));

  for (const [target, executable] of Object.entries(targets)) {
    const archive = archiveName(tag, target);
    const directory = archive.replace(/\.(zip|tar\.gz)$/, '');
    const stage = join(work, directory);
    mkdirSync(stage);
    copyFileSync(join(binaries, `binary-${target}`, executable), join(stage, executable));
    chmodSync(join(stage, executable), 0o755);
    for (const file of ['README.md', 'LICENSE-MIT', 'LICENSE-APACHE']) copyFileSync(new URL(file, root), join(stage, file));

    if (archive.endsWith('.zip')) {
      execFileSync('zip', ['-qrX', join(out, archive), directory], { cwd: work, stdio: 'inherit' });
    } else {
      execFileSync('tar', ['-czf', join(out, archive), '-C', work, directory], { stdio: 'inherit' });
    }
  }
  copyFileSync(npmTarball, join(out, tarball));

  const assets = [];
  for (const name of readdirSync(out)) assets.push({ name, sha256: await sha256(join(out, name)) });
  writeFileSync(join(out, checksumsName), formatChecksums(assets));
  await localAssets(tag, out);
}

async function localAssets(tag, directory) {
  const names = readdirSync(directory).sort();
  const expected = expectedAssetNames(tag, targets, tarball);
  if (JSON.stringify(names) !== JSON.stringify(expected)) {
    throw new Error(`Unexpected release assets: ${names.join(', ')} (expected ${expected.join(', ')})`);
  }
  const assets = [];
  for (const name of names) assets.push({ name, sha256: await sha256(join(directory, name)) });
  if (readFileSync(join(directory, checksumsName), 'utf8') !== formatChecksums(assets)) {
    throw new Error(`${checksumsName} does not match the release assets`);
  }
  return assets;
}

// Lists releases (drafts included, which needs push access) for the tag and hashes their assets.
async function remoteReleases(tag) {
  const filter = `.[] | select(.tag_name == ${JSON.stringify(tag)}) | {id, draft, assets: [.assets[] | {id, name, digest}]}`;
  const releases = gh(['api', '--paginate', 'repos/{owner}/{repo}/releases?per_page=100', '--jq', filter])
    .split('\n').filter(Boolean).map(line => JSON.parse(line));
  for (const release of releases) {
    for (const asset of release.assets) {
      if (asset.digest?.startsWith('sha256:')) {
        asset.sha256 = asset.digest.slice('sha256:'.length);
      } else {
        const bytes = gh(['api', '-H', 'Accept: application/octet-stream', `repos/{owner}/{repo}/releases/assets/${asset.id}`], { encoding: 'buffer' });
        asset.sha256 = createHash('sha256').update(bytes).digest('hex');
      }
    }
  }
  return releases;
}

async function draft(tag, prerelease, directory) {
  const local = await localAssets(tag, directory);
  const files = names => names.map(name => join(directory, name));
  const plan = planRelease({ tag, releases: await remoteReleases(tag), local });

  if (plan.action === 'create') {
    gh(['release', 'create', tag, '--draft', '--verify-tag', '--generate-notes', '--title', tag,
      `--prerelease=${prerelease}`, ...files(local.map(asset => asset.name))], { stdio: 'inherit' });
  } else if (plan.action === 'sync') {
    for (const name of plan.remove) gh(['release', 'delete-asset', tag, name, '--yes'], { stdio: 'inherit' });
    if (plan.upload.length) gh(['release', 'upload', tag, '--clobber', ...files(plan.upload)], { stdio: 'inherit' });
  }

  const [release] = await remoteReleases(tag);
  if (!release || !sameAssets(release.assets, local)) throw new Error(`GitHub Release ${tag} assets did not settle`);
  console.log(`GitHub Release ${tag} is ${release.draft ? 'a draft' : 'published'} with the tested assets (${plan.action}).`);
}

async function publish(tag, prerelease, directory) {
  const local = await localAssets(tag, directory);
  const releases = await remoteReleases(tag);
  const plan = planRelease({ tag, releases, local });
  if (plan.action !== 'none') throw new Error(`GitHub Release ${tag} must hold the tested assets before publishing`);

  if (plan.draft) {
    gh(['release', 'edit', tag, '--draft=false', `--prerelease=${prerelease}`], { stdio: 'inherit' });
  }
  const [release] = await remoteReleases(tag);
  if (!release || release.draft || !sameAssets(release.assets, local)) throw new Error(`GitHub Release ${tag} did not publish cleanly`);
  console.log(`GitHub Release ${tag} is published.`);
}

const [operation, tag, ...paths] = process.argv.slice(2);
const release = parseRelease(tag, manifest.version);
const prerelease = release.dist_tag !== 'latest';

if (operation === 'assets' && paths.length === 3) {
  await buildAssets(tag, ...paths.map(path => resolve(path)));
} else if (operation === 'draft' && paths.length === 1) {
  await draft(tag, prerelease, resolve(paths[0]));
} else if (operation === 'publish' && paths.length === 1) {
  await publish(tag, prerelease, resolve(paths[0]));
} else {
  throw new Error('Usage: node scripts/github-release.mjs assets TAG BINARIES_DIR NPM_TARBALL OUT_DIR | draft TAG ASSETS_DIR | publish TAG ASSETS_DIR');
}
