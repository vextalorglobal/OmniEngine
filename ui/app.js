'use strict';

const byId = (id) => document.getElementById(id);
const gatewayUrlInput = byId('gatewayUrl');
const apiKeyInput = byId('apiKey');
const languageSelector = byId('languageSelector');
const editor = byId('sourceEditor');
const lineNumbers = byId('lineNumbers');
const runButton = byId('runButton');
const LOCAL_GATEWAY_URL = 'https://127.0.0.1:8443';
const gatewayUrlStorageKey = 'omni-gateway-url';
const historyStorageKey = 'omni-execution-history';
const generatedKeyStorageKey = 'omni-generated-api-keys';
const languageStorageKey = 'omni-execution-language';
const historyLimit = 12;
const cpuLimitMs = 50;
const memoryLimitBytes = 128 * 1024 * 1024;
const fallbackLanguages = [
  { id: 'omni', name: 'OmniEngine', runtime: 'omni-core', available: true },
  { id: 'lua', name: 'Lua 5.4', runtime: 'mlua / Lua 5.4', available: true },
  { id: 'javascript', name: 'JavaScript', runtime: 'not configured', available: false, unavailable_reason: 'No embedded JavaScript interpreter with enforced gas and memory limits is configured; host processes are disabled.' },
  { id: 'python', name: 'Python', runtime: 'not configured', available: false, unavailable_reason: 'No embedded Python interpreter with enforced gas and memory limits is configured; host processes are disabled.' },
  {
    id: 'wasm',
    name: 'WebAssembly',
    runtime: 'Wasmi 0.46 / WAT',
    available: true,
    standard_libraries: [],
    capabilities: {
      filesystem_reads: false,
      filesystem_writes: false,
      module_imports: false,
      host_functions: false,
      network_access: false,
      gas_limit: true,
      cpu_limit: true,
      memory_limit: true,
      unicode_text: false,
      deterministic_verification: false,
      cluster_dispatch: false,
    },
  },
];
let history = loadExecutionHistory();
let generatedApiKeys = getGeneratedApiKeys();
let runtimeLanguages = fallbackLanguages;
let keyManagementEnabled = false;
let gatewayReachable = false;
let configuredGatewayUrl = LOCAL_GATEWAY_URL;
let selectedHistoryId = null;
let nextHistoryId = Date.now() * 1000;
let activePreset = 'arithmetic';
let displayedRecord = null;

const presets = {
  omni: {
    arithmetic: {
      label: 'Arithmetic',
      source: 'let principal = 40\nlet adjustment = 2\nprincipal + adjustment',
      gas: '',
      timeout: '',
      verification: '',
    },
    gas: {
      label: 'Gas limit',
      source: numericChain(300),
      gas: '500',
      timeout: '50',
      verification: '',
    },
    memory: {
      label: 'Memory load',
      source: stringChain(1400),
      gas: '50000',
      timeout: '50',
      verification: '',
    },
    unicode: {
      label: 'Unicode text',
      source: '// বাংলা — हिन्दी — 日本語 — العربية — Ελληνικά — 😀\nlet greeting = "বাংলা — नमस्ते — 日本語 — العربية — Ελληνικά — 😀"\ngreeting',
      gas: '',
      timeout: '',
      verification: '',
    },
  },
  lua: {
    arithmetic: {
      label: 'Arithmetic',
      source: 'local principal = 40\nlocal adjustment = 2\nreturn principal + adjustment',
      gas: '',
      timeout: '',
      verification: '',
    },
    gas: {
      label: 'Gas limit',
      source: 'local total = 0\nfor i = 1, 5000 do\n  total = total + i\nend\nreturn total',
      gas: '500',
      timeout: '50',
      verification: '',
    },
    memory: {
      label: 'Memory load',
      source: 'local values = {}\nfor i = 1, 10000 do\n  values[i] = {i, i + 1, i + 2}\nend\nreturn #values',
      gas: '50000',
      timeout: '50',
      verification: '',
    },
    unicode: {
      label: 'Unicode text',
      source: '-- বাংলা — हिन्दी — 日本語 — العربية — Ελληνικά — 😀\nreturn "বাংলা — नमस्ते — 日本語 — العربية — Ελληνικά — 😀"',
      gas: '',
      timeout: '',
      verification: '',
    },
  },
  wasm: {
    arithmetic: {
      label: 'Arithmetic',
      source: ';; WAT module: export a no-argument run function.\n(module\n  (func (export "run") (result i32)\n    i32.const 42))',
      gas: '',
      timeout: '',
      verification: '',
    },
    gas: {
      label: 'Gas limit',
      source: '(module\n  (func (export "run")\n    (loop $again\n      (br $again))))',
      gas: '500',
      timeout: '50',
      verification: '',
    },
    memory: {
      label: 'Memory load',
      source: '(module\n  (memory (export "memory") 1 2)\n  (func (export "run") (result i32)\n    memory.size))',
      gas: '50000',
      timeout: '50',
      verification: '',
    },
  },
};

function numericChain(count) {
  const statements = ['let x0 = 0'];
  for (let index = 1; index < count; index += 1) {
    statements.push(`let x${index} = x${index - 1} + 1`);
  }
  return statements.join('\n');
}

function stringChain(count) {
  const statements = ['let x0 = "a"'];
  for (let index = 1; index < count; index += 1) {
    statements.push(`let x${index} = x${index - 1} + "a"`);
  }
  return statements.join('\n');
}

function getSessionValue(key) {
  try {
    return sessionStorage.getItem(key) || '';
  } catch {
    return '';
  }
}

function setSessionValue(key, value) {
  try {
    sessionStorage.setItem(key, value);
  } catch {
    byId('runNote').textContent = 'Session storage is unavailable; connection values will not persist.';
  }
}

function getPersistentValue(key) {
  try {
    return localStorage.getItem(key) || '';
  } catch {
    return '';
  }
}

function setPersistentValue(key, value) {
  try {
    localStorage.setItem(key, value);
  } catch {
    byId('runNote').textContent = 'Local storage is unavailable; the gateway URL will not persist.';
  }
}

function updateGatewayMode() {
  const url = selectedGatewayUrl();
  byId('gatewayMode').value = url === LOCAL_GATEWAY_URL ? 'local' : 'custom';
}

function selectGatewayMode(mode) {
  if (mode === 'local') {
    gatewayUrlInput.value = LOCAL_GATEWAY_URL;
  } else if (gatewayUrlInput.value === LOCAL_GATEWAY_URL) {
    gatewayUrlInput.value = configuredGatewayUrl === LOCAL_GATEWAY_URL
      ? ''
      : configuredGatewayUrl;
  }
  setPersistentValue(gatewayUrlStorageKey, gatewayUrlInput.value);
  updateGatewayMode();
  if (gatewayUrlInput.value) {
    checkGateway();
    loadLanguages();
  } else {
    setConnectionState('offline', 'Gateway URL required');
    gatewayUrlInput.focus();
  }
}

function selectedLanguage() {
  return languageSelector.value || 'omni';
}

function languageDescriptor(id = selectedLanguage()) {
  return runtimeLanguages.find((language) => language.id === id) || null;
}

function updateLanguageStatus() {
  const descriptor = languageDescriptor();
  const status = byId('languageRuntimeStatus');
  if (!descriptor) {
    status.textContent = 'Runtime status unavailable; the gateway remains authoritative.';
    status.dataset.available = 'false';
    return;
  }
  status.dataset.available = String(descriptor.available === true);
  if (!descriptor.available) {
    status.textContent = `${descriptor.name} unavailable · ${descriptor.unavailable_reason || descriptor.runtime}`;
    return;
  }
  const capabilities = descriptor.capabilities || {};
  const libraries = Array.isArray(descriptor.standard_libraries)
    ? descriptor.standard_libraries.join(', ')
    : '';
  const executionScope = capabilities.cluster_dispatch === false
    ? `gateway-local only; no worker dispatch or deterministic quorum${libraries ? `; libraries: ${libraries}` : ''}`
    : 'sandboxed; no filesystem, network, imports, or host APIs';
  status.textContent = `${descriptor.name} · ${descriptor.runtime} · ${executionScope}`;
}

function renderLanguageOptions(preferredLanguage) {
  const options = runtimeLanguages.map((descriptor) => {
    const option = document.createElement('option');
    option.value = descriptor.id;
    option.textContent = `${descriptor.name} · ${descriptor.available ? 'available' : 'unavailable'}`;
    return option;
  });
  languageSelector.replaceChildren(...options);
  const preferred = runtimeLanguages.some((language) => language.id === preferredLanguage)
    ? preferredLanguage
    : 'omni';
  languageSelector.value = preferred;
  updateLanguageStatus();
  renderPresets();
}

async function loadLanguages() {
  const preferred = getSessionValue(languageStorageKey) || selectedLanguage();
  try {
    const response = await fetch('/api/languages', {
      headers: { 'X-Omni-Gateway-URL': selectedGatewayUrl() },
      cache: 'no-store',
    });
    const payload = await response.json();
    const languages = Array.isArray(payload.languages)
      ? payload.languages.filter((language) => language
        && typeof language.id === 'string'
        && typeof language.name === 'string'
        && typeof language.runtime === 'string'
        && typeof language.available === 'boolean')
      : [];
    if (!response.ok || !languages.some((language) => language.id === 'omni' && language.available)) {
      throw new Error('Gateway did not return a valid language catalog');
    }
    runtimeLanguages = languages;
  } catch {
    runtimeLanguages = fallbackLanguages;
  }
  renderLanguageOptions(preferred);
}

function loadExecutionHistory() {
  try {
    const stored = JSON.parse(sessionStorage.getItem(historyStorageKey) || '[]');
    if (!Array.isArray(stored)) return [];
    return stored
      .filter((record) => record
        && typeof record.source === 'string'
        && Number.isInteger(record.httpStatus)
        && record.payload
        && typeof record.payload === 'object'
        && Number.isFinite(Number(record.latencyMs)))
      .slice(0, historyLimit)
      .map((record, index) => ({
        ...record,
        id: Number.isFinite(Number(record.id)) ? Number(record.id) : Date.now() + index,
        language: typeof record.language === 'string' ? record.language : 'omni',
        preset: typeof record.preset === 'string' ? record.preset : 'custom',
        createdAt: typeof record.createdAt === 'string' && Number.isFinite(Date.parse(record.createdAt))
          ? record.createdAt
          : new Date().toISOString(),
        latencyMs: Math.max(0, Number(record.latencyMs)),
      }));
  } catch {
    return [];
  }
}

function saveExecutionHistory() {
  while (history.length > 0) {
    try {
      sessionStorage.setItem(historyStorageKey, JSON.stringify(history));
      return true;
    } catch {
      if (history.length === 1) break;
      history.pop();
    }
  }
  byId('runNote').textContent = 'This execution could not be saved to browser session history.';
  return false;
}

function getGeneratedApiKeys() {
  try {
    const stored = JSON.parse(sessionStorage.getItem(generatedKeyStorageKey) || '[]');
    return Array.isArray(stored)
      ? stored.filter((entry) => entry && typeof entry.api_key === 'string')
      : [];
  } catch {
    return [];
  }
}

function saveGeneratedApiKeys() {
  try {
    sessionStorage.setItem(generatedKeyStorageKey, JSON.stringify(generatedApiKeys));
    return true;
  } catch {
    setKeyGeneratorStatus('Session storage is unavailable; generated keys will be listed only until this page closes.', 'error');
    return false;
  }
}

function setKeyGeneratorStatus(message, state) {
  const status = byId('keyGeneratorStatus');
  status.textContent = message;
  if (state) status.dataset.state = state;
  else delete status.dataset.state;
}

function setKeyManagementEnabled(enabled) {
  keyManagementEnabled = enabled;
  byId('generateKeyButton').disabled = !enabled;
  byId('keyLabel').disabled = !enabled;
  byId('keyTier').disabled = !enabled;
  if (!enabled) {
    setKeyGeneratorStatus(
      'Key generation and runtime sync require the managed local gateway. Start it with .\\scripts\\dev-up.ps1.',
      'error',
    );
  }
}

function createKeyAction(label, action) {
  const button = document.createElement('button');
  button.type = 'button';
  button.className = 'text-button';
  button.textContent = label;
  button.addEventListener('click', action);
  return button;
}

function maskApiKey(apiKey) {
  return `${apiKey.slice(0, 15)}…${apiKey.slice(-5)}`;
}

async function copyText(text) {
  if (navigator.clipboard && window.isSecureContext) {
    await navigator.clipboard.writeText(text);
    return;
  }
  const buffer = document.createElement('textarea');
  buffer.className = 'copy-buffer';
  buffer.readOnly = true;
  buffer.value = text;
  document.body.append(buffer);
  buffer.select();
  const copied = document.execCommand('copy');
  buffer.remove();
  if (!copied) throw new Error('Clipboard access is unavailable in this browser.');
}

async function copyGeneratedKey(apiKey, button) {
  try {
    await copyText(apiKey);
    const label = button.textContent;
    button.textContent = 'Copied';
    setKeyGeneratorStatus('API key copied to the clipboard.', 'success');
    window.setTimeout(() => {
      button.textContent = label;
    }, 1400);
  } catch (error) {
    setKeyGeneratorStatus(error.message, 'error');
  }
}

function renderGeneratedApiKeys() {
  const list = byId('generatedKeyList');
  list.replaceChildren();
  byId('clearGeneratedKeys').disabled = !keyManagementEnabled || generatedApiKeys.length === 0;
  if (generatedApiKeys.length === 0) {
    const empty = document.createElement('p');
    empty.className = 'key-list-empty';
    empty.textContent = 'No keys generated in this session.';
    list.append(empty);
    return;
  }

  for (const entry of generatedApiKeys) {
    const item = document.createElement('article');
    item.className = 'generated-key-item';
    const details = document.createElement('div');
    details.className = 'generated-key-details';

    const name = document.createElement('span');
    name.className = 'generated-key-name';
    name.textContent = entry.label || 'Local development key';

    const value = document.createElement('code');
    value.className = 'generated-key-value';
    value.textContent = entry.revealed ? entry.api_key : maskApiKey(entry.api_key);
    details.append(name, value);

    const actions = document.createElement('div');
    actions.className = 'generated-key-actions';
    const reveal = createKeyAction(entry.revealed ? 'Hide' : 'Reveal', () => {
      entry.revealed = !entry.revealed;
      renderGeneratedApiKeys();
    });
    const copy = createKeyAction('Copy', () => copyGeneratedKey(entry.api_key, copy));
    const use = createKeyAction('Use in Studio', () => useGeneratedApiKey(entry));
    const revoke = createKeyAction('Revoke', () => revokeGeneratedApiKeys([entry.api_key]));
    actions.append(reveal, copy, use, revoke);
    item.append(details, actions);
    list.append(item);
  }
}

async function postKeyManagerRequest(route, body) {
  const response = await fetch(route, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
    cache: 'no-store',
  });
  const payload = await response.json();
  if (!response.ok) {
    throw new Error(payload.error && payload.error.message
      ? payload.error.message
      : `Key manager returned HTTP ${response.status}`);
  }
  return payload;
}

async function generateApiKey() {
  if (!keyManagementEnabled) return;
  const label = byId('keyLabel').value.trim();
  if (!label || label.length > 48) {
    setKeyGeneratorStatus('Enter a label between 1 and 48 characters.', 'error');
    byId('keyLabel').focus();
    return;
  }

  const button = byId('generateKeyButton');
  button.disabled = true;
  setKeyGeneratorStatus('Generating a secure key and syncing the local gateway…');
  try {
    const payload = await postKeyManagerRequest('/api/keys/generate', {
      label,
      tier: byId('keyTier').value,
    });
    generatedApiKeys.unshift(payload.key);
    if (generatedApiKeys.length > 25) generatedApiKeys.length = 25;
    const storedForSession = saveGeneratedApiKeys();
    byId('generatedKeyOutput').value = payload.key.api_key;
    byId('generatedKeyResult').hidden = false;
    renderGeneratedApiKeys();
    setKeyGeneratorStatus(
      storedForSession
        ? 'Key generated and accepted by the managed gateway.'
        : 'Key is active in the gateway, but browser session storage is unavailable.',
      storedForSession ? 'success' : 'error',
    );
  } catch (error) {
    setKeyGeneratorStatus(error.message, 'error');
  } finally {
    button.disabled = !keyManagementEnabled;
  }
}

async function useGeneratedApiKey(entry) {
  apiKeyInput.value = entry.api_key;
  setSessionValue('omni-api-key', entry.api_key);
  if (entry.gateway_url) {
    gatewayUrlInput.value = entry.gateway_url;
    setPersistentValue(gatewayUrlStorageKey, entry.gateway_url);
    updateGatewayMode();
  }
  setKeyGeneratorStatus('Checking the selected key with the local gateway…');
  const online = await checkGateway();
  setKeyGeneratorStatus(
    online ? 'Key is active; the gateway health check succeeded.' : 'Key selected, but the gateway health check failed.',
    online ? 'success' : 'error',
  );
}

async function revokeGeneratedApiKeys(apiKeys) {
  if (!keyManagementEnabled || apiKeys.length === 0) return;
  setKeyGeneratorStatus('Revoking selected session key(s)…');
  try {
    const payload = await postKeyManagerRequest('/api/keys/revoke', { api_keys: apiKeys });
    const revoked = new Set(payload.revoked_api_keys);
    generatedApiKeys = generatedApiKeys.filter((entry) => !revoked.has(entry.api_key));
    saveGeneratedApiKeys();
    if (revoked.has(apiKeyInput.value)) {
      apiKeyInput.value = '';
      setSessionValue('omni-api-key', '');
    }
    if (revoked.has(byId('generatedKeyOutput').value)) {
      byId('generatedKeyOutput').value = '';
      byId('generatedKeyResult').hidden = true;
    }
    renderGeneratedApiKeys();
    setKeyGeneratorStatus(`Revoked ${payload.revoked_api_keys.length} key(s) from the local gateway.`, 'success');
  } catch (error) {
    setKeyGeneratorStatus(error.message, 'error');
  }
}

function updateLineNumbers() {
  const count = Math.max(1, editor.value.split('\n').length);
  const fragment = document.createDocumentFragment();
  for (let line = 1; line <= count; line += 1) {
    const number = document.createElement('span');
    number.textContent = String(line);
    fragment.append(number);
  }
  lineNumbers.replaceChildren(fragment);
  lineNumbers.scrollTop = editor.scrollTop;
  const chars = Array.from(editor.value).length;
  const bytes = new TextEncoder().encode(editor.value).length;
  byId('sourceStats').textContent =
    `${count} lines · ${chars.toLocaleString()} chars · ${bytes.toLocaleString()} UTF-8 bytes`;
}

function selectedGatewayUrl() {
  return gatewayUrlInput.value.trim();
}

function setConnectionState(state, message) {
  const indicator = byId('connectionState');
  indicator.className = `connection-state state-${state}`;
  byId('connectionLabel').textContent = message;
}

function updateGatewayConnection(online) {
  gatewayReachable = online;
  if (!online) {
    setConnectionState('offline', 'Gateway offline');
    return;
  }
  const hasApiKey = apiKeyInput.value.trim().length > 0;
  if (!hasApiKey) {
    byId('runNote').textContent = 'API key required before execution. No source is sent until a key is entered.';
  }
  setConnectionState(
    'online',
    hasApiKey ? 'Gateway online' : 'Gateway online · API key required',
  );
}

function setUsageLevel(elementId, percentage) {
  const level = Number.isFinite(percentage)
    ? Math.max(0, Math.min(5, Math.ceil(percentage / 20)))
    : 0;
  byId(elementId).dataset.level = String(level);
}

function renderLatencyTrend() {
  const container = byId('latencyTrend');
  container.replaceChildren();
  const records = history.slice(0, 8).reverse();
  if (records.length === 0) {
    container.setAttribute('aria-label', 'No execution latency history');
    return;
  }
  const values = records.map((record) => record.latencyMs);
  const minimum = Math.min(...values);
  const maximum = Math.max(...values);
  for (const record of records) {
    const bar = document.createElement('span');
    bar.className = 'metric-spark-bar';
    const normalized = maximum === minimum ? 0.5 : (record.latencyMs - minimum) / (maximum - minimum);
    bar.dataset.level = String(Math.max(1, Math.min(5, Math.round(normalized * 4) + 1)));
    bar.title = `${record.latencyMs.toFixed(1)} ms`;
    container.append(bar);
  }
  container.setAttribute('aria-label', `Round-trip latency for ${records.length} recent executions`);
}

function updateHealthMetrics(payload) {
  const metrics = payload && payload.metrics;
  if (!metrics) {
    byId('quotaMetric').textContent = 'No metrics';
    byId('quotaCaption').textContent = 'Gateway health response has no aggregate counters';
    byId('gatewayHealthTrend').textContent = 'Health counters unavailable';
    setUsageLevel('gatewayHealthFill', 0);
    return;
  }
  const requests = Number(metrics.requests_total || 0);
  const successes = Number(metrics.requests_succeeded_total || 0);
  const failures = Number(metrics.requests_failed_total || 0);
  const quotaBlocks = Number(metrics.quota_rejections_total || 0);
  const rateBlocks = Number(metrics.rate_limit_rejections_total || 0);
  const concurrencyBlocks = Number(metrics.concurrency_rejections_total || 0);
  const quorumIssues = Number(metrics.worker_quorum_mismatches_total || 0)
    + Number(metrics.worker_quorum_unavailable_total || 0);
  const blocked = quotaBlocks + rateBlocks + concurrencyBlocks + quorumIssues;
  byId('quotaMetric').textContent = `${requests.toLocaleString()} requests`;
  byId('quotaCaption').textContent =
    `${quotaBlocks} quota · ${rateBlocks} rate · ${quorumIssues} quorum issues`;
  const successRate = requests > 0 ? Math.round((successes / requests) * 100) : 0;
  byId('gatewayHealthTrend').textContent = requests > 0
    ? `${successRate}% success · ${failures} failed · ${blocked} blocked`
    : 'No requests yet · aggregate gateway counters';
  setUsageLevel('gatewayHealthFill', successRate);
}

async function checkGateway() {
  const url = selectedGatewayUrl();
  if (!url) {
    updateGatewayConnection(false);
    setConnectionState('offline', 'Gateway URL required');
    return false;
  }
  setConnectionState('checking', 'Checking gateway');
  try {
    const response = await fetch('/api/health', {
      headers: { 'X-Omni-Gateway-URL': url },
      cache: 'no-store',
    });
    const payload = await response.json();
    const online = response.ok && payload.status === 'ok';
    updateGatewayConnection(online);
    if (!online && payload.error) {
      byId('quotaMetric').textContent = 'Unavailable';
      byId('quotaCaption').textContent = payload.error.message || 'Gateway health request failed';
    } else {
      updateHealthMetrics(payload);
    }
    return online;
  } catch {
    updateGatewayConnection(false);
    byId('quotaMetric').textContent = 'Unavailable';
    byId('quotaCaption').textContent = 'Bridge could not read gateway health';
    return false;
  }
}

function formatMemory(bytes) {
  const value = Number(bytes);
  if (!Number.isFinite(value) || value < 0) return '—';
  if (value >= 1024 * 1024) return `${(value / (1024 * 1024)).toFixed(2)} MiB`;
  if (value >= 1024) return `${(value / 1024).toFixed(1)} KiB`;
  return `${value} B`;
}

function updateExecutionMetrics(record) {
  const succeeded = record.httpStatus >= 200 && record.httpStatus < 300;
  byId('executionMetric').textContent = succeeded
    ? 'Complete'
    : `Rejected · ${record.httpStatus}`;
  byId('executionCaption').textContent = record.payload
    && record.payload.error
    ? record.payload.error.code || 'Gateway error'
    : (succeeded ? 'Sandbox result received' : 'Gateway response');
  byId('latencyMetric').textContent = record.latencyMs.toFixed(1);

  const usage = record.payload && record.payload.usage;
  const cpuMs = usage ? Number(usage.cpu_ms) : Number.NaN;
  const memoryBytes = usage ? Number(usage.peak_memory_bytes) : Number.NaN;
  const requestedBudget = Number(record.timeoutMs);
  const cpuBudget = Number.isFinite(requestedBudget) && requestedBudget > 0
    ? Math.min(requestedBudget, cpuLimitMs)
    : cpuLimitMs;
  const cpu = Number.isFinite(cpuMs)
    ? `${cpuMs} / ${cpuBudget} ms`
    : `≤ ${cpuBudget} ms budget`;
  const memory = Number.isFinite(memoryBytes) ? formatMemory(memoryBytes) : '—';
  byId('cpuMetric').textContent = cpu;
  byId('memoryMetric').textContent = `Peak memory ${memory}`;
  if (Number.isFinite(cpuMs) && Number.isFinite(memoryBytes)) {
    const cpuPercent = Math.min(100, (cpuMs / cpuBudget) * 100);
    const memoryPercent = Math.min(100, (memoryBytes / memoryLimitBytes) * 100);
    byId('cpuUsagePercent').textContent = `${Math.round(cpuPercent)}%`;
    byId('memoryUsagePercent').textContent = `${Math.round(memoryPercent)}%`;
    setUsageLevel('cpuUsageFill', cpuPercent);
    setUsageLevel('memoryUsageFill', memoryPercent);
  } else {
    byId('cpuUsagePercent').textContent = '—';
    byId('memoryUsagePercent').textContent = '—';
    setUsageLevel('cpuUsageFill', 0);
    setUsageLevel('memoryUsageFill', 0);
  }

  const badge = byId('httpStatus');
  badge.textContent = `${record.httpStatus} · ${succeeded ? 'OK' : 'ERROR'}`;
  badge.className = `http-badge ${succeeded ? 'badge-ok' : 'badge-error'}`;
  const runtime = record.payload && record.payload.runtime;
  const descriptor = languageDescriptor(record.language);
  const runtimeBadge = byId('runtimeBadge');
  if (runtime && typeof runtime.name === 'string') {
    runtimeBadge.textContent = `${runtime.name} · ${runtime.runtime || runtime.language || 'runtime'}`;
    runtimeBadge.dataset.available = String(runtime.available !== false);
  } else if (descriptor) {
    runtimeBadge.textContent = `${descriptor.name} · ${descriptor.available ? descriptor.runtime : 'unavailable'}`;
    runtimeBadge.dataset.available = String(descriptor.available === true);
  } else {
    runtimeBadge.textContent = `Runtime · ${record.language || 'unknown'}`;
    runtimeBadge.dataset.available = 'false';
  }
}

function displayRecord(record) {
  displayedRecord = record;
  byId('outputEmpty').hidden = true;
  const output = byId('responseJson');
  output.hidden = false;
  output.querySelector('code').textContent = JSON.stringify(record.payload, null, 2);
  byId('copyResultButton').disabled = false;
  updateExecutionMetrics(record);
}

function showApiKeyRequired(language, timeoutMs) {
  displayRecord({
    httpStatus: 0,
    latencyMs: 0,
    timeoutMs,
    language,
    payload: {
      error: {
        code: 'api_key_required',
        message: 'No source was sent. Generate a local API key or paste a key, then select Use in Studio.',
      },
    },
  });
  byId('executionMetric').textContent = 'Not sent';
  byId('executionCaption').textContent = 'API key required';
  byId('latencyMetric').textContent = '—';
  byId('httpStatus').textContent = 'LOCAL · BLOCKED';
  byId('runNote').textContent = 'Generate a key or paste one above. No source was sent to the backend.';
  updateGatewayConnection(gatewayReachable);
}

async function copyResult() {
  if (!displayedRecord) return;
  const button = byId('copyResultButton');
  try {
    await copyText(JSON.stringify(displayedRecord.payload, null, 2));
    const label = button.textContent;
    button.textContent = 'Copied';
    window.setTimeout(() => {
      button.textContent = label;
    }, 1400);
  } catch (error) {
    byId('runNote').textContent = error.message;
  }
}

function renderHistory() {
  const list = byId('historyList');
  list.replaceChildren();
  if (history.length === 0) {
    const empty = document.createElement('p');
    empty.className = 'history-empty';
    empty.textContent = 'Your current session is ready for its first run.';
    list.append(empty);
    return;
  }
  for (const record of history) {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = `history-item${record.id === selectedHistoryId ? ' selected' : ''}`;
    const preset = presetLabel(record.preset);
    const descriptor = languageDescriptor(record.language);
    const languageName = descriptor ? descriptor.name : record.language;
    const time = formatHistoryTimestamp(record.createdAt);
    button.setAttribute(
      'aria-label',
      `Restore ${languageName} ${preset} execution, HTTP ${record.httpStatus}, ${record.latencyMs.toFixed(1)} milliseconds round trip, ${time}`,
    );

    const status = document.createElement('span');
    status.className = `history-status ${record.httpStatus >= 200 && record.httpStatus < 300 ? 'ok' : 'error'}`;
    status.textContent = String(record.httpStatus);

    const summary = document.createElement('span');
    summary.className = 'history-summary';
    const meta = document.createElement('span');
    meta.className = 'history-meta';
    const presetName = document.createElement('span');
    presetName.className = 'history-preset';
    presetName.textContent = `${languageName} · ${preset}`;
    const timestamp = document.createElement('span');
    timestamp.className = 'history-time';
    timestamp.textContent = time;
    const code = document.createElement('span');
    code.className = 'history-code';
    code.textContent = record.source.split('\n')[0].slice(0, 72) || '(empty source)';

    meta.append(presetName, timestamp);
    summary.append(meta, code);
    const latency = document.createElement('span');
    latency.className = 'history-latency';
    latency.textContent = `${record.latencyMs.toFixed(1)} ms`;

    button.append(status, summary, latency);
    button.addEventListener('click', () => {
      selectedHistoryId = record.id;
      editor.value = record.source;
      const restoredLanguage = runtimeLanguages.some((language) => language.id === record.language)
        ? record.language
        : 'omni';
      languageSelector.value = restoredLanguage;
      setSessionValue(languageStorageKey, restoredLanguage);
      updateLanguageStatus();
      renderPresets();
      byId('gasLimit').value = record.gasLimit;
      byId('timeoutMs').value = record.timeoutMsInput;
      byId('verification').value = record.verification;
      setActivePreset(record.preset);
      updateLineNumbers();
      displayRecord(record);
      renderHistory();
      byId('runNote').textContent = `Restored ${presetLabel(record.preset)} execution from session history.`;
    });
    list.append(button);
  }
}

function presetLabel(name) {
  const labels = {
    arithmetic: 'Arithmetic',
    gas: 'Gas limit',
    memory: 'Memory load',
    unicode: 'Unicode text',
    custom: 'Custom',
  };
  return labels[name] || 'Custom';
}

function formatHistoryTimestamp(timestamp) {
  const date = new Date(timestamp);
  if (!Number.isFinite(date.getTime())) return 'Unknown time';
  return date.toLocaleString([], {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  });
}

async function runExecution() {
  if (runButton.disabled) return;
  const source = editor.value;
  if (!source.trim()) {
    byId('runNote').textContent = 'Add a source expression before running.';
    editor.focus();
    return;
  }

  const url = selectedGatewayUrl();
  const apiKey = apiKeyInput.value.trim();
  const timeoutMs = Number(byId('timeoutMs').value) || 50;
  const language = selectedLanguage();
  if (!apiKey) {
    showApiKeyRequired(language, timeoutMs);
    return;
  }
  const gasText = byId('gasLimit').value.trim();
  const timeoutText = byId('timeoutMs').value.trim();
  const requestTimeoutMs = timeoutText ? Number(timeoutText) : 50;
  const request = { source, language };
  if (gasText) request.gas_limit = Number(gasText);
  if (timeoutText) request.timeout_ms = Number(timeoutText);
  if (byId('verification').value) request.verification = byId('verification').value;

  const headers = {
    'Content-Type': 'application/json',
    'X-Omni-Gateway-URL': url,
  };
  if (apiKey) headers.Authorization = `Bearer ${apiKey}`;

  runButton.disabled = true;
  byId('runNote').textContent = 'Request in progress · gateway policy remains authoritative.';
  byId('executionMetric').textContent = 'Running';
  byId('executionCaption').textContent = 'Waiting for sandbox response';
  const started = performance.now();
  let httpStatus = 503;
  let payload = {
    error: { code: 'gateway_unavailable', message: 'No response was received from the bridge' },
  };
  try {
    const response = await fetch('/api/execute', {
      method: 'POST',
      headers,
      body: JSON.stringify(request),
      cache: 'no-store',
    });
    httpStatus = response.status;
    payload = await response.json();
  } catch {
    updateGatewayConnection(false);
    payload = {
      error: {
        code: 'gateway_unavailable',
        message: 'The local bridge could not reach the gateway. Check the URL and gateway status.',
      },
    };
  } finally {
    runButton.disabled = false;
  }

  const record = {
    id: nextHistoryId++,
    source,
    language,
    httpStatus,
    payload,
    latencyMs: performance.now() - started,
    timeoutMs: requestTimeoutMs,
    timeoutMsInput: timeoutText,
    gasLimit: gasText,
    verification: byId('verification').value,
    preset: activePreset,
    createdAt: new Date().toISOString(),
  };
  history.unshift(record);
  if (history.length > historyLimit) history.length = historyLimit;
  saveExecutionHistory();
  selectedHistoryId = record.id;
  displayRecord(record);
  renderHistory();
  renderLatencyTrend();
  byId('runNote').textContent = httpStatus >= 200 && httpStatus < 300
    ? 'Execution completed under the configured gateway policy.'
    : 'Gateway returned a structured response; inspect details above.';
  if (httpStatus < 500) {
    updateGatewayConnection(true);
  }
  checkGateway();
}

function loadPreset(name) {
  const preset = (presets[selectedLanguage()] || {})[name];
  if (!preset) return;
  editor.value = preset.source;
  byId('gasLimit').value = preset.gas;
  byId('timeoutMs').value = preset.timeout;
  byId('verification').value = preset.verification;
  setActivePreset(name);
  updateLineNumbers();
}

function setActivePreset(name) {
  activePreset = (presets[selectedLanguage()] || {})[name] ? name : 'custom';
  document.querySelectorAll('.preset-button').forEach((button) => {
    button.classList.toggle('active', button.dataset.preset === activePreset);
  });
}

function renderPresets() {
  const languagePresets = presets[selectedLanguage()] || {};
  let visiblePresets = 0;
  document.querySelectorAll('.preset-button').forEach((button) => {
    const preset = languagePresets[button.dataset.preset];
    button.hidden = !preset;
    if (preset) {
      button.textContent = preset.label;
      visiblePresets += 1;
    }
    button.classList.toggle('active', Boolean(preset) && button.dataset.preset === activePreset);
  });
  byId('presetNote').hidden = visiblePresets > 0;
  if (!languagePresets[activePreset]) activePreset = 'custom';
}

function setupShortcutHint() {
  const isMac = /mac/i.test(navigator.platform || '');
  byId('shortcutModifier').textContent = isMac ? '⌘' : 'Ctrl';
  byId('shortcutHint').setAttribute('aria-label', `${isMac ? 'Command' : 'Control'} plus Enter runs the task`);
}

function setupTheme() {
  const saved = getSessionValue('omni-theme');
  document.body.dataset.theme = saved === 'dark' ? 'dark' : 'light';
  const updateLabel = () => {
    const dark = document.body.dataset.theme === 'dark';
    byId('themeToggle').textContent = dark ? 'Obsidian Slate' : 'Atelier Light';
    document.querySelector('meta[name="theme-color"]').content = dark ? '#20211f' : '#f6f5f2';
  };
  updateLabel();
  byId('themeToggle').addEventListener('click', () => {
    document.body.dataset.theme = document.body.dataset.theme === 'dark' ? 'light' : 'dark';
    setSessionValue('omni-theme', document.body.dataset.theme);
    updateLabel();
  });
}

async function initialize() {
  setupTheme();
  apiKeyInput.value = getSessionValue('omni-api-key');
  gatewayUrlInput.value = getPersistentValue(gatewayUrlStorageKey);
  try {
    const response = await fetch('/api/config', { cache: 'no-store' });
    const config = await response.json();
    configuredGatewayUrl = config.gateway_url || LOCAL_GATEWAY_URL;
    if (!gatewayUrlInput.value) {
      gatewayUrlInput.value = configuredGatewayUrl;
    }
    if (!apiKeyInput.value && config.api_key) {
      apiKeyInput.value = config.api_key;
      setSessionValue('omni-api-key', config.api_key);
    }
    setKeyManagementEnabled(config.key_management_enabled === true);
    if (config.key_management_enabled === true && config.managed_gateway_status === 'error') {
      setKeyGeneratorStatus(
        config.managed_gateway_error || 'The managed gateway did not start. Check the .dev gateway logs.',
        'error',
      );
    }
  } catch {
    if (!gatewayUrlInput.value) {
      gatewayUrlInput.value = LOCAL_GATEWAY_URL;
    }
    setKeyManagementEnabled(false);
  }
  updateGatewayMode();

  await loadLanguages();
  renderGeneratedApiKeys();
  if (presets[selectedLanguage()]) loadPreset('arithmetic');
  setupShortcutHint();
  editor.addEventListener('input', () => {
    setActivePreset('custom');
    updateLineNumbers();
  });
  editor.addEventListener('scroll', () => {
    lineNumbers.scrollTop = editor.scrollTop;
  });
  byId('gatewayUrl').addEventListener('change', () => {
    setPersistentValue(gatewayUrlStorageKey, selectedGatewayUrl());
    updateGatewayMode();
    checkGateway();
    loadLanguages();
  });
  byId('gatewayMode').addEventListener('change', (event) => {
    selectGatewayMode(event.target.value);
  });
  languageSelector.addEventListener('change', () => {
    setSessionValue(languageStorageKey, selectedLanguage());
    updateLanguageStatus();
    renderPresets();
    if (presets[selectedLanguage()]) loadPreset('arithmetic');
    else setActivePreset('custom');
  });
  byId('apiKey').addEventListener('change', () => {
    setSessionValue('omni-api-key', apiKeyInput.value);
    updateGatewayConnection(gatewayReachable);
  });
  byId('apiKey').addEventListener('input', () => {
    updateGatewayConnection(gatewayReachable);
  });
  byId('connectButton').addEventListener('click', checkGateway);
  byId('runButton').addEventListener('click', runExecution);
  byId('copyResultButton').addEventListener('click', copyResult);
  byId('generateKeyButton').addEventListener('click', generateApiKey);
  byId('copyGeneratedKey').addEventListener('click', () => {
    copyGeneratedKey(byId('generatedKeyOutput').value, byId('copyGeneratedKey'));
  });
  byId('clearGeneratedKeys').addEventListener('click', () => {
    revokeGeneratedApiKeys(generatedApiKeys.map((entry) => entry.api_key));
  });
  byId('clearHistory').addEventListener('click', () => {
    history.length = 0;
    selectedHistoryId = null;
    try {
      sessionStorage.removeItem(historyStorageKey);
    } catch {
      byId('runNote').textContent = 'Browser session storage is unavailable; history could not be cleared.';
    }
    renderHistory();
    renderLatencyTrend();
  });
  document.querySelectorAll('.preset-button').forEach((button) => {
    button.addEventListener('click', () => loadPreset(button.dataset.preset));
  });
  document.addEventListener('keydown', (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key === 'Enter' && !runButton.disabled) {
      event.preventDefault();
      runExecution();
    }
  });

  renderHistory();
  renderLatencyTrend();
  checkGateway();
  window.setInterval(checkGateway, 15_000);
}

initialize();
