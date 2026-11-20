'use strict';

const http = require('node:http');
const https = require('node:https');
const { spawn } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const HOST = process.env.OMNI_UI_BIND_ADDR || process.env.HOST || '127.0.0.1';
const PORT = Number(process.env.PORT || 3000);
const MAX_REQUEST_BYTES = 80 * 1024;
const MAX_RESPONSE_BYTES = 1024 * 1024;
const LOCAL_GATEWAY_URL = 'https://127.0.0.1:8443';
const DEFAULT_PUBLIC_SITE_ORIGIN = 'https://omniengine.onrender.com';
const CONFIGURED_PUBLIC_SITE_ORIGIN = normalizePublicSiteOrigin(process.env.OMNI_PUBLIC_SITE_URL);
const GOOGLE_VERIFICATION_PATH = '/googlebf0032297d3c5fef.html';
const SITE_ORIGIN_TOKEN = '__OMNI_SITE_ORIGIN__';
const CANONICAL_URL_TOKEN = '__OMNI_CANONICAL_URL__';
const CONFIGURED_HOST_VALUES = [
  process.env.OMNI_ALLOWED_HOSTS,
  process.env.OMNI_UI_ALLOWED_HOSTS,
]
  .filter(Boolean)
  .flatMap((hosts) => hosts.split(',').map((host) => host.trim().toLowerCase()).filter(Boolean));
const CONFIGURED_HOST_ENTRIES = CONFIGURED_HOST_VALUES
  .filter((host) => host !== '*')
  .map(normalizeHostEntry)
  .filter(Boolean);
const RENDER_EXTERNAL_HOST = normalizeHostEntry(process.env.RENDER_EXTERNAL_HOSTNAME);
const RENDER_MODE = Boolean(
  process.env.RENDER
  || RENDER_EXTERNAL_HOST
  || (process.env.OMNI_TLS_MODE || '').toLowerCase() === 'plain',
);
const PUBLIC_GATEWAY_URL = process.env.OMNI_PUBLIC_GATEWAY_URL
  || (RENDER_EXTERNAL_HOST ? `https://${RENDER_EXTERNAL_HOST}` : '')
  || configuredPublicHostUrl(CONFIGURED_HOST_ENTRIES)
  || '';
const DEFAULT_GATEWAY_URL = PUBLIC_GATEWAY_URL
  || process.env.OMNI_GATEWAY_URL
  || LOCAL_GATEWAY_URL;
const ALLOWED_HOSTS = new Set([
  `localhost:${PORT}`,
  `127.0.0.1:${PORT}`,
  'localhost',
  'localhost:3000',
  'localhost:8443',
  '127.0.0.1',
  '127.0.0.1:3000',
  '127.0.0.1:8443',
  '::1',
  '[::1]',
  `[::1]:${PORT}`,
  '[::1]:3000',
  '[::1]:8443',
  ...CONFIGURED_HOST_ENTRIES,
  ...(RENDER_EXTERNAL_HOST ? [RENDER_EXTERNAL_HOST] : []),
]);
const ALLOW_ANY_HOST = CONFIGURED_HOST_VALUES.includes('*');
const ALLOWED_ORIGINS = new Set([
  ...(process.env.OMNI_UI_ALLOWED_ORIGINS || '')
    .split(',')
    .map((origin) => origin.trim())
    .filter(Boolean),
  ...(RENDER_EXTERNAL_HOST ? [`https://${RENDER_EXTERNAL_HOST}`] : []),
  ...configuredHttpsOrigins(CONFIGURED_HOST_ENTRIES),
]);
const GATEWAY_PROXY_LOCK = process.env.OMNI_GATEWAY_PROXY_LOCK === '1';
const ROOT = __dirname;
const REPO_ROOT = path.resolve(ROOT, '..');
const DEV_DIR = path.join(REPO_ROOT, '.dev');
const DEV_KEY_FILE = path.join(DEV_DIR, 'api-key.txt');
const DEV_PID_FILE = path.join(DEV_DIR, 'gateway.pid');
const GATEWAY_BINARY = path.join(REPO_ROOT, 'target', 'debug', 'omni-gateway.exe');
const GATEWAY_CONFIG = path.join(DEV_DIR, 'gateway.toml');
const GATEWAY_STATE = path.join(DEV_DIR, 'gateway-state.json');
const MANAGED_GATEWAY = process.env.OMNI_DEV_MANAGED === '1'
  && samePath(process.env.OMNI_DEV_KEY_FILE, DEV_KEY_FILE)
  && samePath(process.env.OMNI_GATEWAY_BINARY, GATEWAY_BINARY)
  && fs.existsSync(GATEWAY_BINARY)
  && fs.existsSync(GATEWAY_CONFIG)
  && fs.existsSync(DEV_KEY_FILE);
const KEY_MANAGEMENT_ENABLED = MANAGED_GATEWAY || process.env.OMNI_KEY_ADMIN_ENABLED === '1';
const KEY_ADMIN_REQUIRED = KEY_MANAGEMENT_ENABLED && !MANAGED_GATEWAY;
const STATIC_FILES = new Map([
  ['/', ['index.html', 'text/html; charset=utf-8']],
  ['/index.html', ['index.html', 'text/html; charset=utf-8']],
  ['/styles.css', ['styles.css', 'text/css; charset=utf-8']],
  ['/app.js', ['app.js', 'text/javascript; charset=utf-8']],
  ['/omni-logo.png', ['omni-logo.png', 'image/png']],
  ['/robots.txt', ['robots.txt', 'text/plain; charset=utf-8']],
  ['/sitemap.xml', ['sitemap.xml', 'application/xml; charset=utf-8']],
  [GOOGLE_VERIFICATION_PATH, ['googlebf0032297d3c5fef.html', 'text/html; charset=utf-8']],
]);

if (!Number.isInteger(PORT) || PORT < 1 || PORT > 65535) {
  throw new Error('PORT must be an integer between 1 and 65535');
}

let managedGateway = null;
let managedGatewayState = MANAGED_GATEWAY ? 'starting' : 'disabled';
let managedGatewayError = '';

function samePath(actual, expected) {
  return typeof actual === 'string'
    && path.resolve(actual).toLowerCase() === path.resolve(expected).toLowerCase();
}

function normalizeHostEntry(value) {
  if (typeof value !== 'string' || !value.trim() || value.includes('*')) return '';
  try {
    const raw = value.trim();
    const target = new URL(raw.includes('://') ? raw : `http://${raw}`);
    if (target.username || target.password || target.pathname !== '/' || target.search || target.hash) {
      return '';
    }
    return target.host.toLowerCase();
  } catch {
    return '';
  }
}

function configuredPublicHostUrl(hosts) {
  const host = hosts.find((entry) => !localHost(new URL(`http://${entry}`).hostname));
  return host ? `https://${host}` : '';
}

function configuredHttpsOrigins(hosts) {
  return hosts
    .filter((host) => !localHost(new URL(`http://${host}`).hostname))
    .map((host) => new URL(`https://${host}`).origin);
}

function normalizePublicSiteOrigin(value) {
  if (!value) return '';
  let url;
  try {
    url = new URL(value);
  } catch {
    throw new Error('OMNI_PUBLIC_SITE_URL must be an HTTP(S) origin');
  }
  if (!['http:', 'https:'].includes(url.protocol)
      || url.username
      || url.password
      || (url.pathname !== '/' && url.pathname !== '')
      || url.search
      || url.hash
      || (url.protocol === 'http:' && !localHost(url.hostname))) {
    throw new Error('OMNI_PUBLIC_SITE_URL must be a public HTTPS origin (HTTP is allowed only for loopback)');
  }
  return url.origin;
}

function publicSiteOrigin(request) {
  if (CONFIGURED_PUBLIC_SITE_ORIGIN) return CONFIGURED_PUBLIC_SITE_ORIGIN;
  if (RENDER_EXTERNAL_HOST) return new URL(`https://${RENDER_EXTERNAL_HOST}`).origin;
  const host = normalizeHostEntry(request.headers.host);
  if (host && !localHost(new URL(`http://${host}`).hostname)) {
    return new URL(`https://${host}`).origin;
  }
  return DEFAULT_PUBLIC_SITE_ORIGIN;
}

function readManagedKeyEntries() {
  const contents = fs.readFileSync(DEV_KEY_FILE, 'utf8').trim();
  if (!contents) throw new Error('The local API key file is empty.');
  const entries = contents.split(',').map((entry) => entry.trim());
  for (const [index, entry] of entries.entries()) {
    const separator = entry.indexOf(':');
    const tenant = separator < 0 ? '' : entry.slice(0, separator).trim();
    const token = separator < 0 ? '' : entry.slice(separator + 1).trim();
    if (!tenant || token.length < 24 || token.length > 512) {
      throw new Error(`Local API key file entry ${index + 1} is invalid; expected tenant:key.`);
    }
  }
  return entries;
}

function waitForChildExit(child, timeoutMs) {
  return new Promise((resolve) => {
    if (child.exitCode !== null || child.signalCode !== null) {
      resolve();
      return;
    }
    let timer;
    const finish = () => {
      if (timer) clearTimeout(timer);
      resolve();
    };
    child.once('close', finish);
    timer = setTimeout(() => {
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      finish();
    }, timeoutMs);
  });
}

async function stopManagedGateway() {
  const child = managedGateway;
  managedGateway = null;
  if (!child) return;
  if (child.exitCode === null && child.signalCode === null) child.kill();
  await waitForChildExit(child, 5000);
  try {
    if (fs.readFileSync(DEV_PID_FILE, 'utf8').trim() === String(child.pid)) {
      fs.unlinkSync(DEV_PID_FILE);
    }
  } catch (error) {
    if (error.code !== 'ENOENT') console.error(`Could not clean gateway PID file: ${error.message}`);
  }
}

function requestManagedHealth() {
  return new Promise((resolve, reject) => {
    let target;
    try {
      target = new URL('/v1/health', process.env.OMNI_GATEWAY_URL || DEFAULT_GATEWAY_URL);
      if (target.protocol !== 'https:' || !localHost(target.hostname)) {
        throw new Error('Managed local gateway must use HTTPS on a loopback address.');
      }
    } catch (error) {
      reject(error);
      return;
    }
    const request = https.request(target, {
      method: 'GET',
      headers: { Accept: 'application/json' },
      rejectUnauthorized: false,
    }, (response) => {
      const chunks = [];
      let size = 0;
      response.on('data', (chunk) => {
        size += chunk.length;
        if (size > 64 * 1024) {
          response.destroy(new Error('Health response exceeded the local limit.'));
          return;
        }
        chunks.push(chunk);
      });
      response.on('end', () => {
        try {
          const payload = JSON.parse(Buffer.concat(chunks).toString('utf8'));
          if (response.statusCode === 200 && payload.status === 'ok') resolve();
          else reject(new Error(payload.error && payload.error.message
            ? payload.error.message
            : `Gateway health returned HTTP ${response.statusCode || 502}.`));
        } catch (error) {
          reject(error);
        }
      });
      response.on('error', reject);
    });
    request.setTimeout(1500, () => request.destroy(new Error('Gateway health check timed out.')));
    request.on('error', reject);
    request.end();
  });
}

async function waitForManagedHealth(child) {
  const deadline = Date.now() + 20_000;
  let lastError = 'Gateway has not reported healthy yet.';
  while (Date.now() < deadline) {
    if (child.exitCode !== null || child.signalCode !== null) {
      throw new Error(`omni-gateway exited during startup (code ${child.exitCode}, signal ${child.signalCode || 'none'}). Check .dev\\gateway.stderr.log.`);
    }
    try {
      await requestManagedHealth();
      return;
    } catch (error) {
      lastError = error.message;
      await new Promise((resolve) => setTimeout(resolve, 300));
    }
  }
  throw new Error(`The managed gateway did not become healthy: ${lastError}`);
}

async function startManagedGateway(entries) {
  if (!MANAGED_GATEWAY) throw new Error('The managed local gateway is not configured. Start services with .\\scripts\\dev-up.ps1.');
  managedGatewayState = 'starting';
  managedGatewayError = '';
  await stopManagedGateway();

  const stdoutPath = path.join(DEV_DIR, 'gateway.stdout.log');
  const stderrPath = path.join(DEV_DIR, 'gateway.stderr.log');
  const stdout = fs.createWriteStream(stdoutPath, { flags: 'a' });
  const stderr = fs.createWriteStream(stderrPath, { flags: 'a' });
  process.env.OMNI_API_KEYS = entries.join(',');
  const child = spawn(GATEWAY_BINARY, [], {
    cwd: REPO_ROOT,
    env: {
      ...process.env,
      OMNI_GATEWAY_CONFIG: GATEWAY_CONFIG,
      OMNI_GATEWAY_STATE: GATEWAY_STATE,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
    windowsHide: true,
  });
  managedGateway = child;
  child.stdout.pipe(stdout);
  child.stderr.pipe(stderr);
  stdout.on('error', (error) => console.error(`Could not write gateway stdout log: ${error.message}`));
  stderr.on('error', (error) => console.error(`Could not write gateway stderr log: ${error.message}`));
  child.once('error', (error) => {
    managedGatewayError = `Could not launch omni-gateway: ${error.message}`;
    managedGatewayState = 'error';
  });
  child.once('close', (code, signal) => {
    stdout.end();
    stderr.end();
    if (managedGateway === child) {
      managedGateway = null;
      managedGatewayState = 'error';
      managedGatewayError = `omni-gateway stopped (code ${code}, signal ${signal || 'none'}). Check .dev\\gateway.stderr.log.`;
    }
    try {
      if (fs.readFileSync(DEV_PID_FILE, 'utf8').trim() === String(child.pid)) {
        fs.unlinkSync(DEV_PID_FILE);
      }
    } catch (error) {
      if (error.code !== 'ENOENT') console.error(`Could not clean gateway PID file: ${error.message}`);
    }
  });
  fs.writeFileSync(DEV_PID_FILE, String(child.pid), 'ascii');
  try {
    await waitForManagedHealth(child);
    managedGatewayState = 'online';
  } catch (error) {
    managedGatewayError = error.message;
    managedGatewayState = 'error';
    await stopManagedGateway();
    throw error;
  }
}

async function handleKeyManagementProxy(request, response, route) {
  if (!KEY_MANAGEMENT_ENABLED) {
    sendJson(response, 503, {
      error: {
        code: 'key_manager_unavailable',
        message: 'Key management is not enabled for this dashboard deployment.',
      },
    });
    return;
  }
  if (!String(request.headers['content-type'] || '').toLowerCase().startsWith('application/json')) {
    sendJson(response, 415, {
      error: { code: 'unsupported_media_type', message: 'Send application/json' },
    });
    return;
  }
  try {
    const body = await readBody(request);
    proxyToGateway(request, response, route, body);
  } catch (error) {
    sendJson(response, error.status || 400, {
      error: { code: error.code || 'bad_request', message: error.message },
    });
  }
}

function setSecurityHeaders(response) {
  response.setHeader('X-Content-Type-Options', 'nosniff');
  response.setHeader('X-Frame-Options', 'DENY');
  response.setHeader('Referrer-Policy', 'no-referrer');
  response.setHeader(
    'Content-Security-Policy',
    "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
  );
}

function sendJson(response, status, payload) {
  if (response.headersSent || response.writableEnded) return;
  const body = Buffer.from(JSON.stringify(payload));
  setSecurityHeaders(response);
  response.writeHead(status, {
    'Cache-Control': 'no-store',
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': body.length,
  });
  response.end(body);
}

function localHost(hostname) {
  const value = hostname.toLowerCase().replace(/^\[|\]$/g, '');
  return value === 'localhost' || value === '127.0.0.1' || value === '::1';
}

function isLoopbackRequest(request) {
  const address = String(request.socket.remoteAddress || '')
    .toLowerCase()
    .replace(/^::ffff:/, '')
    .split('%')[0];
  return address === '::1' || /^127(?:\.\d{1,3}){3}$/.test(address);
}

function allowedHostHeader(hostHeader) {
  if (typeof hostHeader !== 'string') return false;
  const rawHost = hostHeader.trim().toLowerCase();
  if (ALLOW_ANY_HOST || ALLOWED_HOSTS.has(rawHost)) return true;
  const normalized = normalizeHostEntry(hostHeader);
  if (!normalized) return false;
  if (ALLOWED_HOSTS.has(normalized)) return true;
  if (!RENDER_MODE) return false;
  const hostname = new URL(`http://${normalized}`).hostname.toLowerCase();
  return hostname.endsWith('.onrender.com');
}

function allowedOrigin(origin, hostHeader) {
  if (!origin) return true;
  if (ALLOWED_ORIGINS.has(origin)) return true;
  try {
    const parsed = new URL(origin);
    const requestHost = normalizeHostEntry(hostHeader);
    if (RENDER_MODE
        && parsed.protocol === 'https:'
        && normalizeHostEntry(parsed.host) === requestHost
        && allowedHostHeader(hostHeader)) {
      return true;
    }
    return parsed.protocol === 'http:'
      && localHost(parsed.hostname)
      && (parsed.port === String(PORT) || (parsed.port === '' && PORT === 80));
  } catch {
    return false;
  }
}

function gatewayBase(request) {
  const requested = request.headers['x-omni-gateway-url'];
  let raw = typeof requested === 'string' && requested.trim()
    ? requested.trim()
    : (process.env.OMNI_GATEWAY_URL || DEFAULT_GATEWAY_URL);
  const internalGatewayUrl = process.env.OMNI_GATEWAY_URL;
  const publicGatewayUrl = PUBLIC_GATEWAY_URL;
  if (GATEWAY_PROXY_LOCK) {
    const allowedTargets = new Set(
      [internalGatewayUrl, publicGatewayUrl].filter(Boolean).map((value) => {
        const target = new URL(value);
        return target.origin;
      }),
    );
    let selectedOrigin;
    try {
      selectedOrigin = new URL(raw).origin;
    } catch {
      throw new Error('Gateway URL must be a valid HTTP(S) origin');
    }
    if (!allowedTargets.has(selectedOrigin)) {
      throw new Error('Gateway URL is not in this dashboard bridge’s configured upstream allowlist');
    }
    if (publicGatewayUrl && selectedOrigin === new URL(publicGatewayUrl).origin) {
      raw = internalGatewayUrl || publicGatewayUrl;
    }
  }
  const target = new URL(raw);
  if (!['http:', 'https:'].includes(target.protocol)
      || target.username
      || target.password
      || target.search
      || target.hash
      || (target.pathname !== '/' && target.pathname !== '')) {
    throw new Error('Gateway URL must be an HTTP(S) origin without credentials or a path');
  }
  if (target.protocol === 'http:'
      && !localHost(target.hostname)
      && process.env.OMNI_GATEWAY_ALLOW_HTTP !== '1') {
    throw new Error('Non-local gateways must use HTTPS');
  }
  return target;
}

function readBody(request) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    request.on('data', (chunk) => {
      size += chunk.length;
      if (size > MAX_REQUEST_BYTES) {
        reject(Object.assign(new Error('Request body exceeds the local bridge limit'), {
          status: 413,
          code: 'payload_too_large',
        }));
        request.resume();
        return;
      }
      chunks.push(chunk);
    });
    request.on('end', () => resolve(Buffer.concat(chunks)));
    request.on('error', reject);
  });
}

function proxyToGateway(request, response, route, body) {
  let base;
  try {
    base = gatewayBase(request);
  } catch (error) {
    sendJson(response, 400, {
      error: { code: 'invalid_gateway_url', message: error.message },
    });
    return;
  }

  const target = new URL(route, base);
  const transport = target.protocol === 'https:' ? https : http;
  const headers = { Accept: 'application/json' };
  if (body) {
    headers['Content-Type'] = request.headers['content-type'] || 'application/json';
    headers['Content-Length'] = body.length;
    if (typeof request.headers.authorization === 'string') {
      headers.Authorization = request.headers.authorization;
    }
  }

  const options = { method: body ? 'POST' : 'GET', headers };
  if (target.protocol === 'https:') {
    const allowLocalDevCertificate = process.env.OMNI_GATEWAY_INSECURE_TLS === '1'
      && localHost(target.hostname);
    options.rejectUnauthorized = !allowLocalDevCertificate;
  }

  let settled = false;
  const fail = (status, code, message) => {
    if (settled) return;
    settled = true;
    sendJson(response, status, { error: { code, message } });
  };

  const upstreamRequest = transport.request(target, options, (upstream) => {
    const chunks = [];
    let size = 0;
    upstream.on('data', (chunk) => {
      size += chunk.length;
      if (size > MAX_RESPONSE_BYTES) {
        upstream.destroy();
        fail(502, 'gateway_response_too_large', 'Gateway response exceeds the bridge limit');
        return;
      }
      chunks.push(chunk);
    });
    upstream.on('end', () => {
      if (settled) return;
      const raw = Buffer.concat(chunks).toString('utf8');
      let payload;
      try {
        payload = JSON.parse(raw);
      } catch {
        fail(502, 'invalid_gateway_response', 'Gateway returned a non-JSON response');
        return;
      }
      settled = true;
      sendJson(response, upstream.statusCode || 502, payload);
    });
    upstream.on('error', (error) => {
      fail(503, 'gateway_unavailable', gatewayFailureMessage(target, error));
    });
  });

  upstreamRequest.setTimeout(10_000, () => {
    upstreamRequest.destroy(new Error('Gateway request timed out'));
  });
  upstreamRequest.on('error', (error) => {
    fail(503, 'gateway_unavailable', gatewayFailureMessage(target, error));
  });
  upstreamRequest.end(body);
}

function gatewayFailureMessage(target, error) {
  if (error.code === 'ECONNREFUSED') {
    return `No gateway is listening at ${target.host}. Run .\\scripts\\dev-up.ps1 to generate local TLS files and start both services; if started manually, check the gateway terminal for OMNI_API_KEYS or certificate errors.`;
  }
  if (error.code === 'EADDRNOTAVAIL' || error.code === 'ETIMEDOUT') {
    return `Cannot reach gateway at ${target.host} (${error.code}). Check the gateway URL and that its port is available.`;
  }
  if (target.protocol === 'https:'
      && typeof error.code === 'string'
      && (error.code.startsWith('ERR_TLS')
        || error.code.startsWith('ERR_SSL')
        || ['DEPTH_ZERO_SELF_SIGNED_CERT', 'SELF_SIGNED_CERT_IN_CHAIN', 'UNABLE_TO_VERIFY_LEAF_SIGNATURE']
          .includes(error.code))) {
    return `TLS handshake with ${target.host} failed (${error.code}). For local development, run .\\scripts\\dev-up.ps1 to regenerate the certificate and key.`;
  }
  return `Gateway at ${target.host} closed the connection (${error.code || 'network error'}). Check its logs and TLS configuration.`;
}

async function handle(request, response) {
  setSecurityHeaders(response);
  let pathname;
  try {
    pathname = new URL(request.url, 'http://127.0.0.1').pathname;
  } catch {
    sendJson(response, 400, { error: { code: 'bad_request', message: 'Invalid request URL' } });
    return;
  }

  if (pathname === GOOGLE_VERIFICATION_PATH
      && (request.method === 'GET' || request.method === 'HEAD')) {
    serveStaticFile(request, response, pathname, STATIC_FILES.get(pathname));
    return;
  }

  if (!allowedHostHeader(request.headers.host)) {
    sendJson(response, 403, {
      error: {
        code: 'invalid_host',
        message: 'Host is not allowed. Configure OMNI_ALLOWED_HOSTS with this hostname; local defaults include localhost:3000.',
      },
    });
    return;
  }
  if (!allowedOrigin(request.headers.origin, request.headers.host)) {
    sendJson(response, 403, { error: { code: 'invalid_origin', message: 'Origin is not allowed' } });
    return;
  }

  if (pathname === '/api/config' && request.method === 'GET') {
    sendJson(response, 200, {
      gateway_url: DEFAULT_GATEWAY_URL,
      api_key: process.env.OMNI_DEV_API_KEY || '',
      key_admin_token: MANAGED_GATEWAY && isLoopbackRequest(request)
        ? process.env.OMNI_DEV_KEY_ADMIN_TOKEN || ''
        : '',
      key_management_enabled: KEY_MANAGEMENT_ENABLED,
      key_admin_required: KEY_ADMIN_REQUIRED,
      managed_gateway_status: managedGatewayState,
      managed_gateway_error: managedGatewayError,
    });
    return;
  }
  if (pathname === '/api/keys/generate' && request.method === 'POST') {
    await handleKeyManagementProxy(request, response, '/v1/keys');
    return;
  }
  if (pathname === '/api/keys/revoke' && request.method === 'POST') {
    await handleKeyManagementProxy(request, response, '/v1/keys/revoke');
    return;
  }
  if (pathname === '/v1/health' && request.method === 'GET') {
    proxyToGateway(request, response, '/v1/health');
    return;
  }
  if (pathname === '/v1/languages' && request.method === 'GET') {
    proxyToGateway(request, response, '/v1/languages');
    return;
  }
  if ((pathname === '/v1/keys' || pathname === '/v1/keys/revoke')
      && request.method === 'POST') {
    await handleKeyManagementProxy(request, response, pathname);
    return;
  }
  if (pathname === '/v1/execute' && request.method === 'POST') {
    if (!String(request.headers['content-type'] || '').toLowerCase().startsWith('application/json')) {
      sendJson(response, 415, {
        error: { code: 'unsupported_media_type', message: 'Send application/json' },
      });
      return;
    }
    try {
      const body = await readBody(request);
      proxyToGateway(request, response, '/v1/execute', body);
    } catch (error) {
      sendJson(response, error.status || 400, {
        error: { code: error.code || 'bad_request', message: error.message },
      });
    }
    return;
  }
  if (pathname === '/api/health' && request.method === 'GET') {
    proxyToGateway(request, response, '/v1/health');
    return;
  }
  if (pathname === '/api/languages' && request.method === 'GET') {
    proxyToGateway(request, response, '/v1/languages');
    return;
  }
  if (pathname === '/api/execute' && request.method === 'POST') {
    if (!String(request.headers['content-type'] || '')
      .toLowerCase()
      .startsWith('application/json')) {
      sendJson(response, 415, {
        error: { code: 'unsupported_media_type', message: 'Send application/json' },
      });
      return;
    }
    let body;
    try {
      body = await readBody(request);
    } catch (error) {
      sendJson(response, error.status || 400, {
        error: {
          code: error.code || 'bad_request',
          message: error.message || 'Could not read request body',
        },
      });
      return;
    }
    try {
      const parsed = JSON.parse(body.toString('utf8'));
      if (!parsed || typeof parsed !== 'object' || typeof parsed.source !== 'string') {
        sendJson(response, 400, {
          error: { code: 'invalid_request', message: 'JSON body must contain a source string' },
        });
        return;
      }
    } catch {
      sendJson(response, 400, {
        error: { code: 'invalid_json', message: 'Request body must be valid JSON' },
      });
      return;
    }
    proxyToGateway(request, response, '/v1/execute', body);
    return;
  }

  const asset = STATIC_FILES.get(pathname);
  if (asset && (request.method === 'GET' || request.method === 'HEAD')) {
    serveStaticFile(request, response, pathname, asset);
    return;
  }

  if (pathname.startsWith('/api/') || pathname.startsWith('/v1/')) {
    sendJson(response, 404, { error: { code: 'not_found', message: 'API route not found' } });
    return;
  }
  if (request.method !== 'GET') {
    sendJson(response, 405, { error: { code: 'method_not_allowed', message: 'GET is required' } });
    return;
  }
  sendJson(response, 404, { error: { code: 'not_found', message: 'Dashboard asset not found' } });
}

function serveStaticFile(request, response, pathname, asset) {
  if (!asset) {
    sendJson(response, 404, { error: { code: 'not_found', message: 'Static asset not found' } });
    return;
  }
  const filePath = path.join(ROOT, asset[0]);
  fs.readFile(filePath, (error, contents) => {
    if (error) {
      sendJson(response, 500, {
        error: { code: 'asset_unavailable', message: 'Dashboard asset could not be read' },
      });
      return;
    }
    let body = contents;
    const isTextAsset = asset[1].startsWith('text/')
      || asset[1].startsWith('application/xml');
    if (pathname !== GOOGLE_VERIFICATION_PATH && isTextAsset) {
      const siteOrigin = publicSiteOrigin(request);
      body = Buffer.from(contents.toString('utf8')
        .split(SITE_ORIGIN_TOKEN).join(siteOrigin)
        .split(CANONICAL_URL_TOKEN).join(`${siteOrigin}/`));
    }
    setSecurityHeaders(response);
    response.writeHead(200, {
      'Cache-Control': 'no-store',
      'Content-Type': asset[1],
      'Content-Length': body.length,
    });
    response.end(request.method === 'HEAD' ? undefined : body);
  });
}

const server = http.createServer((request, response) => {
  handle(request, response).catch(() => {
    sendJson(response, 500, {
      error: { code: 'bridge_error', message: 'Local dashboard bridge failed safely' },
    });
  });
});

server.listen(PORT, HOST, () => {
  console.log(`OmniEngine dashboard: http://localhost:${PORT}`);
  console.log(`Gateway target: ${process.env.OMNI_GATEWAY_URL || DEFAULT_GATEWAY_URL}`);
  if (MANAGED_GATEWAY) {
    startManagedGateway(readManagedKeyEntries())
      .catch((error) => {
        managedGatewayState = 'error';
        managedGatewayError = error.message;
        console.error(`Managed gateway startup failed: ${error.message}`);
      });
  } else if (process.env.OMNI_KEY_ADMIN_ENABLED === '1') {
    console.log('Gateway key management is enabled; requests require the configured administrator bearer token.');
  } else {
    console.log('Gateway key management is disabled; set OMNI_KEY_ADMIN_ENABLED=1 and configure OMNI_KEY_ADMIN_TOKEN on the gateway to enable it.');
  }
});

server.on('error', (error) => {
  const message = error.code === 'EADDRINUSE'
    ? `Port ${PORT} is already in use. Close the other dashboard process, then run .\\scripts\\dev-up.ps1 again.`
    : `OmniEngine dashboard could not listen on ${HOST}:${PORT}: ${error.message}`;
  console.error(message);
  process.exitCode = 1;
});

process.once('SIGINT', () => {
  server.close();
  stopManagedGateway().finally(() => process.exit(0));
});
process.once('SIGTERM', () => {
  server.close();
  stopManagedGateway().finally(() => process.exit(0));
});
