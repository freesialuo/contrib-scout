export const checksumsName = 'SHA256SUMS';

export function archiveName(tag, target) {
  return `contrib-scout-${tag}-${target}${target.startsWith('win32-') ? '.zip' : '.tar.gz'}`;
}

export function expectedAssetNames(tag, targets, tarball) {
  return [...Object.keys(targets).map(target => archiveName(tag, target)), tarball, checksumsName].sort();
}

export function formatChecksums(assets) {
  return assets
    .filter(asset => asset.name !== checksumsName)
    .map(asset => {
      if (!/^[0-9a-f]{64}$/.test(asset.sha256) || /[\s/\\]/.test(asset.name)) {
        throw new Error(`Invalid checksum entry for ${asset.name}`);
      }
      return `${asset.sha256}  ${asset.name}\n`;
    })
    .sort((a, b) => a.slice(66).localeCompare(b.slice(66)))
    .join('');
}

export function sameAssets(existing, local) {
  if (existing.length !== local.length) return false;
  const digests = new Map(existing.map(asset => [asset.name, asset.sha256]));
  return local.every(asset => digests.get(asset.name) === asset.sha256);
}

// Decides how to make the GitHub Release for one tag hold exactly the local assets.
// Drafts are private, so they may be repaired; a published release must already match.
export function planRelease({ tag, releases, local }) {
  if (releases.length > 1) throw new Error(`Found ${releases.length} GitHub Releases for ${tag}; delete the duplicates`);
  const [release] = releases;
  if (!release) return { action: 'create' };

  const existing = new Map(release.assets.map(asset => [asset.name, asset.sha256]));
  if (existing.size !== release.assets.length) throw new Error(`GitHub Release ${tag} has duplicate asset names`);
  const upload = local.filter(asset => existing.get(asset.name) !== asset.sha256).map(asset => asset.name);
  const wanted = new Set(local.map(asset => asset.name));
  const remove = release.assets.filter(asset => !wanted.has(asset.name)).map(asset => asset.name);

  if (!release.draft) {
    if (upload.length || remove.length) {
      throw new Error(`Published GitHub Release ${tag} differs from the tested assets (${[...upload, ...remove].join(', ')})`);
    }
    return { action: 'none', draft: false };
  }
  return upload.length || remove.length ? { action: 'sync', upload, remove } : { action: 'none', draft: true };
}
