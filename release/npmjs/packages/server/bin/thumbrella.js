#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');
const path = require('node:path');
const fs = require('node:fs');

function detectTarget() {
  const platform = process.platform;
  const arch = process.arch;

  if (platform === 'linux' && arch === 'x64') {
    return {
      pkg: '@thumbrella/server-linux-x64-gnu',
      exeRelPath: 'bin/thumbrella',
      target: 'linux-x64-gnu',
    };
  }

  if (platform === 'win32' && arch === 'x64') {
    return {
      pkg: '@thumbrella/server-win32-x64-msvc',
      exeRelPath: 'bin/thumbrella.exe',
      target: 'win32-x64-msvc',
    };
  }

  return null;
}

function resolveExecutable() {
  const info = detectTarget();
  if (!info) {
    return {
      error:
        `@thumbrella/server has no published binary for ${process.platform}-${process.arch}. ` +
        'Build from source: https://github.com/thumbrella-dev/thumbrella',
    };
  }

  let pkgJsonPath;
  try {
    pkgJsonPath = require.resolve(`${info.pkg}/package.json`);
  } catch (err) {
    return {
      error:
        `Missing optional dependency ${info.pkg} for target ${info.target}. ` +
        'Try reinstalling @thumbrella/server.',
    };
  }

  const pkgDir = path.dirname(pkgJsonPath);
  const exePath = path.join(pkgDir, info.exeRelPath);

  if (!fs.existsSync(exePath)) {
    return {
      error:
        `Installed package ${info.pkg} does not contain expected executable: ${exePath}`,
    };
  }

  return { exePath };
}

function main() {
  const resolved = resolveExecutable();
  if (resolved.error) {
    console.error(resolved.error);
    process.exit(1);
  }

  const result = spawnSync(resolved.exePath, process.argv.slice(2), {
    stdio: 'inherit',
  });

  if (result.error) {
    console.error(`Failed to execute Thumbrella binary: ${result.error.message}`);
    process.exit(1);
  }

  if (typeof result.status === 'number') {
    process.exit(result.status);
  }

  process.exit(1);
}

main();
