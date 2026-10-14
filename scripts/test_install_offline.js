#!/usr/bin/env node
'use strict';

const fs = require('fs');
const path = require('path');
const os = require('os');
const http = require('http');
const crypto = require('crypto');
const zlib = require('zlib');
const { spawn } = require('child_process');
const { getTargetInfo, parseTar } = require('../npm/lib/install.js');

const ROOT_DIR = path.resolve(__dirname, '..');
let testsPassed = 0;
let testsFailed = 0;

function assert(condition, message) {
  if (!condition) {
    testsFailed++;
    console.error(`  FAIL: ${message}`);
    throw new Error(message);
  } else {
    testsPassed++;
    console.log(`  PASS: ${message}`);
  }
}

// Helper to spawn a process asynchronously with timeout
function runSubprocessAsync(cmd, args, options = {}, timeoutMs = 15000) {
  return new Promise((resolve) => {
    let stdout = '';
    let stderr = '';
    let settled = false;

    const child = spawn(cmd, args, {
      ...options,
      stdio: ['ignore', 'pipe', 'pipe']
    });

    const timer = setTimeout(() => {
      if (!settled) {
        settled = true;
        child.kill('SIGKILL');
        resolve({
          status: null,
          signal: 'SIGKILL',
          stdout,
          stderr: stderr + `\n[test_install_offline] Process timed out after ${timeoutMs}ms`,
          timedOut: true
        });
      }
    }, timeoutMs);

    child.stdout.on('data', (d) => { stdout += d.toString('utf8'); });
    child.stderr.on('data', (d) => { stderr += d.toString('utf8'); });

    child.on('error', (err) => {
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        resolve({
          status: null,
          signal: null,
          stdout,
          stderr: stderr + `\n[test_install_offline] Spawn error: ${err.message}`,
          timedOut: false
        });
      }
    });

    child.on('close', (code, sig) => {
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        resolve({
          status: code,
          signal: sig,
          stdout,
          stderr,
          timedOut: false
        });
      }
    });
  });
}

// Helper to create a minimal valid tarball containing a mock binary at root
function createMockTarGz(binaryName, binaryContent) {
  const buf = Buffer.alloc(1024); // 512 header + 512 data/padding
  const contentBuf = Buffer.from(binaryContent, 'utf8');

  // Name (at root)
  buf.write(binaryName, 0, 100, 'utf8');
  // Mode 0755
  buf.write('0000755\0', 100, 8, 'utf8');
  // UID/GID
  buf.write('0000000\0', 108, 8, 'utf8');
  buf.write('0000000\0', 116, 8, 'utf8');
  // Size in octal
  const sizeOctal = contentBuf.length.toString(8).padStart(11, '0') + '\0';
  buf.write(sizeOctal, 124, 12, 'utf8');
  // Mtime
  buf.write('14000000000\0', 136, 12, 'utf8');
  // Typeflag '0'
  buf.write('0', 156, 1, 'utf8');

  // Compute checksum
  buf.write('        ', 148, 8, 'utf8');
  let chksum = 0;
  for (let i = 0; i < 512; i++) {
    chksum += buf[i];
  }
  const chksumOctal = chksum.toString(8).padStart(6, '0') + '\0 ';
  buf.write(chksumOctal, 148, 8, 'utf8');

  // Data
  contentBuf.copy(buf, 512);

  return zlib.gzipSync(buf);
}

async function runFixtureTests() {
  console.log('=== Running Mio Install Offline Fixture Tests (Async / Safe) ===\n');

  // Use os.tmpdir() for private test working directory
  const testBaseDir = fs.mkdtempSync(path.join(os.tmpdir(), 'mio-test-offline-'));

  // mock script outputs each argument on a separate line prefixed with ARG:
  const mockBinaryContent = [
    '#!/bin/sh',
    'echo "=== MOCK START ==="',
    'for a in "$@"; do',
    '  echo "ARG:[$a]"',
    'done',
    'exit 42',
    ''
  ].join('\n');
  const mockTarGz = createMockTarGz('mio', mockBinaryContent);
  const correctSha = crypto.createHash('sha256').update(mockTarGz).digest('hex');

  // Determine current platform target info
  const targetInfo = getTargetInfo();
  const archiveName = targetInfo.archiveName;

  let mockChecksums = `${correctSha}  ${archiveName}\n`;
  let shouldCorruptSha = false;
  let simulateDownloadFail = false;

  // Local fixture HTTP server
  const server = http.createServer((req, res) => {
    if (simulateDownloadFail) {
      res.writeHead(404);
      res.end('Not Found');
      return;
    }

    if (req.url.endsWith('/SHA256SUMS')) {
      res.writeHead(200, { 'Content-Type': 'text/plain' });
      if (shouldCorruptSha) {
        res.end('0000000000000000000000000000000000000000000000000000000000000000  ' + archiveName + '\n');
      } else {
        res.end(mockChecksums);
      }
    } else if (req.url.endsWith(`/${archiveName}`)) {
      res.writeHead(200, { 'Content-Type': 'application/gzip' });
      res.end(mockTarGz);
    } else {
      res.writeHead(404);
      res.end('Not found');
    }
  });

  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  const port = server.address().port;
  const releasesBaseUrl = `http://127.0.0.1:${port}/releases`;

  try {
    const installScript = path.join(ROOT_DIR, 'docs', 'install.sh');
    const binWrapper = path.join(ROOT_DIR, 'npm', 'bin', 'mio.js');

    // -------------------------------------------------------------
    // Test 1: install.sh success path with offline fixture server
    // -------------------------------------------------------------
    console.log('[Test 1] install.sh offline success run:');
    const mockInstallDir = path.join(testBaseDir, 'bin');

    const envSh = {
      ...process.env,
      MIO_VERSION: 'v0.1.0',
      MIO_INSTALL_DIR: mockInstallDir,
      MIO_ALLOW_INSECURE_TEST_HTTP: '1',
      MIO_RELEASE_BASE_URL: releasesBaseUrl
    };

    const shRes = await runSubprocessAsync('bash', [installScript], { env: envSh });
    assert(shRes.status === 0, `install.sh exited with 0 (stderr: ${shRes.stderr})`);
    assert(fs.existsSync(path.join(mockInstallDir, 'mio')), 'install.sh installed mio binary to mockInstallDir');

    // -------------------------------------------------------------
    // Test 2: install.sh sha256 mismatch failure test
    // -------------------------------------------------------------
    console.log('\n[Test 2] install.sh SHA256 checksum mismatch failure handling:');
    shouldCorruptSha = true;
    const shFailRes = await runSubprocessAsync('bash', [installScript], { env: envSh });
    assert(shFailRes.status !== 0, 'install.sh exits with non-zero on checksum mismatch');
    assert(shFailRes.stderr.includes('SHA256 verification failed'), 'install.sh outputs checksum failure message');
    shouldCorruptSha = false;

    // -------------------------------------------------------------
    // Test 3: install.sh 404 download failure handling
    // -------------------------------------------------------------
    console.log('\n[Test 3] install.sh 404 download failure handling:');
    simulateDownloadFail = true;
    const sh404Res = await runSubprocessAsync('bash', [installScript], { env: envSh });
    assert(sh404Res.status !== 0, 'install.sh exits non-zero on 404 error');
    simulateDownloadFail = false;

    // -------------------------------------------------------------
    // Test 4: npm wrapper arguments with spaces, exit code 42 passthrough
    // -------------------------------------------------------------
    console.log('\n[Test 4] npm wrapper arguments with spaces & exit code passthrough:');
    const npmCacheDir = path.join(testBaseDir, 'npm-cache');
    const envNpm = {
      ...process.env,
      MIO_VERSION: 'v0.1.0',
      MIO_CACHE_DIR: npmCacheDir,
      MIO_ALLOW_INSECURE_TEST_HTTP: '1',
      MIO_RELEASE_BASE_URL: releasesBaseUrl
    };

    const npmRunRes = await runSubprocessAsync(
      process.execPath,
      [binWrapper, 'arg with spaces', '--flag="val with space"'],
      { env: envNpm }
    );

    assert(npmRunRes.status === 42, `npm wrapper transparently returned exit code 42 (got ${npmRunRes.status})`);
    assert(
      npmRunRes.stdout.includes('ARG:[arg with spaces]') &&
      npmRunRes.stdout.includes('ARG:[--flag="val with space"]'),
      'npm wrapper correctly passed arguments with spaces untouched (verified argument boundaries)'
    );
    assert(
      fs.existsSync(path.join(npmCacheDir, 'v0.1.0', targetInfo.targetTriple, 'mio')),
      'npm wrapper cached binary into version and targetTriple subpath'
    );

    // -------------------------------------------------------------
    // Test 5: npm wrapper uses cached binary on second run without network
    // -------------------------------------------------------------
    console.log('\n[Test 5] npm wrapper cached secondary execution:');
    simulateDownloadFail = true; // Server returns 404
    const secondRunRes = await runSubprocessAsync(
      process.execPath,
      [binWrapper, '--cached-mode'],
      { env: envNpm }
    );
    assert(secondRunRes.status === 42, 'Second run succeeds using cached binary without network');
    assert(secondRunRes.stdout.includes('ARG:[--cached-mode]'), 'Second run stdout contains untouched argument');
    simulateDownloadFail = false;

    // -------------------------------------------------------------
    // Test 6: npm wrapper 404 failure handling with clean error output
    // -------------------------------------------------------------
    console.log('\n[Test 6] npm wrapper 404 download failure handling:');
    const emptyCacheDir = path.join(testBaseDir, 'npm-empty-cache');
    simulateDownloadFail = true;
    const failRunRes = await runSubprocessAsync(
      process.execPath,
      [binWrapper],
      {
        env: {
          ...process.env,
          MIO_VERSION: 'v0.1.0',
          MIO_CACHE_DIR: emptyCacheDir,
          MIO_ALLOW_INSECURE_TEST_HTTP: '1',
          MIO_RELEASE_BASE_URL: releasesBaseUrl
        }
      }
    );
    assert(failRunRes.status !== 0, 'npm wrapper exits non-zero when download fails');
    assert(failRunRes.stderr.includes('Failed to ensure mio binary'), 'npm wrapper reports clear error message on 404');
    simulateDownloadFail = false;

    // -------------------------------------------------------------
    // Test 7: npm wrapper concurrent downloads race safety
    // -------------------------------------------------------------
    console.log('\n[Test 7] npm wrapper concurrent invocations race safety:');
    const concurrentCacheDir = path.join(testBaseDir, 'npm-concurrent-cache');
    const envConcurrent = {
      ...process.env,
      MIO_VERSION: 'v0.1.0',
      MIO_CACHE_DIR: concurrentCacheDir,
      MIO_ALLOW_INSECURE_TEST_HTTP: '1',
      MIO_RELEASE_BASE_URL: releasesBaseUrl
    };

    const p1 = runSubprocessAsync(process.execPath, [binWrapper, 'concurrent-1'], { env: envConcurrent });
    const p2 = runSubprocessAsync(process.execPath, [binWrapper, 'concurrent-2'], { env: envConcurrent });
    const p3 = runSubprocessAsync(process.execPath, [binWrapper, 'concurrent-3'], { env: envConcurrent });

    const [res1, res2, res3] = await Promise.all([p1, p2, p3]);
    assert(res1.status === 42 && res2.status === 42 && res3.status === 42, 'All concurrent invocations exited with 42');
    assert(
      fs.existsSync(path.join(concurrentCacheDir, 'v0.1.0', targetInfo.targetTriple, 'mio')),
      'Concurrent runs safely produced valid cached binary without corruption'
    );

    // -------------------------------------------------------------
    // Test 8: npm wrapper checksum mismatch rejection
    // -------------------------------------------------------------
    console.log('\n[Test 8] npm wrapper checksum mismatch rejection:');
    const checksumMismatchCacheDir = path.join(testBaseDir, 'npm-mismatch-cache');
    shouldCorruptSha = true;
    const mismatchRes = await runSubprocessAsync(
      process.execPath,
      [binWrapper],
      {
        env: {
          ...process.env,
          MIO_VERSION: 'v0.1.0',
          MIO_CACHE_DIR: checksumMismatchCacheDir,
          MIO_ALLOW_INSECURE_TEST_HTTP: '1',
          MIO_RELEASE_BASE_URL: releasesBaseUrl
        }
      }
    );
    assert(mismatchRes.status !== 0, 'npm wrapper exits non-zero on checksum mismatch');
    assert(mismatchRes.stderr.includes('SHA256 checksum mismatch'), 'npm wrapper reports checksum mismatch error');
    shouldCorruptSha = false;

    // -------------------------------------------------------------
    // Test 9: npm wrapper archive missing mio executable
    // -------------------------------------------------------------
    console.log('\n[Test 9] npm wrapper archive missing mio executable:');
    const missingExeCacheDir = path.join(testBaseDir, 'npm-missing-exe-cache');
    const corruptTarGz = createMockTarGz('not-mio', '#!/bin/sh\nexit 0\n');
    const corruptSha = crypto.createHash('sha256').update(corruptTarGz).digest('hex');
    mockChecksums = `${corruptSha}  ${archiveName}\n`;
    
    // Temporarily serve corrupt archive
    const origTarGz = mockTarGz;
    // We can swap the mockTarGz response by pointing to corrupt
    const customServer = http.createServer((req, res) => {
      if (req.url.endsWith('/SHA256SUMS')) {
        res.writeHead(200, { 'Content-Type': 'text/plain' });
        res.end(`${corruptSha}  ${archiveName}\n`);
      } else if (req.url.endsWith(`/${archiveName}`)) {
        res.writeHead(200, { 'Content-Type': 'application/gzip' });
        res.end(corruptTarGz);
      } else {
        res.writeHead(404);
        res.end();
      }
    });
    await new Promise((r) => customServer.listen(0, '127.0.0.1', r));
    const customPort = customServer.address().port;
    const missingExeRes = await runSubprocessAsync(
      process.execPath,
      [binWrapper],
      {
        env: {
          ...process.env,
          MIO_VERSION: 'v0.1.0',
          MIO_CACHE_DIR: missingExeCacheDir,
          MIO_ALLOW_INSECURE_TEST_HTTP: '1',
          MIO_RELEASE_BASE_URL: `http://127.0.0.1:${customPort}/releases`
        }
      }
    );
    customServer.close();
    assert(missingExeRes.status !== 0, 'npm wrapper exits non-zero when archive misses mio binary');
    assert(missingExeRes.stderr.includes("Archive did not contain 'mio'"), 'npm wrapper reports missing mio executable error');
    mockChecksums = `${correctSha}  ${archiveName}\n`;

    // -------------------------------------------------------------
    // Test 10: npm wrapper rejects insecure HTTP without test flag
    // -------------------------------------------------------------
    console.log('\n[Test 10] npm wrapper rejects insecure HTTP without test flag:');
    const httpRejectCacheDir = path.join(testBaseDir, 'npm-http-reject-cache');
    const httpRejectRes = await runSubprocessAsync(
      process.execPath,
      [binWrapper],
      {
        env: {
          ...process.env,
          MIO_VERSION: 'v0.1.0',
          MIO_CACHE_DIR: httpRejectCacheDir,
          MIO_ALLOW_INSECURE_TEST_HTTP: '0',
          MIO_RELEASE_BASE_URL: releasesBaseUrl
        }
      }
    );
    assert(httpRejectRes.status !== 0, 'npm wrapper exits non-zero on insecure HTTP without flag');
    assert(httpRejectRes.stderr.includes('Insecure HTTP is only permitted'), 'npm wrapper reports insecure HTTP refusal');

    // -------------------------------------------------------------
    // Test 11: MIO_BIN_PATH pointing to non-existent path
    // -------------------------------------------------------------
    console.log('\n[Test 11] MIO_BIN_PATH pointing to non-existent path:');
    const badBinPathRes = await runSubprocessAsync(
      process.execPath,
      [binWrapper],
      {
        env: {
          ...process.env,
          MIO_BIN_PATH: path.join(testBaseDir, 'non-existent-binary-file-12345')
        }
      }
    );
    assert(badBinPathRes.status !== 0, 'npm wrapper exits non-zero on invalid MIO_BIN_PATH');
    assert(badBinPathRes.stderr.includes('MIO_BIN_PATH points to non-existent file'), 'npm wrapper reports clear MIO_BIN_PATH error without downloading');

    // -------------------------------------------------------------
    // Test 12: Subprocess signal forwarding and child termination
    // -------------------------------------------------------------
    console.log('\n[Test 12] Subprocess signal forwarding & no lingering child:');
    const signalCacheDir = path.join(testBaseDir, 'npm-signal-cache');
    // Binary that sleeps and prints its PID
    const sleepMockBinary = [
      '#!/bin/sh',
      'echo "CHILD_PID:$$"',
      'sleep 30',
      'exit 0',
      ''
    ].join('\n');
    const sleepTarGz = createMockTarGz('mio', sleepMockBinary);
    const sleepSha = crypto.createHash('sha256').update(sleepTarGz).digest('hex');

    const sleepServer = http.createServer((req, res) => {
      if (req.url.endsWith('/SHA256SUMS')) {
        res.writeHead(200, { 'Content-Type': 'text/plain' });
        res.end(`${sleepSha}  ${archiveName}\n`);
      } else if (req.url.endsWith(`/${archiveName}`)) {
        res.writeHead(200, { 'Content-Type': 'application/gzip' });
        res.end(sleepTarGz);
      }
    });
    await new Promise((r) => sleepServer.listen(0, '127.0.0.1', r));
    const sleepPort = sleepServer.address().port;

    const childProc = spawn(process.execPath, [binWrapper], {
      env: {
        ...process.env,
        MIO_VERSION: 'v0.1.0',
        MIO_CACHE_DIR: signalCacheDir,
        MIO_ALLOW_INSECURE_TEST_HTTP: '1',
        MIO_RELEASE_BASE_URL: `http://127.0.0.1:${sleepPort}/releases`
      },
      stdio: ['ignore', 'pipe', 'pipe']
    });

    let detectedChildPid = null;
    childProc.stdout.on('data', (d) => {
      const str = d.toString('utf8');
      const m = str.match(/CHILD_PID:(\d+)/);
      if (m) detectedChildPid = parseInt(m[1], 10);
    });

    // Wait until child is running
    await new Promise((resolve) => {
      const check = setInterval(() => {
        if (detectedChildPid) {
          clearInterval(check);
          resolve();
        }
      }, 50);
    });

    // Send SIGTERM to the Node wrapper process
    childProc.kill('SIGTERM');

    await new Promise((resolve) => childProc.on('close', resolve));
    sleepServer.close();

    // Check if inner child process is still running
    let childStillAlive = false;
    try {
      // 0 signal checks existence
      process.kill(detectedChildPid, 0);
      childStillAlive = true;
    } catch (_) {
      childStillAlive = false;
    }
    assert(!childStillAlive, 'Inner child process was successfully terminated and did not linger');

    console.log(`\nAll fixture tests completed! Passed: ${testsPassed}, Failed: ${testsFailed}`);
  } finally {
    server.close();
    try {
      fs.rmSync(testBaseDir, { recursive: true, force: true });
    } catch (_) {}
  }

  if (testsFailed > 0) {
    process.exit(1);
  }
}

runFixtureTests().catch((err) => {
  console.error('Fatal fixture test error:', err);
  process.exit(1);
});
