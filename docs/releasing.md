# Releasing

Tag pushes matching `vX.Y.Z` or `vX.Y.Z-<prerelease>` (no build metadata) trigger the **Release** workflow, [`.github/workflows/release.yml`](../.github/workflows/release.yml). It ships the GitHub Release and the npm package together, from the same tested bytes.

## Pipeline

1. **version** checks that the tag, `npm/package.json`, and every Cargo workspace package agree, and runs `node --test scripts/*.test.mjs`.
2. **native** builds and tests release binaries for all five targets.
3. **pack** stages and packs `@freesialuo/contrib-scout`, verifies its contents and SHA-512 integrity, smoke-installs it offline, then builds the GitHub Release assets from the same binaries: `contrib-scout-vX.Y.Z-<target>.tar.gz` (`.zip` for `win32-x64`, each holding the binary, README, and licenses), the exact npm tarball, and `SHA256SUMS`.
4. **draft** refuses to continue if npm already holds this version with a different tarball, then creates the GitHub Release as a **draft** with those assets and generated notes (marked prerelease when the tag has a `-` suffix).
5. **publish** publishes the tarball through npm trusted publishing (OIDC, with provenance). Stable versions go to dist-tag `latest`, prereleases to `next`. The `npm` environment requires a maintainer's approval under **Review deployments**.
6. **release** re-checks the draft's assets and makes it public only after npm holds the same tarball.

Every step can be re-run. An existing npm version passes only if its tarball is byte-identical. An existing draft is repaired to match the run's assets; it is private, and it is only made public after npm matches. A published GitHub Release is never changed: re-running against it passes only if its assets are identical, and fails otherwise. After a partial failure, use **Re-run failed jobs** so the run reuses its own artifacts; **Re-run all jobs** rebuilds the binaries, which are not byte-reproducible, and stops at **draft** once npm already holds the version.

If the repository variable `NPM_TRUSTED_PUBLISHING_ENABLED` is not `true`, the run stops after **draft**. Nothing becomes public, so GitHub and npm cannot diverge. The run summary notes this, and the verified tarball stays available in the `npm-package` artifact.

## Cutting a release

1. Set `[workspace.package].version` in `Cargo.toml` and `"version"` in `npm/package.json` to the same SemVer string. Refresh `Cargo.lock` (`cargo check --workspace`), run the [development checks](../README.md#development), and run `node scripts/npm-release.mjs validate vX.Y.Z`.
2. Create a signed commit and a matching signed tag, then push both: `git tag -s vX.Y.Z -m "vX.Y.Z"` and `git push origin main vX.Y.Z`.
3. Approve the `npm` deployment when the run asks for it.

## Repository and npm setup

The release needs, once per repository:

- An npm trusted publisher for `@freesialuo/contrib-scout`: GitHub Actions, this repository, workflow `release.yml`, environment `npm`, with direct `npm publish` allowed. Configurations created after Sep 3, 2026 allow only `npm stage publish` by default, and the **publish** job runs `npm publish`.
- A GitHub environment named `npm` with required reviewers. If you are the only maintainer, leave self-review allowed so you can approve your own release.
- The repository variable `NPM_TRUSTED_PUBLISHING_ENABLED` set to `true`.
- The default workflow permissions left read-only. Only the **draft** and **release** jobs request `contents: write`, and only **publish** requests `id-token: write`.

`@freesialuo/contrib-scout` has been set up this way since v0.1.0. The steps below are only needed again for a new package name, or if the trusted publisher is revoked.

## First release of a new package

npm only accepts a trusted publisher for a package that already exists, so the first version is published by hand, from the tarball the workflow built and tested. You need npm CLI 11.15.0 or later (`npm install -g npm@^11.15.0`) and 2FA on your npm account.

1. Leave `NPM_TRUSTED_PUBLISHING_ENABLED` unset and push the first tag (see [Cutting a release](#cutting-a-release)). The run builds everything and stops at a private draft release.
2. Download the tested tarball from that run and check it:

   ```sh
   gh run download <run-id> -n npm-package
   node scripts/npm-release.mjs verify vX.Y.Z freesialuo-contrib-scout-X.Y.Z.tgz
   ```

   Its SHA-256 must match the tarball's line in the draft release's `SHA256SUMS`.
3. Log in and publish that exact file. npm asks for 2FA in the browser:

   ```sh
   npm login
   npm publish freesialuo-contrib-scout-X.Y.Z.tgz --access public
   ```

   For a prerelease version (`X.Y.Z-rc.1`), npm requires an explicit dist-tag: add `--tag next`, which is what the workflow uses for prereleases.

   A new package can take a few minutes to appear. npm may first show a `0.0.0-stage` placeholder version; it is harmless once `latest` points to the real version. A version published by hand carries no provenance.
4. Add the trusted publisher (from the CLI, or under the package's **Settings → Trusted publishing** on npmjs.com):

   ```sh
   npm trust github @freesialuo/contrib-scout --repo freesialuo/contrib-scout --file release.yml --env npm --allow-publish
   ```

5. Create the `npm` environment and set the variable:

   ```sh
   echo "{\"reviewers\":[{\"type\":\"User\",\"id\":$(gh api user --jq .id)}],\"prevent_self_review\":false}" \
     | gh api -X PUT repos/freesialuo/contrib-scout/environments/npm --input -
   gh variable set NPM_TRUSTED_PUBLISHING_ENABLED --body true
   ```

   Or create the environment under the repository's **Settings → Environments**.
6. Make the draft public. Do not re-run the workflow for this version:

   ```sh
   gh release edit vX.Y.Z --tag vX.Y.Z --verify-tag --draft=false
   ```

From the next version on, pushing a tag releases to both GitHub and npm.

The trusted publisher from step 4 expires if no trusted publish uses it within 2 days of its creation; the manual publish in step 3 does not count. Push the next release tag within those 2 days, or, if it has expired, recreate it before the next tag:

```sh
npm trust list @freesialuo/contrib-scout
npm trust revoke @freesialuo/contrib-scout --id <id>
npm trust github @freesialuo/contrib-scout --repo freesialuo/contrib-scout --file release.yml --env npm --allow-publish
```

and push the next tag within 2 days of recreating it.
