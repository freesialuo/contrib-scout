#!/usr/bin/env node

const { spawnSync } = require('node:child_process');
const { constants } = require('node:os');
const { join } = require('node:path');
const targets = require('../targets.json');

const target = `${process.platform}-${process.arch}`;
const executable = targets[target];

// The Linux binaries link against glibc; on musl (e.g. Alpine) they fail with a bare ENOENT.
// Node reports no glibc runtime there. Any failure to read the report means "assume glibc".
function isMuslLinux() {
  if (process.platform !== 'linux') return false;
  try {
    return !process.report.getReport().header.glibcVersionRuntime;
  } catch {
    return false;
  }
}

if (!executable) {
  console.error(`contrib-scout has no bundled binary for ${target}. Supported targets: ${Object.keys(targets).join(', ')}.`);
  process.exitCode = 1;
} else if (isMuslLinux()) {
  console.error('contrib-scout does not support musl libc (for example Alpine Linux); a glibc-based Linux is required.');
  process.exitCode = 1;
} else {
  const binary = join(__dirname, '..', 'native', target, executable);
  const result = spawnSync(binary, process.argv.slice(2), { stdio: 'inherit' });

  if (result.error) {
    console.error(`contrib-scout could not start its ${target} binary: ${result.error.message}`);
    process.exitCode = 1;
  } else if (result.signal) {
    process.exitCode = 128 + (constants.signals[result.signal] ?? 0);
  } else {
    process.exitCode = result.status ?? 1;
  }
}
