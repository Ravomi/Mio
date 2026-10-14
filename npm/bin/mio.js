#!/usr/bin/env node
'use strict';

const os = require('os');
const { spawn } = require('child_process');
const { ensureBinary } = require('../lib/install.js');

async function main() {
  let binPath;
  try {
    binPath = await ensureBinary();
  } catch (err) {
    console.error(`[mio-npm] Failed to ensure mio binary: ${err.message}`);
    process.exit(1);
  }

  const child = spawn(binPath, process.argv.slice(2), {
    stdio: 'inherit',
    windowsHide: true
  });

  // Forward termination signals to child process so it doesn't linger
  const forwardSignal = (sig) => {
    if (child && !child.killed) {
      child.kill(sig);
    }
  };

  process.on('SIGINT', () => forwardSignal('SIGINT'));
  process.on('SIGTERM', () => forwardSignal('SIGTERM'));

  child.on('error', (err) => {
    console.error(`[mio-npm] Failed to execute ${binPath}: ${err.message}`);
    process.exit(1);
  });

  child.on('close', (code, signal) => {
    if (signal) {
      process.exit(128 + (os.constants.signals[signal] || 15));
    } else {
      process.exit(code !== null ? code : 0);
    }
  });
}

main();
