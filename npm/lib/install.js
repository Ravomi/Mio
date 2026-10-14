'use strict';

const fs = require('fs');
const path = require('path');
const os = require('os');
const https = require('https');
const http = require('http');
const crypto = require('crypto');
const zlib = require('zlib');
const { spawnSync } = require('child_process');

const REPO = 'Ravomi/Mio';
const PKG_VERSION = require('../package.json').version;

function getTargetInfo() {
  const platform = process.platform;
  const arch = process.arch;

  let osTarget = '';
  let archTarget = '';
  let ext = 'tar.gz';
  let binaryName = 'mio';

  if (platform === 'linux') {
    osTarget = 'unknown-linux-gnu';
  } else if (platform === 'darwin') {
    osTarget = 'apple-darwin';
  } else if (platform === 'win32') {
    osTarget = 'pc-windows-msvc';
    ext = 'zip';
    binaryName = 'mio.exe';
  } else {
    throw new Error(`Unsupported operating system: ${platform}`);
  }

  if (arch === 'x64') {
    archTarget = 'x86_64';
  } else if (arch === 'arm64') {
    if (platform === 'win32') {
      throw new Error('Windows arm64 is currently not supported for prebuilt binaries');
    }
    archTarget = 'aarch64';
  } else {
    throw new Error(`Unsupported architecture: ${arch}`);
  }

  const targetTriple = `${archTarget}-${osTarget}`;
  const archiveName = `mio-${targetTriple}.${ext}`;
  return {
    platform,
    arch,
    targetTriple,
    archiveName,
    binaryName,
    isZip: ext === 'zip'
  };
}

function getCacheDir() {
  if (process.env.MIO_CACHE_DIR) {
    return process.env.MIO_CACHE_DIR;
  }
  const home = os.homedir();
  if (process.platform === 'win32') {
    const localAppData = process.env.LOCALAPPDATA || path.join(home, 'AppData', 'Local');
    return path.join(localAppData, 'mio', 'bin');
  }
  if (process.platform === 'darwin') {
    return path.join(home, 'Library', 'Caches', 'mio', 'bin');
  }
  return path.join(home, '.cache', 'mio', 'bin');
}

function validateUrl(urlStr) {
  const parsed = new URL(urlStr);
  const allowHttp = process.env.MIO_ALLOW_INSECURE_TEST_HTTP === '1';

  if (parsed.protocol === 'https:') {
    return parsed;
  }

  if (parsed.protocol === 'http:') {
    const isLocalhost =
      parsed.hostname === 'localhost' ||
      parsed.hostname === '127.0.0.1' ||
      parsed.hostname === '[::1]' ||
      parsed.hostname === '::1';

    if (allowHttp && isLocalhost) {
      return parsed;
    }
    throw new Error(`Insecure HTTP is only permitted for localhost during testing: ${urlStr}`);
  }

  throw new Error(`Unsupported protocol ${parsed.protocol} in URL: ${urlStr}`);
}

function downloadToBuffer(urlStr, redirectCount = 0) {
  return new Promise((resolve, reject) => {
    if (redirectCount > 5) {
      return reject(new Error(`Too many redirects (max 5) fetching ${urlStr}`));
    }

    let parsed;
    try {
      parsed = validateUrl(urlStr);
    } catch (err) {
      return reject(err);
    }

    const reqModule = parsed.protocol === 'http:' ? http : https;
    let settled = false;

    const req = reqModule.get(
      parsed,
      {
        timeout: 60000,
        headers: {
          'User-Agent': 'Mio-Installer-Node/' + PKG_VERSION
        }
      },
      (res) => {
        if (settled) return;

        if (res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume(); // consume response data to free up memory
          let redirectUrl = res.headers.location;
          try {
            redirectUrl = new URL(redirectUrl, urlStr).toString();
          } catch (e) {
            settled = true;
            return reject(new Error(`Invalid redirect Location: ${res.headers.location}`));
          }
          settled = true;
          return downloadToBuffer(redirectUrl, redirectCount + 1).then(resolve, reject);
        }

        if (res.statusCode !== 200) {
          res.resume();
          settled = true;
          return reject(new Error(`Failed to download ${urlStr}: HTTP status ${res.statusCode}`));
        }

        const chunks = [];
        res.on('data', (d) => chunks.push(d));
        res.on('error', (err) => {
          if (!settled) {
            settled = true;
            reject(err);
          }
        });
        res.on('aborted', () => {
          if (!settled) {
            settled = true;
            reject(new Error(`Download connection aborted: ${urlStr}`));
          }
        });
        res.on('end', () => {
          if (!settled) {
            settled = true;
            resolve(Buffer.concat(chunks));
          }
        });
      }
    );

    req.on('timeout', () => {
      req.destroy(new Error(`Request timed out after 60s: ${urlStr}`));
    });

    req.on('error', (err) => {
      if (!settled) {
        settled = true;
        reject(err);
      }
    });
  });
}

function parseTar(buffer) {
  // Pure JS tar parser with strict bounds checks
  let offset = 0;
  const files = {};

  while (offset + 512 <= buffer.length) {
    const header = buffer.subarray(offset, offset + 512);
    // Check if end of archive (all zeroes in 512 byte block)
    let allZero = true;
    for (let i = 0; i < 512; i++) {
      if (header[i] !== 0) {
        allZero = false;
        break;
      }
    }
    if (allZero) break;

    let name = header.subarray(0, 100).toString('utf8').replace(/\0.*$/, '').trim();
    const sizeStr = header.subarray(124, 136).toString('utf8').replace(/\0.*$/, '').trim();
    const typeflag = header.subarray(156, 157).toString('utf8');
    const prefix = header.subarray(345, 500).toString('utf8').replace(/\0.*$/, '').trim();

    if (prefix) {
      name = prefix + '/' + name;
    }

    const size = parseInt(sizeStr, 8);
    if (isNaN(size) || size < 0 || offset + 512 + size > buffer.length) {
      throw new Error(`Corrupted tar archive header: invalid entry size ${sizeStr}`);
    }

    offset += 512;

    // Normal regular file ('0' or '\0')
    if (typeflag === '0' || typeflag === '' || typeflag === '\0') {
      files[name] = buffer.subarray(offset, offset + size);
    }

    const padding = (512 - (size % 512)) % 512;
    offset += size + padding;
  }

  return files;
}

function extractZipWindows(archivePath, destDir) {
  // Use PowerShell with environment variables to prevent command injection
  const psEnv = {
    ...process.env,
    MIO_ZIP_SRC: archivePath,
    MIO_ZIP_DST: destDir
  };
  const psRes = spawnSync(
    'powershell.exe',
    [
      '-NoProfile',
      '-NonInteractive',
      '-Command',
      'Expand-Archive -Path $env:MIO_ZIP_SRC -DestinationPath $env:MIO_ZIP_DST -Force'
    ],
    { env: psEnv }
  );

  if (psRes.status !== 0) {
    const errText = psRes.stderr ? psRes.stderr.toString('utf8') : '';
    throw new Error(`Failed to extract zip archive on Windows: ${errText}`);
  }
}

async function ensureBinary() {
  const customBin = process.env.MIO_BIN_PATH;
  if (customBin) {
    if (fs.existsSync(customBin)) {
      return customBin;
    }
    throw new Error(`MIO_BIN_PATH points to non-existent file: ${customBin}`);
  }

  const targetInfo = getTargetInfo();
  const rawVersion = process.env.MIO_VERSION || `v${PKG_VERSION}`;
  
  // Validate version format to prevent path traversal or invalid names
  if (!/^v?[0-9]+(\.[0-9]+)*(-[a-zA-Z0-9.-]+)?$/.test(rawVersion) && rawVersion !== 'latest') {
    throw new Error(`Invalid MIO_VERSION format: ${rawVersion}`);
  }

  // Cache path contains both version and target triple to prevent cross-platform poisoning
  const versionKey = rawVersion.startsWith('v') || rawVersion === 'latest' ? rawVersion : `v${rawVersion}`;
  const versionTargetCacheDir = path.join(getCacheDir(), versionKey, targetInfo.targetTriple);
  const destBinPath = path.join(versionTargetCacheDir, targetInfo.binaryName);

  if (fs.existsSync(destBinPath)) {
    return destBinPath;
  }

  fs.mkdirSync(versionTargetCacheDir, { recursive: true, mode: 0o700 });
  try {
    fs.chmodSync(versionTargetCacheDir, 0o700);
  } catch (_) {}

  const rawBaseUrl = (process.env.MIO_RELEASE_BASE_URL || `https://github.com/${REPO}/releases`).replace(/\/+$/, '');
  const downloadRoot = versionKey === 'latest'
    ? `${rawBaseUrl}/latest/download`
    : `${rawBaseUrl}/download/${versionKey}`;

  const checksumsUrl = `${downloadRoot}/SHA256SUMS`;
  const archiveUrl = `${downloadRoot}/${targetInfo.archiveName}`;

  process.stderr.write(`[mio-npm] First-time run: fetching prebuilt binary (${targetInfo.targetTriple})...\n`);
  process.stderr.write(`[mio-npm] Fetching checksums from ${checksumsUrl}...\n`);

  let checksumsData;
  try {
    checksumsData = (await downloadToBuffer(checksumsUrl)).toString('utf8');
  } catch (err) {
    throw new Error(`Failed to fetch SHA256SUMS: ${err.message}. If release is not yet published on Ravomi/Mio, binary cannot be downloaded.`);
  }

  let expectedSha = null;
  const lines = checksumsData.split(/\r?\n/);
  for (const line of lines) {
    const trimmed = line.trim();
    const match = trimmed.match(/^([a-fA-F0-9]{64})\s+(?:\*?)(.+)$/);
    if (match) {
      const hash = match[1].toLowerCase();
      const filename = match[2].trim();
      if (filename === targetInfo.archiveName) {
        expectedSha = hash;
        break;
      }
    }
  }

  if (!expectedSha) {
    throw new Error(`Archive ${targetInfo.archiveName} not found in SHA256SUMS.`);
  }

  process.stderr.write(`[mio-npm] Downloading archive from ${archiveUrl}...\n`);
  const archiveBuffer = await downloadToBuffer(archiveUrl);

  const actualSha = crypto.createHash('sha256').update(archiveBuffer).digest('hex').toLowerCase();
  if (actualSha !== expectedSha) {
    throw new Error(`SHA256 checksum mismatch! Expected ${expectedSha}, got ${actualSha}`);
  }

  // Use secure private temporary directory inside versionTargetCacheDir with 0700 permissions
  const stageDir = fs.mkdtempSync(path.join(versionTargetCacheDir, '.stage-'));
  try {
    fs.chmodSync(stageDir, 0o700);
  } catch (_) {}

  try {
    const stageBin = path.join(stageDir, targetInfo.binaryName);

    if (targetInfo.isZip) {
      const tempZip = path.join(stageDir, targetInfo.archiveName);
      fs.writeFileSync(tempZip, archiveBuffer);
      const extractDir = path.join(stageDir, 'extract');
      fs.mkdirSync(extractDir, { recursive: true });
      extractZipWindows(tempZip, extractDir);

      const rootExe = path.join(extractDir, 'mio.exe');
      if (!fs.existsSync(rootExe)) {
        throw new Error("Archive does not contain 'mio.exe' at root");
      }
      fs.copyFileSync(rootExe, stageBin);
    } else {
      // .tar.gz
      const decompressed = zlib.gunzipSync(archiveBuffer);
      const tarFiles = parseTar(decompressed);

      // Strictly select root 'mio' or './mio'
      let binBuffer = tarFiles['mio'] || tarFiles['./mio'];
      if (!binBuffer) {
        throw new Error("Archive did not contain 'mio' binary executable at root");
      }

      fs.writeFileSync(stageBin, binBuffer, { mode: 0o755 });
    }

    try {
      fs.chmodSync(stageBin, 0o755);
    } catch (_) {}

    // Atomic rename into final cached destination
    try {
      fs.renameSync(stageBin, destBinPath);
    } catch (renameErr) {
      // On Windows or concurrent runs, another process may have finished renaming first
      if (fs.existsSync(destBinPath)) {
        // Reuse the valid existing target binary
        process.stderr.write(`[mio-npm] Binary already cached by concurrent process.\n`);
      } else {
        throw renameErr;
      }
    }
  } finally {
    try {
      fs.rmSync(stageDir, { recursive: true, force: true });
    } catch (_) {}
  }

  process.stderr.write(`[mio-npm] Successfully cached binary to ${destBinPath}\n`);
  return destBinPath;
}

module.exports = {
  getTargetInfo,
  getCacheDir,
  ensureBinary,
  parseTar,
  validateUrl
};
