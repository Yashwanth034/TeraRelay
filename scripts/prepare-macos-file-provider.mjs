#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

if (process.platform !== 'darwin') {
  console.log(`macOS File Provider build skipped: host platform is ${process.platform}`);
  process.exit(0);
}

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const buildScript = path.join(scriptDir, 'build-macos-file-provider.sh');
const result = spawnSync('/bin/bash', [buildScript], { stdio: 'inherit', env: process.env });
if (result.error) {
  console.error(`Could not launch macOS File Provider build: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status ?? 1);
