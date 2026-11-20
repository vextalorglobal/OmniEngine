//! Managed REST API gateway for policy-bounded OmniEngine execution.

use fs2::FileExt;
use omni_cluster::wire::{ErrorCode, TaskStatus, ValueTag};
use omni_cluster::{ClusterClient, RemoteError, TaskReply};
use omni_core::DEFAULT_STEP_LIMIT;
use ring::digest::{SHA256, digest};
mod runners;
use runners::{
    ExecutionLimits, ExecutionRunner, LanguageDescriptor, RunnerError, RunnerOutput,
    RunnerRegistry, RunnerResolutionError,
};
use rustls::ServerConfig;
use rustls::pki_types::PrivateKeyDer;
use rustls_pemfile::{certs, private_key};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_OVERHEAD: usize = 4096;
const DEFAULT_STATE_PATH: &str = "omni-gateway-state.json";
const STATE_PATH_ENV: &str = "OMNI_GATEWAY_STATE";
const OMNI_LANGUAGE_ID: &str = "omni";
const OMNI_LANGUAGE_NAME: &str = "OmniEngine";
const OMNI_RUNTIME_NAME: &str = "omni-core";

#[derive(Debug, Clone, Deserialize)]
pub struct TierPolicy {
    pub api_requests_per_month: u64,
    pub cpu_ms_per_request: u64,
    pub sandbox_memory_mib: u64,
    pub max_concurrent_requests: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct TierPolicyFile {
    tiers: TierPolicyTiers,
    gateway: TierGatewayPolicy,
}

#[derive(Debug, Clone, Deserialize)]
struct TierPolicyTiers {
    free: TierPolicy,
}

#[derive(Debug, Clone, Deserialize)]
struct TierGatewayPolicy {
    rest_execute_endpoint: String,
    tls_required: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ExampleConfig {
    gateway: GatewayConfig,
    auth: AuthConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GatewayConfig {
    pub bind: String,
    pub tls_cert: PathBuf,
    pub tls_key: PathBuf,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    pub execute_endpoint: String,
    pub health_endpoint: String,
    pub rate_limit: RateLimitConfig,
    pub resource_limits: ResourceLimitConfig,
    pub sandbox: SandboxConfig,
    #[serde(default)]
    pub cluster: ClusterConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ClusterConfig {
    pub enabled: bool,
    pub worker_endpoints: Vec<String>,
    pub psk_source: String,
    pub tls_ca_file: Option<PathBuf>,
    pub tls_server_name: Option<String>,
    pub quorum: usize,
    pub retries: u32,
    pub timeout_ms: u64,
    pub fallback_to_local: bool,
}

impl Default for ClusterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            worker_endpoints: Vec::new(),
            psk_source: "env:OMNI_CLUSTER_PSK_HEX".to_string(),
            tls_ca_file: None,
            tls_server_name: None,
            quorum: 2,
            retries: 1,
            timeout_ms: 5_000,
            fallback_to_local: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RateLimitConfig {
    pub free_requests_per_month: u64,
    pub free_max_concurrent: usize,
    pub free_rate_window_seconds: u64,
    pub free_max_requests_per_window: u64,
    pub retry_after_on_limit: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResourceLimitConfig {
    pub cpu_ms_per_request: u64,
    pub sandbox_memory_mib: u64,
    pub max_source_bytes: usize,
    pub max_output_bytes: usize,
    pub request_timeout_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SandboxConfig {
    pub filesystem_reads: bool,
    pub filesystem_writes: bool,
    pub module_imports: bool,
    pub host_functions: bool,
    pub network_access: bool,
    pub deterministic_verification: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct AuthConfig {
    mode: String,
    token_header: String,
    token_secret_source: String,
    token_secret_required: bool,
}

#[derive(Debug, Clone)]
pub struct GatewaySettings {
    pub config: GatewayConfig,
    pub free_tier: TierPolicy,
    pub tls_required: bool,
    pub auth: AuthSettings,
}

#[derive(Debug, Clone)]
pub struct AuthSettings {
    pub mode: String,
    pub token_header: String,
    pub token_secret_source: String,
    pub token_secret_required: bool,
}

impl GatewaySettings {
    /// Load both policy documents and reject drift in their shared Free Tier
    /// limits or endpoint/TLS declarations.
    pub fn load(policy_path: &Path, config_path: &Path) -> Result<Self, String> {
        let policy_text = std::fs::read_to_string(policy_path)
            .map_err(|e| format!("cannot read {}: {e}", policy_path.display()))?;
        let config_text = std::fs::read_to_string(config_path)
            .map_err(|e| format!("cannot read {}: {e}", config_path.display()))?;
        let settings = Self::parse(&policy_text, &config_text)?;
        settings.with_environment_overrides(&std::env::vars().collect())
    }

    pub fn parse(policy_text: &str, config_text: &str) -> Result<Self, String> {
        let policy: TierPolicyFile =
            toml::from_str(policy_text).map_err(|e| format!("invalid tier policy: {e}"))?;
        let example: ExampleConfig =
            toml::from_str(config_text).map_err(|e| format!("invalid cloud config: {e}"))?;
        let config = example.gateway;
        validate_allowed_origins(&config.allowed_origins)?;
        if policy.gateway.rest_execute_endpoint != config.execute_endpoint {
            return Err("REST execute endpoint differs between policy and config".to_string());
        }
        let free = policy.tiers.free;
        if free.api_requests_per_month != config.rate_limit.free_requests_per_month
            || free.max_concurrent_requests != config.rate_limit.free_max_concurrent
            || free.cpu_ms_per_request != config.resource_limits.cpu_ms_per_request
            || free.sandbox_memory_mib != config.resource_limits.sandbox_memory_mib
        {
            return Err("Free Tier limits differ between policy and cloud config".to_string());
        }
        if config.resource_limits.request_timeout_ms > free.cpu_ms_per_request {
            return Err("request timeout exceeds the Free Tier CPU ceiling".to_string());
        }
        if config.sandbox.filesystem_reads
            || config.sandbox.filesystem_writes
            || config.sandbox.module_imports
            || config.sandbox.host_functions
            || config.sandbox.network_access
        {
            return Err("managed sandbox capabilities must remain disabled by default".to_string());
        }
        let auth = example.auth;
        if auth.mode != "bearer" || !auth.token_secret_required {
            return Err("gateway requires bearer auth and a configured secret source".to_string());
        }
        validate_cluster_config(&config.cluster)?;
        Ok(GatewaySettings {
            config,
            free_tier: free,
            tls_required: policy.gateway.tls_required,
            auth: AuthSettings {
                mode: auth.mode,
                token_header: auth.token_header,
                token_secret_source: auth.token_secret_source,
                token_secret_required: auth.token_secret_required,
            },
        })
    }

    fn with_environment_overrides(
        mut self,
        environment: &HashMap<String, String>,
    ) -> Result<Self, String> {
        if let Some(bind) = environment.get("OMNI_BIND_ADDR") {
            if bind.trim().is_empty() {
                return Err("OMNI_BIND_ADDR must not be empty".to_string());
            }
            self.config.bind = bind.trim().to_string();
        }
        if let Some(port) = environment.get("OMNI_PORT") {
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| "OMNI_PORT must be an integer between 1 and 65535".to_string())?;
            let bind = self.config.bind.trim();
            self.config.bind = if let Ok(mut address) = bind.parse::<std::net::SocketAddr>() {
                address.set_port(port);
                address.to_string()
            } else if let Ok(address) = bind.parse::<std::net::IpAddr>() {
                std::net::SocketAddr::new(address, port).to_string()
            } else if let Some((host, existing_port)) = bind.rsplit_once(':') {
                if !host.is_empty() && existing_port.parse::<u16>().is_ok() {
                    format!("{host}:{port}")
                } else {
                    format!("{bind}:{port}")
                }
            } else {
                format!("{bind}:{port}")
            };
        }
        if let Some(origins) = environment.get("OMNI_ALLOWED_ORIGINS") {
            self.config.allowed_origins = origins
                .split(',')
                .map(str::trim)
                .filter(|origin| !origin.is_empty())
                .map(str::to_string)
                .collect();
        }
        validate_allowed_origins(&self.config.allowed_origins)?;
        if let Some(keys) = environment.get("OMNI_API_KEYS") {
            if keys.trim().is_empty() {
                return Err("OMNI_API_KEYS must not be empty when set".to_string());
            }
            self.auth.token_secret_source = "env:OMNI_API_KEYS".to_string();
        }
        if let Some(mode) = environment.get("OMNI_TLS_MODE") {
            match mode.trim().to_ascii_lowercase().as_str() {
                "tls" => self.tls_required = true,
                "plain" => self.tls_required = false,
                _ => return Err("OMNI_TLS_MODE must be either 'tls' or 'plain'".to_string()),
            }
        }
        if self.tls_required
            && (self.config.tls_cert.as_os_str().is_empty()
                || self.config.tls_key.as_os_str().is_empty())
        {
            return Err("TLS mode requires nonempty certificate and private-key paths".to_string());
        }
        Ok(self)
    }
}

fn validate_allowed_origins(origins: &[String]) -> Result<(), String> {
    for origin in origins {
        let Some((scheme, authority)) = origin.split_once("://") else {
            return Err(format!(
                "invalid allowed origin '{origin}': expected an http(s) origin"
            ));
        };
        if !matches!(scheme, "http" | "https")
            || authority.is_empty()
            || !authority.chars().all(|character| {
                character.is_ascii_alphanumeric()
                    || matches!(character, '.' | '-' | ':' | '[' | ']')
            })
            || origin.trim() != origin
        {
            return Err(format!(
                "invalid allowed origin '{origin}': expected an exact http(s) origin without path, credentials, or wildcard"
            ));
        }
    }
    Ok(())
}

fn validate_cluster_config(config: &ClusterConfig) -> Result<(), String> {
    if !config.enabled {
        return Ok(());
    }
    if config.worker_endpoints.is_empty() {
        return Err(
            "cluster dispatch is enabled but no worker endpoints are configured".to_string(),
        );
    }
    if config.quorum == 0 || config.quorum > config.worker_endpoints.len() {
        return Err("cluster quorum must be between 1 and the worker endpoint count".to_string());
    }
    if config.timeout_ms == 0 {
        return Err("cluster worker timeout_ms must be greater than zero".to_string());
    }
    if !config
        .psk_source
        .strip_prefix("env:")
        .is_some_and(|name| !name.is_empty())
    {
        return Err("cluster PSK source must use env:VARIABLE".to_string());
    }
    if config
        .tls_ca_file
        .as_ref()
        .is_none_or(|path| path.as_os_str().is_empty())
        || config
            .tls_server_name
            .as_ref()
            .is_none_or(|name| name.trim().is_empty())
    {
        return Err("cluster dispatch requires a TLS CA file and server name".to_string());
    }
    let mut endpoints = HashMap::new();
    if config
        .worker_endpoints
        .iter()
        .any(|endpoint| endpoint.trim().is_empty() || endpoints.insert(endpoint, ()).is_some())
    {
        return Err("cluster worker endpoints must be nonempty and unique".to_string());
    }
    Ok(())
}

fn configured_cluster_client(config: &ClusterConfig) -> Result<Option<ClusterClient>, String> {
    validate_cluster_config(config)?;
    if !config.enabled {
        return Ok(None);
    }
    let variable = config
        .psk_source
        .strip_prefix("env:")
        .expect("validated cluster PSK source");
    let encoded = std::env::var(variable)
        .map_err(|_| format!("cluster PSK environment variable {variable} is not set"))?;
    let psk = parse_cluster_psk(&encoded)?;
    let ca_path = config
        .tls_ca_file
        .as_ref()
        .expect("validated cluster CA path");
    let ca_pem = fs::read(ca_path)
        .map_err(|error| format!("cannot read cluster TLS CA {}: {error}", ca_path.display()))?;
    let server_name = config
        .tls_server_name
        .as_deref()
        .expect("validated cluster TLS server name");
    Ok(Some(
        ClusterClient::new(config.worker_endpoints.clone(), psk)
            .with_timeout(Duration::from_millis(config.timeout_ms))
            .with_tls(&ca_pem, server_name),
    ))
}

fn parse_cluster_psk(encoded: &str) -> Result<[u8; 32], String> {
    if encoded.len() != 64 || !encoded.is_ascii() {
        return Err("cluster PSK must be exactly 64 hexadecimal characters".to_string());
    }
    let mut psk = [0; 32];
    for (index, chunk) in encoded.as_bytes().chunks_exact(2).enumerate() {
        let pair = std::str::from_utf8(chunk)
            .map_err(|_| "cluster PSK must be hexadecimal".to_string())?;
        psk[index] = u8::from_str_radix(pair, 16)
            .map_err(|_| "cluster PSK must be hexadecimal".to_string())?;
    }
    Ok(psk)
}

#[derive(Debug, Clone)]
pub struct ApiKey {
    token_digest: Vec<u8>,
    pub tenant_id: String,
    pub limits: TierPolicy,
}

impl ApiKey {
    /// Tokens are accepted at provisioning time but only their SHA-256 digest
    /// is retained by the gateway.
    pub fn new(
        token: &str,
        tenant_id: impl Into<String>,
        limits: TierPolicy,
    ) -> Result<Self, String> {
        if token.len() < 24 || token.len() > 512 {
            return Err("API key must contain between 24 and 512 bytes".to_string());
        }
        Ok(Self {
            token_digest: digest(&SHA256, token.as_bytes()).as_ref().to_vec(),
            tenant_id: tenant_id.into(),
            limits,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Compile,
    Runtime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub retry_after_seconds: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GatewayMetricsSnapshot {
    pub requests_total: u64,
    pub requests_succeeded_total: u64,
    pub requests_failed_total: u64,
    pub executions_succeeded_total: u64,
    pub quota_rejections_total: u64,
    pub rate_limit_rejections_total: u64,
    pub concurrency_rejections_total: u64,
    pub worker_dispatches_total: u64,
    pub worker_quorum_successes_total: u64,
    pub worker_quorum_mismatches_total: u64,
    pub worker_quorum_unavailable_total: u64,
}

#[derive(Default)]
struct GatewayMetrics {
    requests_total: AtomicU64,
    requests_succeeded_total: AtomicU64,
    requests_failed_total: AtomicU64,
    executions_succeeded_total: AtomicU64,
    quota_rejections_total: AtomicU64,
    rate_limit_rejections_total: AtomicU64,
    concurrency_rejections_total: AtomicU64,
    worker_dispatches_total: AtomicU64,
    worker_quorum_successes_total: AtomicU64,
    worker_quorum_mismatches_total: AtomicU64,
    worker_quorum_unavailable_total: AtomicU64,
}

impl GatewayMetrics {
    fn snapshot(&self) -> GatewayMetricsSnapshot {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        GatewayMetricsSnapshot {
            requests_total: load(&self.requests_total),
            requests_succeeded_total: load(&self.requests_succeeded_total),
            requests_failed_total: load(&self.requests_failed_total),
            executions_succeeded_total: load(&self.executions_succeeded_total),
            quota_rejections_total: load(&self.quota_rejections_total),
            rate_limit_rejections_total: load(&self.rate_limit_rejections_total),
            concurrency_rejections_total: load(&self.concurrency_rejections_total),
            worker_dispatches_total: load(&self.worker_dispatches_total),
            worker_quorum_successes_total: load(&self.worker_quorum_successes_total),
            worker_quorum_mismatches_total: load(&self.worker_quorum_mismatches_total),
            worker_quorum_unavailable_total: load(&self.worker_quorum_unavailable_total),
        }
    }
}

impl HttpResponse {
    fn error(status: u16, code: &str, message: &str, retry_after_seconds: Option<u64>) -> Self {
        Self {
            status,
            body: json!({"error":{"code":code,"message":message}}).to_string(),
            retry_after_seconds,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExecuteRequest {
    source: String,
    language: Option<String>,
    gas_limit: Option<u64>,
    timeout_ms: Option<u64>,
    verification: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedKeyUsage {
    month: i64,
    monthly_requests: u64,
    window_started_unix_ms: u64,
    window_requests: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UsageSnapshot {
    version: u32,
    keys: HashMap<String, PersistedKeyUsage>,
}

#[derive(Debug, Clone, Default)]
struct UsageState {
    keys: HashMap<String, PersistedKeyUsage>,
    active: HashMap<String, usize>,
}

struct KeyState {
    digest: Vec<u8>,
    digest_hex: String,
    tenant_id: String,
    limits: TierPolicy,
}

struct Permit {
    state: Arc<Mutex<UsageState>>,
    key_id: String,
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(active) = state.active.get_mut(&self.key_id) {
                *active = active.saturating_sub(1);
                if *active == 0 {
                    state.active.remove(&self.key_id);
                }
            }
        }
    }
}

pub struct Gateway {
    settings: GatewaySettings,
    keys: Vec<Arc<KeyState>>,
    cluster: Option<ClusterClient>,
    state_path: PathBuf,
    usage: Arc<Mutex<UsageState>>,
    next_request: AtomicU64,
    metrics: GatewayMetrics,
    runners: RunnerRegistry,
}

impl Gateway {
    pub fn new(settings: GatewaySettings, api_keys: Vec<ApiKey>) -> Result<Self, String> {
        Self::new_with_state_path(settings, api_keys, default_state_path())
    }

    pub fn new_with_state_path(
        settings: GatewaySettings,
        api_keys: Vec<ApiKey>,
        state_path: impl Into<PathBuf>,
    ) -> Result<Self, String> {
        let cluster = configured_cluster_client(&settings.config.cluster)?;
        Self::new_with_runtime_cluster(settings, api_keys, state_path, cluster)
    }

    fn new_with_runtime_cluster(
        settings: GatewaySettings,
        api_keys: Vec<ApiKey>,
        state_path: impl Into<PathBuf>,
        cluster: Option<ClusterClient>,
    ) -> Result<Self, String> {
        if api_keys.is_empty() {
            return Err("at least one API key must be provisioned".to_string());
        }
        validate_cluster_config(&settings.config.cluster)?;
        if settings.config.cluster.enabled != cluster.is_some() {
            return Err(
                "cluster runtime does not match the enabled gateway configuration".to_string(),
            );
        }
        let state_path = state_path.into();
        let mut usage = load_usage_state(&state_path)?;
        let month = current_month();
        let now = current_unix_millis();
        let mut seen = HashMap::new();
        let keys = api_keys
            .into_iter()
            .map(|key| {
                let digest_hex = hex_digest(&key.token_digest);
                if seen.insert(digest_hex.clone(), ()).is_some() {
                    return Err("duplicate API key provisioned".to_string());
                }
                usage
                    .keys
                    .entry(digest_hex.clone())
                    .or_insert(PersistedKeyUsage {
                        month,
                        monthly_requests: 0,
                        window_started_unix_ms: now,
                        window_requests: 0,
                    });
                Ok(Arc::new(KeyState {
                    digest: key.token_digest,
                    digest_hex,
                    tenant_id: key.tenant_id,
                    limits: key.limits,
                }))
            })
            .collect::<Result<Vec<_>, String>>()?;
        Ok(Self {
            settings,
            keys,
            cluster,
            state_path,
            usage: Arc::new(Mutex::new(usage)),
            next_request: AtomicU64::new(1),
            metrics: GatewayMetrics::default(),
            runners: RunnerRegistry::default(),
        })
    }

    pub fn settings(&self) -> &GatewaySettings {
        &self.settings
    }

    pub fn metrics(&self) -> GatewayMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub fn handle(
        &self,
        method: &str,
        path: &str,
        headers: &HashMap<String, String>,
        body: &[u8],
    ) -> HttpResponse {
        if path == self.settings.config.health_endpoint && method == "GET" {
            return self.health_response();
        }
        if path == "/v1/languages" && method == "GET" {
            return self.languages_response();
        }
        self.metrics.requests_total.fetch_add(1, Ordering::Relaxed);
        let is_execute = path == self.settings.config.execute_endpoint && method == "POST";
        let response = self.handle_execute(method, path, headers, body);
        if response.status < 400 {
            self.metrics
                .requests_succeeded_total
                .fetch_add(1, Ordering::Relaxed);
            if is_execute && response.status == 200 {
                self.metrics
                    .executions_succeeded_total
                    .fetch_add(1, Ordering::Relaxed);
            }
        } else {
            self.metrics
                .requests_failed_total
                .fetch_add(1, Ordering::Relaxed);
        }
        response
    }

    fn health_response(&self) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: json!({
                "status":"ok",
                "metrics":self.metrics.snapshot(),
                "cluster":{
                    "enabled":self.settings.config.cluster.enabled,
                    "configured_workers":self.settings.config.cluster.worker_endpoints.len(),
                    "quorum":self.settings.config.cluster.quorum
                }
            })
            .to_string(),
            retry_after_seconds: None,
        }
    }

    fn languages_response(&self) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: json!({
                "languages":self.runners.languages(),
                "limits":{
                    "cpu_ms_per_request":self.settings.config.resource_limits.cpu_ms_per_request,
                    "request_timeout_ms":self.settings.config.resource_limits.request_timeout_ms,
                    "sandbox_memory_mib":self.settings.config.resource_limits.sandbox_memory_mib,
                    "max_source_bytes":self.settings.config.resource_limits.max_source_bytes,
                    "max_output_bytes":self.settings.config.resource_limits.max_output_bytes
                },
                "sandbox":{
                    "filesystem_reads":self.settings.config.sandbox.filesystem_reads,
                    "filesystem_writes":self.settings.config.sandbox.filesystem_writes,
                    "module_imports":self.settings.config.sandbox.module_imports,
                    "host_functions":self.settings.config.sandbox.host_functions,
                    "network_access":self.settings.config.sandbox.network_access,
                    "deterministic_verification":self.settings.config.sandbox.deterministic_verification
                }
            })
            .to_string(),
            retry_after_seconds: None,
        }
    }

    fn handle_execute(
        &self,
        method: &str,
        path: &str,
        headers: &HashMap<String, String>,
        body: &[u8],
    ) -> HttpResponse {
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        if path == self.settings.config.health_endpoint {
            return HttpResponse::error(405, "method_not_allowed", "GET is required", None);
        }
        if method != "POST" {
            return HttpResponse::error(405, "method_not_allowed", "POST is required", None);
        }
        if path != self.settings.config.execute_endpoint {
            return HttpResponse::error(404, "not_found", "route not found", None);
        }
        let Some(token) = bearer_token(headers, &self.settings.auth.token_header) else {
            return HttpResponse::error(401, "unauthorized", "missing or invalid API key", None);
        };
        let token_digest = digest(&SHA256, token.as_bytes());
        let Some(key) = self
            .keys
            .iter()
            .find(|key| constant_time_eq(&key.digest, token_digest.as_ref()))
            .cloned()
        else {
            return HttpResponse::error(401, "unauthorized", "missing or invalid API key", None);
        };
        let content_type = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
            .map(|(_, value)| value.as_str())
            .unwrap_or("");
        if !content_type
            .split(';')
            .next()
            .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"))
        {
            return HttpResponse::error(
                415,
                "unsupported_media_type",
                "Content-Type must be application/json",
                None,
            );
        }
        if body.len() > self.settings.config.resource_limits.max_source_bytes + MAX_BODY_OVERHEAD {
            return HttpResponse::error(
                413,
                "payload_too_large",
                "request body exceeds configured limit",
                None,
            );
        }
        let request: ExecuteRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(error) => {
                return HttpResponse::error(400, "invalid_json", &error.to_string(), None);
            }
        };
        if request.source.len() > self.settings.config.resource_limits.max_source_bytes {
            return HttpResponse::error(
                413,
                "source_too_large",
                "source exceeds configured limit",
                None,
            );
        }
        let requested_language = request.language.as_deref().unwrap_or("omni");
        let runner = match self.runners.resolve(requested_language) {
            Ok(runner) => runner,
            Err(RunnerResolutionError::Unavailable(descriptor)) => {
                return runtime_unavailable_response(request_id, descriptor);
            }
            Err(RunnerResolutionError::Unsupported(language)) => {
                return HttpResponse::error(
                    422,
                    "unsupported_language",
                    &format!("unsupported language '{language}'"),
                    None,
                );
            }
        };
        let descriptor = runner.descriptor();
        let deterministic = match request.verification.as_deref() {
            Some("deterministic") => true,
            Some(_) => {
                return HttpResponse::error(
                    400,
                    "invalid_request",
                    "unsupported verification mode",
                    None,
                );
            }
            None => false,
        };
        if deterministic && !runner.supports_deterministic_verification() {
            return runtime_error_response(
                request_id,
                descriptor,
                501,
                "verification_unavailable",
                "deterministic verification is not supported by this language runtime",
            );
        }
        if deterministic
            && (!self.settings.config.sandbox.deterministic_verification || self.cluster.is_none())
        {
            return HttpResponse::error(
                501,
                "verification_unavailable",
                "deterministic verification requires an enabled worker quorum",
                None,
            );
        }
        if deterministic && self.settings.config.cluster.quorum < 2 {
            return HttpResponse::error(
                400,
                "invalid_verification_quorum",
                "deterministic verification requires a quorum of at least two workers",
                None,
            );
        }
        let cpu_limit = key
            .limits
            .cpu_ms_per_request
            .min(self.settings.config.resource_limits.cpu_ms_per_request);
        let timeout = request
            .timeout_ms
            .unwrap_or(self.settings.config.resource_limits.request_timeout_ms);
        if timeout == 0 || timeout > cpu_limit {
            return HttpResponse::error(
                400,
                "resource_limit_exceeded",
                "timeout_ms exceeds the API key CPU limit",
                None,
            );
        }
        let max_steps = request
            .gas_limit
            .unwrap_or(DEFAULT_STEP_LIMIT)
            .min(DEFAULT_STEP_LIMIT);
        if max_steps == 0 {
            return HttpResponse::error(
                400,
                "invalid_request",
                "gas_limit must be greater than zero",
                None,
            );
        }
        if self.cluster.is_some() && !runner.supports_cluster_dispatch() {
            return runtime_error_response(
                request_id,
                descriptor,
                503,
                "language_dispatch_unavailable",
                "the configured worker protocol does not support this language runtime",
            );
        }
        let permit = match self.admit(&key) {
            Ok(permit) => permit,
            Err(AdmissionError::Quota) => {
                self.metrics
                    .quota_rejections_total
                    .fetch_add(1, Ordering::Relaxed);
                return HttpResponse::error(
                    429,
                    "monthly_quota_exceeded",
                    "monthly API request quota exceeded",
                    None,
                );
            }
            Err(AdmissionError::Rate) => {
                self.metrics
                    .rate_limit_rejections_total
                    .fetch_add(1, Ordering::Relaxed);
                let retry = self
                    .settings
                    .config
                    .rate_limit
                    .retry_after_on_limit
                    .then_some(
                        self.settings
                            .config
                            .rate_limit
                            .free_rate_window_seconds
                            .max(1),
                    );
                return HttpResponse::error(
                    429,
                    "rate_limited",
                    "request rate limit exceeded",
                    retry,
                );
            }
            Err(AdmissionError::Concurrency) => {
                self.metrics
                    .concurrency_rejections_total
                    .fetch_add(1, Ordering::Relaxed);
                return HttpResponse::error(
                    429,
                    "concurrency_limit_exceeded",
                    "maximum concurrent requests reached",
                    Some(1),
                );
            }
            Err(AdmissionError::Persistence(error)) => {
                eprintln!("omni-gateway usage state could not be persisted: {error}");
                return HttpResponse::error(
                    503,
                    "usage_state_unavailable",
                    "usage state could not be persisted",
                    Some(1),
                );
            }
            Err(AdmissionError::Internal) => {
                return HttpResponse::error(
                    503,
                    "gateway_unavailable",
                    "usage state is unavailable",
                    Some(1),
                );
            }
        };
        let _permit = permit;
        let max_memory = key
            .limits
            .sandbox_memory_mib
            .min(self.settings.config.resource_limits.sandbox_memory_mib);
        if let Some(cluster) = &self.cluster {
            return self.execute_cluster(
                request_id,
                &key,
                cluster,
                &request.source,
                max_steps,
                timeout,
                max_memory,
                deterministic,
            );
        }
        self.execute_local(
            request_id,
            &key,
            runner,
            &request.source,
            max_steps,
            timeout,
            max_memory,
        )
    }

    fn execute_local(
        &self,
        request_id: u64,
        key: &KeyState,
        runner: &dyn ExecutionRunner,
        source: &str,
        max_steps: u64,
        cpu_ms: u64,
        memory_mib: u64,
    ) -> HttpResponse {
        let memory_bytes =
            usize::try_from(memory_mib.saturating_mul(1024 * 1024)).unwrap_or(usize::MAX);
        let limits = ExecutionLimits {
            gas_limit: max_steps,
            cpu_limit: Duration::from_millis(cpu_ms),
            memory_limit_bytes: memory_bytes,
            max_output_bytes: self.settings.config.resource_limits.max_output_bytes,
        };
        match runner.execute(source, limits) {
            Ok(output) => self.success(request_id, key, runner.descriptor(), output),
            Err(error) => runner_error_response(request_id, runner.descriptor(), error),
        }
    }

    fn execute_cluster(
        &self,
        request_id: u64,
        key: &KeyState,
        cluster: &ClusterClient,
        source: &str,
        gas: u64,
        cpu_ms: u64,
        memory_mib: u64,
        deterministic: bool,
    ) -> HttpResponse {
        let fallback = self.settings.config.cluster.fallback_to_local && !deterministic;
        self.metrics
            .worker_dispatches_total
            .fetch_add(1, Ordering::Relaxed);
        if cluster.health_check() == 0 {
            if deterministic {
                self.metrics
                    .worker_quorum_unavailable_total
                    .fetch_add(1, Ordering::Relaxed);
            }
            if fallback {
                return self.execute_local(
                    request_id,
                    key,
                    self.runners.omni(),
                    source,
                    gas,
                    cpu_ms,
                    memory_mib,
                );
            }
            return HttpResponse::error(
                503,
                "workers_unavailable",
                "no configured worker passed the health check",
                Some(1),
            );
        }
        let started = Instant::now();
        let result = if deterministic {
            cluster.run_remote_verified_with_limits(
                source,
                gas,
                self.settings.config.cluster.quorum,
                cpu_ms,
                memory_mib,
            )
        } else {
            cluster.run_remote_with_limits(
                source,
                gas,
                cpu_ms,
                memory_mib,
                self.settings.config.cluster.retries,
            )
        };
        let elapsed = started.elapsed();
        if deterministic {
            match &result {
                Ok(_) => {
                    self.metrics
                        .worker_quorum_successes_total
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(RemoteError::Verification) => {
                    self.metrics
                        .worker_quorum_mismatches_total
                        .fetch_add(1, Ordering::Relaxed);
                }
                Err(_) => {
                    self.metrics
                        .worker_quorum_unavailable_total
                        .fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        match result {
            Ok(reply) => self.remote_result(request_id, key, reply, elapsed),
            Err(error) if fallback && is_transient_worker_error(&error) => self.execute_local(
                request_id,
                key,
                self.runners.omni(),
                source,
                gas,
                cpu_ms,
                memory_mib,
            ),
            Err(error) => remote_error_response(error),
        }
    }

    fn remote_result(
        &self,
        request_id: u64,
        key: &KeyState,
        reply: TaskReply,
        dispatch_elapsed: Duration,
    ) -> HttpResponse {
        if reply.text.len() > self.settings.config.resource_limits.max_output_bytes {
            return HttpResponse::error(
                413,
                "output_too_large",
                "worker result exceeds configured output limit",
                None,
            );
        }
        if reply.status != TaskStatus::Ok {
            let (status, code) = match reply.status {
                TaskStatus::Compile => (400, "compile_error"),
                TaskStatus::Runtime => (422, "runtime_error"),
                TaskStatus::Ok => unreachable!(),
            };
            return HttpResponse {
                status,
                body: json!({
                    "request_id":format!("req_{request_id}"),
                    "tenant_id":key.tenant_id,
                    "status":"error",
                    "runtime":{
                        "language":OMNI_LANGUAGE_ID,
                        "name":OMNI_LANGUAGE_NAME,
                        "runtime":OMNI_RUNTIME_NAME,
                        "available":true
                    },
                    "error":{"code":code,"message":reply.text}
                })
                .to_string(),
                retry_after_seconds: None,
            };
        }
        let mut value = json!({
            "tag": value_tag_name(reply.kind),
            "text": reply.text
        });
        if reply.kind == ValueTag::Number && reply.num.is_finite() {
            value["number"] = json!(reply.num);
        } else if reply.kind == ValueTag::Bool {
            value["boolean"] = json!(reply.text == "true");
        }
        HttpResponse {
            status: 200,
            body: json!({
                "request_id":format!("req_{request_id}"),
                "tenant_id":key.tenant_id,
                "status":"ok",
                "runtime":{
                    "language":OMNI_LANGUAGE_ID,
                    "name":OMNI_LANGUAGE_NAME,
                    "runtime":OMNI_RUNTIME_NAME,
                    "available":true
                },
                "value":value,
                "usage":{
                    "cpu_ms":null,
                    "peak_memory_bytes":null,
                    "worker_dispatch_ms":dispatch_elapsed.as_millis()
                }
            })
            .to_string(),
            retry_after_seconds: None,
        }
    }

    fn success(
        &self,
        request_id: u64,
        key: &KeyState,
        descriptor: LanguageDescriptor,
        output: RunnerOutput,
    ) -> HttpResponse {
        let mut value_json = json!({
            "tag":output.value.tag,
            "text":output.value.text
        });
        if let Some(number) = output.value.number {
            value_json["number"] = json!(number);
        } else if let Some(boolean) = output.value.boolean {
            value_json["boolean"] = json!(boolean);
        }
        HttpResponse {
            status: 200,
            body: json!({
                "request_id":format!("req_{request_id}"),
                "tenant_id":key.tenant_id,
                "status":"ok",
                "runtime":{
                    "language":descriptor.id,
                    "name":descriptor.name,
                    "runtime":descriptor.runtime,
                    "available":descriptor.available
                },
                "value":value_json,
                "usage":{
                    "cpu_ms":output.cpu_ms,
                    "peak_memory_bytes":output.memory_bytes
                }
            })
            .to_string(),
            retry_after_seconds: None,
        }
    }

    fn admit(&self, key: &Arc<KeyState>) -> Result<Permit, AdmissionError> {
        let mut usage = self.usage.lock().map_err(|_| AdmissionError::Internal)?;
        let _state_lock = lock_usage_state_file(&self.state_path)
            .map_err(|error| AdmissionError::Persistence(error.to_string()))?;
        let mut next =
            load_usage_state_unlocked(&self.state_path).map_err(AdmissionError::Persistence)?;
        next.active = usage.active.clone();
        let now = current_unix_millis();
        let month = current_month();
        let persistent = next
            .keys
            .entry(key.digest_hex.clone())
            .or_insert(PersistedKeyUsage {
                month,
                monthly_requests: 0,
                window_started_unix_ms: now,
                window_requests: 0,
            });
        if persistent.month != month {
            persistent.month = month;
            persistent.monthly_requests = 0;
        }
        if persistent.monthly_requests >= key.limits.api_requests_per_month {
            return Err(AdmissionError::Quota);
        }
        let window_ms = self
            .settings
            .config
            .rate_limit
            .free_rate_window_seconds
            .saturating_mul(1000);
        if window_ms == 0 || now.saturating_sub(persistent.window_started_unix_ms) >= window_ms {
            persistent.window_started_unix_ms = now;
            persistent.window_requests = 0;
        }
        let rate_limit = self.settings.config.rate_limit.free_max_requests_per_window;
        if rate_limit > 0 && persistent.window_requests >= rate_limit {
            return Err(AdmissionError::Rate);
        }
        let max_concurrent = key
            .limits
            .max_concurrent_requests
            .min(self.settings.config.rate_limit.free_max_concurrent);
        let active = next.active.get(&key.digest_hex).copied().unwrap_or(0);
        if max_concurrent > 0 && active >= max_concurrent {
            return Err(AdmissionError::Concurrency);
        }
        persistent.monthly_requests += 1;
        persistent.window_requests += 1;
        next.active.insert(key.digest_hex.clone(), active + 1);
        persist_usage_state(&self.state_path, &next)
            .map_err(|error| AdmissionError::Persistence(error.to_string()))?;
        *usage = next;
        Ok(Permit {
            state: Arc::clone(&self.usage),
            key_id: key.digest_hex.clone(),
        })
    }

    /// Serve HTTP/1.1 directly with TLS, or plain HTTP behind a trusted TLS
    /// reverse proxy when OMNI_TLS_MODE=plain is explicitly configured.
    pub fn serve(self: Arc<Self>) -> io::Result<()> {
        let tls = if self.settings.tls_required {
            Some(Arc::new(
                load_tls_config(
                    &self.settings.config.tls_cert,
                    &self.settings.config.tls_key,
                )
                .map_err(|error| {
                    io::Error::new(
                        error.kind(),
                        format!(
                            "{error}. For local development, run .\\scripts\\dev-up.ps1 to generate valid PEM certificate and key files."
                        ),
                    )
                })?,
            ))
        } else {
            None
        };
        let listener = TcpListener::bind(&self.settings.config.bind).map_err(|error| {
            let action = if error.kind() == io::ErrorKind::AddrInUse {
                "the port is already in use; stop the existing listener or free this address"
            } else {
                "check that this address is valid and available"
            };
            io::Error::new(
                error.kind(),
                format!(
                    "cannot bind gateway on {}: {action} ({error})",
                    self.settings.config.bind
                ),
            )
        })?;
        if let Some(tls) = tls {
            eprintln!(
                "omni-gateway: serving HTTPS on {}",
                self.settings.config.bind
            );
            serve_tls_listener(listener, tls, self, None)
        } else {
            eprintln!(
                "omni-gateway: serving plain HTTP on {} (use only behind a trusted TLS reverse proxy)",
                self.settings.config.bind
            );
            serve_plain_listener(listener, self, None)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AdmissionError {
    Quota,
    Rate,
    Concurrency,
    Persistence(String),
    Internal,
}

fn is_transient_worker_error(error: &RemoteError) -> bool {
    matches!(
        error,
        RemoteError::Io(_)
            | RemoteError::Disconnected
            | RemoteError::ExhaustedRetries
            | RemoteError::NoHealthyNode
            | RemoteError::Refused(ErrorCode::Busy)
    )
}

fn remote_error_response(error: RemoteError) -> HttpResponse {
    if let RemoteError::Io(detail) = &error {
        eprintln!("omni-gateway worker transport error: {detail}");
    }
    let (status, code, message) = match error {
        RemoteError::Verification => (
            409,
            "deterministic_mismatch",
            "worker results disagreed; no result was accepted",
        ),
        RemoteError::QuorumUnavailable => (
            503,
            "verification_quorum_unavailable",
            "not enough healthy workers returned results for the configured quorum",
        ),
        RemoteError::InvalidQuorum => (
            500,
            "invalid_worker_quorum",
            "configured worker quorum is invalid",
        ),
        RemoteError::Refused(ErrorCode::CpuLimit) => (
            429,
            "cpu_limit_exceeded",
            "worker rejected execution after reaching its CPU limit",
        ),
        RemoteError::Refused(ErrorCode::MemoryLimit) => (
            429,
            "memory_limit_exceeded",
            "worker rejected execution after reaching its memory limit",
        ),
        RemoteError::Refused(ErrorCode::TooLong) => {
            (413, "source_too_large", "worker rejected the source size")
        }
        RemoteError::Refused(ErrorCode::GasOverCap) => (
            429,
            "gas_limit_exceeded",
            "worker gas policy is stricter than the requested task budget",
        ),
        RemoteError::Refused(ErrorCode::Busy) => {
            (503, "worker_busy", "all attempted workers are at capacity")
        }
        RemoteError::Refused(ErrorCode::Internal | ErrorCode::Handshake) => (
            503,
            "worker_unavailable",
            "worker failed while preparing the execution",
        ),
        RemoteError::Refused(ErrorCode::Malformed)
        | RemoteError::Tampered
        | RemoteError::Malformed
        | RemoteError::BadAddr(_) => (
            502,
            "invalid_worker_response",
            "worker could not be authenticated or returned an invalid response",
        ),
        RemoteError::Io(_) | RemoteError::Disconnected | RemoteError::ExhaustedRetries => (
            503,
            "workers_unavailable",
            "configured workers could not complete the task",
        ),
        RemoteError::NoHealthyNode => (
            503,
            "workers_unavailable",
            "no configured worker is healthy",
        ),
    };
    HttpResponse::error(status, code, message, Some(1))
}

fn runtime_unavailable_response(request_id: u64, descriptor: LanguageDescriptor) -> HttpResponse {
    let message = descriptor
        .unavailable_reason
        .unwrap_or("the selected runtime is not available");
    runtime_error_response(request_id, descriptor, 503, "runtime_unavailable", message)
}

fn runtime_error_response(
    request_id: u64,
    descriptor: LanguageDescriptor,
    status: u16,
    code: &str,
    message: &str,
) -> HttpResponse {
    HttpResponse {
        status,
        body: json!({
            "request_id":format!("req_{request_id}"),
            "status":"error",
            "runtime":{
                "language":descriptor.id,
                "name":descriptor.name,
                "runtime":descriptor.runtime,
                "available":descriptor.available
            },
            "error":{"code":code,"message":message}
        })
        .to_string(),
        retry_after_seconds: None,
    }
}

fn runner_error_response(
    request_id: u64,
    descriptor: LanguageDescriptor,
    error: RunnerError,
) -> HttpResponse {
    let (status, code, message) = match error {
        RunnerError::Compile(message) => (400, "compile_error", message),
        RunnerError::Runtime(message) => (422, "runtime_error", message),
        RunnerError::GasLimit => (
            429,
            "gas_limit_exceeded",
            "execution exceeded the configured instruction budget".to_string(),
        ),
        RunnerError::CpuLimit => (
            429,
            "cpu_limit_exceeded",
            "execution exceeded the request CPU budget".to_string(),
        ),
        RunnerError::MemoryLimit => (
            429,
            "memory_limit_exceeded",
            "execution exceeded the sandbox memory budget".to_string(),
        ),
        RunnerError::OutputLimit => (
            413,
            "output_too_large",
            "result exceeds configured output limit".to_string(),
        ),
        RunnerError::UnsupportedResult(message) => (422, "unsupported_result", message),
    };
    runtime_error_response(request_id, descriptor, status, code, &message)
}

fn value_tag_name(tag: ValueTag) -> &'static str {
    match tag {
        ValueTag::Number => "number",
        ValueTag::Bool => "boolean",
        ValueTag::String => "string",
        ValueTag::Nil => "null",
        ValueTag::Undef => "undefined",
    }
}

fn bearer_token<'a>(headers: &'a HashMap<String, String>, header_name: &str) -> Option<&'a str> {
    let value = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(header_name))
        .map(|(_, value)| value.trim())?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") || token.trim().is_empty() {
        return None;
    }
    Some(token.trim())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        diff |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    diff == 0
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn load_usage_state(path: &Path) -> Result<UsageState, String> {
    let _state_lock = lock_usage_state_file(path).map_err(|error| {
        format!(
            "cannot lock gateway usage state {}: {error}",
            path.display()
        )
    })?;
    load_usage_state_unlocked(path)
}

fn load_usage_state_unlocked(path: &Path) -> Result<UsageState, String> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(UsageState::default());
        }
        Err(error) => {
            return Err(format!(
                "cannot read gateway usage state {}: {error}",
                path.display()
            ));
        }
    };
    let snapshot: UsageSnapshot = serde_json::from_slice(&contents)
        .map_err(|error| format!("invalid gateway usage state {}: {error}", path.display()))?;
    if snapshot.version != 1 {
        return Err(format!(
            "unsupported gateway usage state version {} at {}",
            snapshot.version,
            path.display()
        ));
    }
    Ok(UsageState {
        keys: snapshot.keys,
        active: HashMap::new(),
    })
}

fn lock_usage_state_file(path: &Path) -> io::Result<File> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock_file = options.open(usage_lock_path(path))?;
    FileExt::lock_exclusive(&lock_file)?;
    Ok(lock_file)
}

fn usage_lock_path(path: &Path) -> PathBuf {
    let mut lock_name = path.as_os_str().to_os_string();
    lock_name.push(".lock");
    PathBuf::from(lock_name)
}

fn default_state_path() -> PathBuf {
    if let Some(path) = std::env::var_os(STATE_PATH_ENV).filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    #[cfg(windows)]
    if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local_app_data)
            .join("OmniEngine")
            .join("gateway-usage.json");
    }
    #[cfg(not(windows))]
    {
        if let Some(state_home) = std::env::var_os("XDG_STATE_HOME") {
            return PathBuf::from(state_home)
                .join("omniengine")
                .join("gateway-usage.json");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("state")
                .join("omniengine")
                .join("gateway-usage.json");
        }
    }
    PathBuf::from(DEFAULT_STATE_PATH)
}

fn persist_usage_state(path: &Path, state: &UsageState) -> io::Result<()> {
    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let filename = path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("omni-gateway-state"));
    let temporary_path = parent.join(format!(
        ".{}.{}.{}.tmp",
        filename.to_string_lossy(),
        std::process::id(),
        NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let snapshot = UsageSnapshot {
        version: 1,
        keys: state.keys.clone(),
    };
    let serialized = serde_json::to_vec(&snapshot)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let write_result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary_path)?;
        file.write_all(&serialized)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary_path, path)?;
        #[cfg(unix)]
        if let Err(error) = File::open(parent).and_then(|directory| directory.sync_all()) {
            eprintln!(
                "omni-gateway: usage state was replaced but its directory could not be synced: {error}"
            );
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    write_result
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn current_month() -> i64 {
    let days = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
        / 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let mut year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = mp + if mp < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    year * 12 + month
}

fn load_tls_config(cert_path: &Path, key_path: &Path) -> io::Result<ServerConfig> {
    let mut cert_reader = BufReader::new(File::open(cert_path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot open TLS certificate {}: {error}",
                cert_path.display()
            ),
        )
    })?);
    let certificates = certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid TLS certificate PEM in {}: {error}",
                    cert_path.display()
                ),
            )
        })?;
    if certificates.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("TLS certificate file {} is empty", cert_path.display()),
        ));
    }
    let mut key_reader = BufReader::new(File::open(key_path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot open TLS private key {}: {error}",
                key_path.display()
            ),
        )
    })?);
    let key: PrivateKeyDer<'static> = private_key(&mut key_reader)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "invalid TLS private-key PEM in {}: {error}",
                    key_path.display()
                ),
            )
        })?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "TLS private-key file {} contains no private key",
                    key_path.display()
                ),
            )
        })?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "TLS certificate {} and private key {} are incompatible: {error}",
                    cert_path.display(),
                    key_path.display()
                ),
            )
        })
}

fn serve_tls_connection(
    stream: TcpStream,
    tls: Arc<ServerConfig>,
    gateway: Arc<Gateway>,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let connection = rustls::ServerConnection::new(tls)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut stream = rustls::StreamOwned::new(connection, stream);
    serve_http_connection(&mut stream, gateway)?;
    stream.conn.send_close_notify();
    stream.flush()
}

fn serve_plain_connection(stream: TcpStream, gateway: Arc<Gateway>) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut stream = stream;
    serve_http_connection(&mut stream, gateway)
}

fn serve_http_connection(
    stream: &mut (impl Read + Write),
    gateway: Arc<Gateway>,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    if method.is_empty() || path.is_empty() || parts.next().is_none() {
        return write_http_response(
            reader.get_mut(),
            HttpResponse::error(400, "bad_request", "invalid HTTP request line", None),
        );
    }
    let mut headers = HashMap::new();
    let mut header_bytes = request_line.len();
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(());
        }
        header_bytes += read;
        if header_bytes > MAX_HEADER_BYTES {
            return write_http_response(
                reader.get_mut(),
                HttpResponse::error(431, "headers_too_large", "HTTP headers exceed limit", None),
            );
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return write_http_response(
                reader.get_mut(),
                HttpResponse::error(400, "bad_request", "invalid HTTP header", None),
            );
        };
        headers.insert(name.trim().to_string(), value.trim().to_string());
    }
    if headers
        .keys()
        .any(|name| name.eq_ignore_ascii_case("transfer-encoding"))
    {
        return write_http_response(
            reader.get_mut(),
            HttpResponse::error(
                400,
                "bad_request",
                "chunked transfer encoding is not supported",
                None,
            ),
        );
    }
    let origin = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("origin"))
        .map(|(_, value)| value.as_str());
    let cors_origin = match origin {
        Some(origin)
            if gateway
                .settings
                .config
                .allowed_origins
                .iter()
                .any(|allowed| allowed == origin) =>
        {
            Some(origin)
        }
        Some(_) => {
            return write_http_response(
                reader.get_mut(),
                HttpResponse::error(403, "origin_not_allowed", "origin is not allowed", None),
            );
        }
        None => None,
    };
    if method.eq_ignore_ascii_case("OPTIONS") {
        let response = if cors_origin.is_some() {
            HttpResponse {
                status: 204,
                body: String::new(),
                retry_after_seconds: None,
            }
        } else {
            HttpResponse::error(
                405,
                "method_not_allowed",
                "OPTIONS requires an allowed Origin",
                None,
            )
        };
        return write_http_response_with_cors(reader.get_mut(), response, cors_origin, true);
    }
    let content_length_header = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.as_str());
    let content_length = match content_length_header {
        Some(value) => match value.parse::<usize>() {
            Ok(length) => length,
            Err(_) => {
                return write_http_response_with_cors(
                    reader.get_mut(),
                    HttpResponse::error(400, "bad_request", "invalid Content-Length", None),
                    cors_origin,
                    false,
                );
            }
        },
        None => 0,
    };
    let max_body = gateway.settings.config.resource_limits.max_source_bytes + MAX_BODY_OVERHEAD;
    if content_length > max_body {
        return write_http_response_with_cors(
            reader.get_mut(),
            HttpResponse::error(
                413,
                "payload_too_large",
                "request body exceeds configured limit",
                None,
            ),
            cors_origin,
            false,
        );
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    let response = gateway.handle(method, path, &headers, &body);
    write_http_response_with_cors(reader.get_mut(), response, cors_origin, false)
}

fn serve_tls_listener(
    listener: TcpListener,
    tls: Arc<ServerConfig>,
    gateway: Arc<Gateway>,
    stop: Option<Arc<AtomicBool>>,
) -> io::Result<()> {
    if stop.is_some() {
        listener.set_nonblocking(true)?;
    }
    loop {
        if stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Acquire))
        {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                let gateway = Arc::clone(&gateway);
                let tls = Arc::clone(&tls);
                std::thread::Builder::new()
                    .name("omni-gateway-connection".to_string())
                    .spawn(move || {
                        if let Err(error) = serve_tls_connection(stream, tls, gateway) {
                            eprintln!("omni-gateway connection failed: {error}");
                        }
                    })?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock && stop.is_some() => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

fn serve_plain_listener(
    listener: TcpListener,
    gateway: Arc<Gateway>,
    stop: Option<Arc<AtomicBool>>,
) -> io::Result<()> {
    if stop.is_some() {
        listener.set_nonblocking(true)?;
    }
    loop {
        if stop
            .as_ref()
            .is_some_and(|stop| stop.load(Ordering::Acquire))
        {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false)?;
                let gateway = Arc::clone(&gateway);
                std::thread::Builder::new()
                    .name("omni-gateway-connection".to_string())
                    .spawn(move || {
                        if let Err(error) = serve_plain_connection(stream, gateway) {
                            eprintln!("omni-gateway connection failed: {error}");
                        }
                    })?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock && stop.is_some() => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

fn write_http_response(stream: &mut impl Write, response: HttpResponse) -> io::Result<()> {
    write_http_response_with_cors(stream, response, None, false)
}

fn write_http_response_with_cors(
    stream: &mut impl Write,
    response: HttpResponse,
    origin: Option<&str>,
    preflight: bool,
) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        409 => "Conflict",
        422 => "Unprocessable Content",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        429 => "Too Many Requests",
        501 => "Not Implemented",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Service Unavailable",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason,
        response.body.len()
    )?;
    if let Some(retry) = response.retry_after_seconds {
        write!(stream, "Retry-After: {retry}\r\n")?;
    }
    if let Some(origin) = origin {
        write!(
            stream,
            "Access-Control-Allow-Origin: {origin}\r\nVary: Origin\r\n"
        )?;
        if preflight {
            write!(
                stream,
                "Access-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: Authorization, Content-Type\r\nAccess-Control-Max-Age: 600\r\n"
            )?;
        }
    }
    write!(stream, "\r\n{}", response.body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, ClientConnection, RootCertStore};
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicBool;

    const POLICY: &str = include_str!("../../../policies/usage-tiers.toml");
    const CONFIG: &str = include_str!("../../../config/cloud-api.example.toml");
    const TOKEN: &str = "omni_test_key_which_is_long_enough_01";
    const CLUSTER_PSK: [u8; 32] = [0x5a; 32];
    static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "omni-gateway-test-{}-{}",
                std::process::id(),
                NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn state_path(&self) -> PathBuf {
            self.0.join("usage-state.json")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_worker_certificate(directory: &TestDirectory) -> (PathBuf, PathBuf, Vec<u8>) {
        let certificate_path = directory.0.join("worker-cert.pem");
        let private_key_path = directory.0.join("worker-key.pem");
        let generated = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        let certificate_pem = generated.cert.pem().into_bytes();
        fs::write(&certificate_path, &certificate_pem).unwrap();
        fs::write(&private_key_path, generated.key_pair.serialize_pem()).unwrap();
        (certificate_path, private_key_path, certificate_pem)
    }

    fn configure_cluster(
        settings: &mut GatewaySettings,
        endpoints: Vec<String>,
        ca_path: PathBuf,
        quorum: usize,
    ) {
        settings.config.cluster.enabled = true;
        settings.config.cluster.worker_endpoints = endpoints;
        settings.config.cluster.tls_ca_file = Some(ca_path);
        settings.config.cluster.tls_server_name = Some("localhost".to_string());
        settings.config.cluster.quorum = quorum;
    }

    fn tls_worker(
        psk: [u8; 32],
        certificate_path: &Path,
        private_key_path: &Path,
    ) -> omni_cluster::WorkerNode {
        omni_cluster::WorkerNode::start(omni_cluster::WorkerConfig {
            psk,
            tls_cert_pem: Some(certificate_path.to_path_buf()),
            tls_key_pem: Some(private_key_path.to_path_buf()),
            ..Default::default()
        })
        .unwrap()
    }

    fn lying_tls_worker(
        psk: [u8; 32],
        certificate_path: &Path,
        private_key_path: &Path,
    ) -> (String, std::thread::JoinHandle<()>) {
        let tls = Arc::new(load_tls_config(certificate_path, private_key_path).unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let handle = std::thread::spawn(move || {
            for incoming in listener.incoming().take(2) {
                let stream = incoming.unwrap();
                let connection = rustls::ServerConnection::new(Arc::clone(&tls)).unwrap();
                let mut stream = rustls::StreamOwned::new(connection, stream);
                let (mut read, mut write) =
                    omni_cluster::wire::handshake(&mut stream, &psk, true).unwrap();
                let request = read.recv(&mut stream).unwrap();
                let reply = match request {
                    omni_cluster::wire::Message::Ping => omni_cluster::wire::Message::Pong,
                    omni_cluster::wire::Message::Task { id, .. }
                    | omni_cluster::wire::Message::TaskWithLimits { id, .. } => {
                        omni_cluster::wire::Message::Value {
                            id,
                            tag: ValueTag::Number,
                            num: 41.0,
                            text: b"41".to_vec(),
                        }
                    }
                    _ => omni_cluster::wire::Message::Error {
                        id: 0,
                        code: ErrorCode::Malformed,
                    },
                };
                write.send(&mut stream, &reply).unwrap();
            }
        });
        (address, handle)
    }

    struct TestHttpsServer {
        address: SocketAddr,
        client_config: Arc<ClientConfig>,
        stop: Arc<AtomicBool>,
        server_thread: Option<std::thread::JoinHandle<io::Result<()>>>,
        _directory: TestDirectory,
    }

    impl TestHttpsServer {
        fn start(settings: GatewaySettings, tenant_id: &str, tier: TierPolicy) -> Self {
            Self::start_with_runtime(settings, tenant_id, tier, None)
        }

        fn start_with_cluster(
            settings: GatewaySettings,
            tenant_id: &str,
            tier: TierPolicy,
            cluster: ClusterClient,
        ) -> Self {
            Self::start_with_runtime(settings, tenant_id, tier, Some(cluster))
        }

        fn start_with_runtime(
            mut settings: GatewaySettings,
            tenant_id: &str,
            tier: TierPolicy,
            cluster: Option<ClusterClient>,
        ) -> Self {
            let directory = TestDirectory::new();
            let certificate_path = directory.0.join("server-cert.pem");
            let private_key_path = directory.0.join("server-key.pem");
            let generated =
                rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
            fs::write(&certificate_path, generated.cert.pem()).unwrap();
            fs::write(&private_key_path, generated.key_pair.serialize_pem()).unwrap();
            settings.config.tls_cert = certificate_path.clone();
            settings.config.tls_key = private_key_path;

            let gateway = Arc::new(
                Gateway::new_with_runtime_cluster(
                    settings,
                    vec![ApiKey::new(TOKEN, tenant_id, tier).unwrap()],
                    directory.state_path(),
                    cluster,
                )
                .unwrap(),
            );
            let tls = Arc::new(
                load_tls_config(
                    &gateway.settings.config.tls_cert,
                    &gateway.settings.config.tls_key,
                )
                .unwrap(),
            );
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let certificate = certs(&mut BufReader::new(File::open(certificate_path).unwrap()))
                .next()
                .unwrap()
                .unwrap();
            let mut roots = RootCertStore::empty();
            roots.add(certificate).unwrap();
            let client_config = Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            );
            let stop = Arc::new(AtomicBool::new(false));
            let server_thread = {
                let gateway = Arc::clone(&gateway);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || serve_tls_listener(listener, tls, gateway, Some(stop)))
            };
            Self {
                address,
                client_config,
                stop,
                server_thread: Some(server_thread),
                _directory: directory,
            }
        }

        fn request(&self, auth: Option<&str>, source: &str) -> (u16, String) {
            self.request_body(auth, &json!({"source":source}).to_string())
        }

        fn request_body(&self, auth: Option<&str>, body: &str) -> (u16, String) {
            self.request_http("POST", "/v1/execute", auth, body)
        }

        fn health(&self) -> (u16, String) {
            self.request_http("GET", "/v1/health", None, "")
        }

        fn request_http(
            &self,
            method: &str,
            path: &str,
            auth: Option<&str>,
            body: &str,
        ) -> (u16, String) {
            let server_name = ServerName::try_from("localhost").unwrap();
            let connection =
                ClientConnection::new(Arc::clone(&self.client_config), server_name).unwrap();
            let socket = TcpStream::connect(self.address).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, socket);
            write!(
                stream,
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n"
            )
            .unwrap();
            if let Some(auth) = auth {
                write!(stream, "Authorization: {auth}\r\n").unwrap();
            }
            write!(
                stream,
                "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            stream.flush().unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            let response = String::from_utf8(response).unwrap();
            let (head, body) = response.split_once("\r\n\r\n").unwrap();
            let status = head
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .nth(1)
                .unwrap()
                .parse()
                .unwrap();
            (status, body.to_string())
        }
    }

    impl Drop for TestHttpsServer {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(server_thread) = self.server_thread.take() {
                server_thread.join().unwrap().unwrap();
            }
        }
    }

    fn settings() -> GatewaySettings {
        GatewaySettings::parse(POLICY, CONFIG).unwrap()
    }

    #[test]
    fn gateway_startup_tls_errors_name_the_path_and_local_bootstrap() {
        let directory = TestDirectory::new();
        let certificate_path = directory.0.join("missing-cert.pem");
        let private_key_path = directory.0.join("missing-key.pem");
        let mut settings = settings();
        settings.config.tls_cert = certificate_path.clone();
        settings.config.tls_key = private_key_path;
        let tier = settings.free_tier.clone();
        let gateway = Arc::new(
            Gateway::new_with_state_path(
                settings,
                vec![ApiKey::new(TOKEN, "tls-test", tier).unwrap()],
                directory.state_path(),
            )
            .unwrap(),
        );

        let error = gateway.serve().unwrap_err().to_string();

        assert!(error.contains(&certificate_path.display().to_string()));
        assert!(error.contains("cannot open TLS certificate"));
        assert!(error.contains(".\\scripts\\dev-up.ps1"));
    }

    fn numeric_chain(count: usize) -> String {
        let mut source = String::from("let x0 = 1;");
        for index in 1..count {
            source.push_str(&format!("let x{index} = x{} + 1;", index - 1));
        }
        source
    }

    fn string_chain(count: usize) -> String {
        let mut source = String::from("let x0 = \"a\";");
        for index in 1..count {
            source.push_str(&format!("let x{index} = x{} + \"a\";", index - 1));
        }
        source
    }

    fn request(gateway: &Gateway, auth: Option<&str>, source: &str) -> HttpResponse {
        request_body(gateway, auth, &json!({"source":source}).to_string())
    }

    fn request_language(
        gateway: &Gateway,
        auth: Option<&str>,
        language: &str,
        source: &str,
    ) -> HttpResponse {
        request_body(
            gateway,
            auth,
            &json!({"language":language,"source":source}).to_string(),
        )
    }

    fn request_body(gateway: &Gateway, auth: Option<&str>, body: &str) -> HttpResponse {
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        if let Some(auth) = auth {
            headers.insert("Authorization".to_string(), auth.to_string());
        }
        gateway.handle("POST", "/v1/execute", &headers, body.as_bytes())
    }

    #[test]
    fn execute_preserves_utf8_json_source_and_output() {
        let settings = settings();
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "unicode-tenant", settings.free_tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        let expected = "বাংলা — हिन्दी — 日本語 — العربية — Ελληνικά — 😀";
        let source = format!("// বাংলা — हिन्दी — 日本語 — العربية — Ελληνικά — 😀\n\"{expected}\"");

        let response = request(&gateway, Some(&format!("Bearer {TOKEN}")), &source);

        assert_eq!(response.status, 200, "{}", response.body);
        let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["value"]["text"], expected);
        assert_eq!(body["runtime"]["language"], "omni");
        assert!(response.body.contains(expected));
    }

    #[test]
    fn execute_selects_lua_and_reports_the_runtime() {
        let settings = settings();
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "lua-tenant", settings.free_tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();

        let response = request_language(
            &gateway,
            Some(&format!("Bearer {TOKEN}")),
            "lua",
            "return \"বাংলা — 日本語 — 😀\"",
        );

        assert_eq!(response.status, 200, "{}", response.body);
        let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["runtime"]["language"], "lua");
        assert_eq!(body["runtime"]["runtime"], "mlua / Lua 5.4");
        assert_eq!(body["value"]["tag"], "string");
        assert_eq!(body["value"]["text"], "বাংলা — 日本語 — 😀");

        let verification = request_body(
            &gateway,
            Some(&format!("Bearer {TOKEN}")),
            &json!({
                "language":"lua",
                "source":"return 42",
                "verification":"deterministic"
            })
            .to_string(),
        );
        assert_eq!(verification.status, 501);
        let body: serde_json::Value = serde_json::from_str(&verification.body).unwrap();
        assert_eq!(body["error"]["code"], "verification_unavailable");
    }

    #[test]
    fn unsupported_and_unavailable_languages_have_distinct_errors() {
        let settings = settings();
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "language-errors", settings.free_tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        let auth = format!("Bearer {TOKEN}");
        let unavailable = request_language(&gateway, Some(&auth), "javascript", "return 1");
        assert_eq!(unavailable.status, 503);
        let body: serde_json::Value = serde_json::from_str(&unavailable.body).unwrap();
        assert_eq!(body["error"]["code"], "runtime_unavailable");
        assert_eq!(body["runtime"]["available"], false);

        let unsupported = request_language(&gateway, Some(&auth), "ruby", "puts 1");
        assert_eq!(unsupported.status, 422);
        let body: serde_json::Value = serde_json::from_str(&unsupported.body).unwrap();
        assert_eq!(body["error"]["code"], "unsupported_language");
    }

    #[test]
    fn language_discovery_reports_runtime_status_and_sandbox_policy() {
        let settings = settings();
        let tier = settings.free_tier.clone();
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings,
            vec![ApiKey::new(TOKEN, "language-discovery", tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();

        let response = gateway.handle("GET", "/v1/languages", &HashMap::new(), &[]);

        assert_eq!(response.status, 200);
        let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
        let languages = body["languages"].as_array().unwrap();
        assert!(
            languages
                .iter()
                .any(|language| language["id"] == "lua" && language["available"] == true)
        );
        assert!(
            languages
                .iter()
                .any(|language| language["id"] == "python" && language["available"] == false)
        );
        let lua = languages
            .iter()
            .find(|language| language["id"] == "lua")
            .unwrap();
        assert_eq!(lua["standard_libraries"], json!(["base", "math"]));
        assert_eq!(lua["capabilities"]["filesystem_reads"], false);
        assert_eq!(lua["capabilities"]["filesystem_writes"], false);
        assert_eq!(lua["capabilities"]["module_imports"], false);
        assert_eq!(lua["capabilities"]["host_functions"], false);
        assert_eq!(lua["capabilities"]["network_access"], false);
        assert_eq!(lua["capabilities"]["cluster_dispatch"], false);
        assert_eq!(lua["capabilities"]["memory_limit"], true);
        assert_eq!(body["sandbox"]["filesystem_reads"], false);
        assert_eq!(body["sandbox"]["filesystem_writes"], false);
        assert_eq!(body["sandbox"]["module_imports"], false);
        assert_eq!(body["sandbox"]["host_functions"], false);
        assert_eq!(body["sandbox"]["network_access"], false);
        assert_eq!(body["sandbox"]["deterministic_verification"], true);
        assert_eq!(body["limits"]["sandbox_memory_mib"], 128);
    }

    #[test]
    fn lua_execution_obeys_gateway_cpu_and_instruction_limits() {
        let settings = settings();
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "lua-limits", settings.free_tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        let auth = format!("Bearer {TOKEN}");
        let gas_limited = request_body(
            &gateway,
            Some(&auth),
            &json!({"language":"lua","source":"while true do end","gas_limit":200}).to_string(),
        );
        assert_eq!(gas_limited.status, 429);
        let body: serde_json::Value = serde_json::from_str(&gas_limited.body).unwrap();
        assert_eq!(body["error"]["code"], "gas_limit_exceeded");

        let cpu_limited = request_body(
            &gateway,
            Some(&auth),
            &json!({
                "language":"lua",
                "source":"while true do end",
                "gas_limit":1000000000,
                "timeout_ms":1
            })
            .to_string(),
        );
        assert_eq!(cpu_limited.status, 429);
        let body: serde_json::Value = serde_json::from_str(&cpu_limited.body).unwrap();
        assert_eq!(body["error"]["code"], "cpu_limit_exceeded");
    }

    #[test]
    fn execute_enforces_source_limit_in_utf8_bytes() {
        let settings = settings();
        let max_source_bytes = settings.config.resource_limits.max_source_bytes;
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "unicode-limit-tenant", settings.free_tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        let source = format!("\"{}\"", "ক".repeat(max_source_bytes / "ক".len() + 1));
        assert!(source.chars().count() < max_source_bytes);
        assert!(source.len() > max_source_bytes);

        let response = request(&gateway, Some(&format!("Bearer {TOKEN}")), &source);

        assert_eq!(response.status, 413);
        let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["error"]["code"], "source_too_large");
    }

    fn health(gateway: &Gateway) -> HttpResponse {
        gateway.handle("GET", "/v1/health", &HashMap::new(), &[])
    }

    #[test]
    fn health_endpoint_reports_aggregate_request_and_quota_metrics() {
        let mut settings = settings();
        settings.config.rate_limit.free_max_requests_per_window = 0;
        let mut tier = settings.free_tier.clone();
        tier.api_requests_per_month = 1;
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings,
            vec![ApiKey::new(TOKEN, "metrics-tenant", tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();

        assert_eq!(
            request(&gateway, Some(&format!("Bearer {TOKEN}")), "1").status,
            200
        );
        assert_eq!(
            request(&gateway, Some(&format!("Bearer {TOKEN}")), "1").status,
            429
        );
        assert_eq!(
            request(&gateway, Some("Bearer invalid-token"), "1").status,
            401
        );

        let response = health(&gateway);
        assert_eq!(response.status, 200);
        let body: serde_json::Value = serde_json::from_str(&response.body).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["metrics"]["requests_total"], 3);
        assert_eq!(body["metrics"]["requests_succeeded_total"], 1);
        assert_eq!(body["metrics"]["requests_failed_total"], 2);
        assert_eq!(body["metrics"]["executions_succeeded_total"], 1);
        assert_eq!(body["metrics"]["quota_rejections_total"], 1);
        assert_eq!(body["cluster"]["enabled"], false);
        assert_eq!(body["cluster"]["configured_workers"], 0);
    }

    #[test]
    fn state_file_lock_blocks_admission_until_released() {
        let settings = settings();
        let test_dir = TestDirectory::new();
        let state_path = test_dir.state_path();
        let gateway = Arc::new(
            Gateway::new_with_state_path(
                settings.clone(),
                vec![ApiKey::new(TOKEN, "locked-state", settings.free_tier).unwrap()],
                &state_path,
            )
            .unwrap(),
        );
        let state_lock = lock_usage_state_file(&state_path).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let request_gateway = Arc::clone(&gateway);
        let request_thread = std::thread::spawn(move || {
            let response = request(&request_gateway, Some(&format!("Bearer {TOKEN}")), "1");
            sender.send(response.status).unwrap();
        });

        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        drop(state_lock);
        assert_eq!(receiver.recv_timeout(Duration::from_secs(2)).unwrap(), 200);
        request_thread.join().unwrap();
    }

    #[test]
    fn concurrent_gateway_instances_preserve_shared_quota_updates() {
        let mut settings = settings();
        settings.config.rate_limit.free_max_requests_per_window = 0;
        settings.config.rate_limit.free_max_concurrent = 0;
        let mut tier = settings.free_tier.clone();
        tier.api_requests_per_month = 19;
        tier.max_concurrent_requests = 0;
        let test_dir = TestDirectory::new();
        let gateways = (0..4)
            .map(|_| {
                Arc::new(
                    Gateway::new_with_state_path(
                        settings.clone(),
                        vec![ApiKey::new(TOKEN, "multi-instance", tier.clone()).unwrap()],
                        test_dir.state_path(),
                    )
                    .unwrap(),
                )
            })
            .collect::<Vec<_>>();
        let succeeded = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let rejected = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let threads = gateways
            .into_iter()
            .map(|gateway| {
                let succeeded = Arc::clone(&succeeded);
                let rejected = Arc::clone(&rejected);
                std::thread::spawn(move || {
                    for _ in 0..8 {
                        let response = request(&gateway, Some(&format!("Bearer {TOKEN}")), "1");
                        if response.status == 200 {
                            succeeded.fetch_add(1, Ordering::Relaxed);
                        } else if response.status == 429
                            && response.body.contains("monthly_quota_exceeded")
                        {
                            rejected.fetch_add(1, Ordering::Relaxed);
                        } else {
                            panic!("unexpected admission response: {}", response.body);
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }

        assert_eq!(succeeded.load(Ordering::Relaxed), 19);
        assert_eq!(rejected.load(Ordering::Relaxed), 13);
        let reloaded = Gateway::new_with_state_path(
            settings,
            vec![ApiKey::new(TOKEN, "multi-instance", tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        assert!(
            request(&reloaded, Some(&format!("Bearer {TOKEN}")), "1")
                .body
                .contains("monthly_quota_exceeded")
        );
    }

    #[test]
    fn parses_and_validates_free_tier_and_gateway_config() {
        let settings = settings();
        assert_eq!(settings.free_tier.api_requests_per_month, 100_000);
        assert_eq!(settings.free_tier.cpu_ms_per_request, 50);
        assert_eq!(settings.free_tier.sandbox_memory_mib, 128);
        assert_eq!(settings.free_tier.max_concurrent_requests, 10);
        assert_eq!(settings.config.execute_endpoint, "/v1/execute");
        assert_eq!(settings.config.bind, "127.0.0.1:8443");
        assert!(settings.tls_required);
    }

    #[test]
    fn deployment_environment_overrides_preserve_sandbox_policy() {
        let environment = HashMap::from([
            ("OMNI_BIND_ADDR".to_string(), "0.0.0.0".to_string()),
            ("OMNI_PORT".to_string(), "9443".to_string()),
            (
                "OMNI_API_KEYS".to_string(),
                "tenant:opaque-secret-value-not-logged".to_string(),
            ),
            ("OMNI_TLS_MODE".to_string(), "plain".to_string()),
            (
                "OMNI_ALLOWED_ORIGINS".to_string(),
                "https://studio.example.com, https://console.example.com".to_string(),
            ),
        ]);
        let overridden = settings().with_environment_overrides(&environment).unwrap();

        assert_eq!(overridden.config.bind, "0.0.0.0:9443");
        assert!(!overridden.tls_required);
        assert_eq!(overridden.auth.token_secret_source, "env:OMNI_API_KEYS");
        assert_eq!(
            overridden.config.allowed_origins,
            ["https://studio.example.com", "https://console.example.com"]
        );
        assert!(!overridden.config.sandbox.filesystem_reads);
        assert!(!overridden.config.sandbox.filesystem_writes);
        assert!(!overridden.config.sandbox.module_imports);
        assert!(!overridden.config.sandbox.host_functions);
        assert!(!overridden.config.sandbox.network_access);
        assert!(overridden.config.sandbox.deterministic_verification);
    }

    #[test]
    fn deployment_environment_overrides_reject_invalid_values() {
        let invalid_mode = HashMap::from([("OMNI_TLS_MODE".to_string(), "disabled".to_string())]);
        assert!(
            settings()
                .with_environment_overrides(&invalid_mode)
                .unwrap_err()
                .contains("OMNI_TLS_MODE")
        );

        for origin in [
            "https://studio.example.com/path",
            "https://studio.example.com\r\nX-Injected: yes",
            "https://*.example.com",
        ] {
            let invalid_origin =
                HashMap::from([("OMNI_ALLOWED_ORIGINS".to_string(), origin.to_string())]);
            assert!(
                settings()
                    .with_environment_overrides(&invalid_origin)
                    .unwrap_err()
                    .contains("invalid allowed origin")
            );
        }
    }

    #[test]
    fn plain_http_listener_serves_health_and_enforces_cors_origins() {
        let directory = TestDirectory::new();
        let mut settings = settings();
        settings
            .config
            .allowed_origins
            .push("https://studio.example.com".to_string());
        let tier = settings.free_tier.clone();
        let gateway = Arc::new(
            Gateway::new_with_state_path(
                settings,
                vec![ApiKey::new(TOKEN, "plain-http-test", tier).unwrap()],
                directory.state_path(),
            )
            .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server =
            std::thread::spawn(move || serve_plain_listener(listener, gateway, Some(server_stop)));

        let request = |raw: &str| {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        };
        let health = request(
            "GET /v1/health HTTP/1.1\r\nHost: localhost\r\nOrigin: https://studio.example.com\r\nConnection: close\r\n\r\n",
        );
        assert!(health.starts_with("HTTP/1.1 200 OK"));
        assert!(health.contains("Access-Control-Allow-Origin: https://studio.example.com"));

        let body = json!({"source":"40 + 2"}).to_string();
        let execution = request(&format!(
            "POST /v1/execute HTTP/1.1\r\nHost: localhost\r\nOrigin: https://studio.example.com\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        assert!(execution.starts_with("HTTP/1.1 200 OK"));
        assert!(execution.contains("\"number\":42"));
        assert!(execution.contains("Access-Control-Allow-Origin: https://studio.example.com"));

        let preflight = request(
            "OPTIONS /v1/execute HTTP/1.1\r\nHost: localhost\r\nOrigin: https://studio.example.com\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: authorization,content-type\r\nConnection: close\r\n\r\n",
        );
        assert!(preflight.starts_with("HTTP/1.1 204 No Content"));
        assert!(preflight.contains("Access-Control-Allow-Methods: GET, POST, OPTIONS"));

        let blocked = request(
            "GET /v1/health HTTP/1.1\r\nHost: localhost\r\nOrigin: https://attacker.example\r\nConnection: close\r\n\r\n",
        );
        assert!(blocked.starts_with("HTTP/1.1 403 Forbidden"));

        stop.store(true, Ordering::Release);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn rejects_policy_and_config_drift() {
        let changed = CONFIG.replace("free_max_concurrent = 10", "free_max_concurrent = 11");
        assert!(GatewaySettings::parse(POLICY, &changed).is_err());
    }

    #[test]
    fn executes_valid_sandbox_request() {
        let settings = settings();
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "test-tenant", settings.free_tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        let response = request(&gateway, Some(&format!("Bearer {TOKEN}")), "40 + 2");
        assert_eq!(response.status, 200);
        assert!(response.body.contains("\"number\":42.0"));
        assert!(response.body.contains("\"tenant_id\":\"test-tenant\""));
    }

    #[test]
    fn enforces_cpu_deadline_during_vm_execution() {
        let settings = settings();
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "cpu-limit", settings.free_tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        let source = numeric_chain(2_000);
        let response = request_body(
            &gateway,
            Some(&format!("Bearer {TOKEN}")),
            &json!({"source":source,"timeout_ms":1}).to_string(),
        );
        assert_eq!(response.status, 429, "{}", response.body);
        assert!(response.body.contains("cpu_limit_exceeded"));
    }

    #[test]
    fn enforces_combined_sandbox_arena_memory_limit() {
        let mut settings = settings();
        settings.free_tier.cpu_ms_per_request = 1_000;
        settings.free_tier.sandbox_memory_mib = 1;
        settings.config.resource_limits.cpu_ms_per_request = 1_000;
        settings.config.resource_limits.request_timeout_ms = 1_000;
        settings.config.resource_limits.sandbox_memory_mib = 1;
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "memory-limit", settings.free_tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        let source = string_chain(1_600);
        let response = request_body(
            &gateway,
            Some(&format!("Bearer {TOKEN}")),
            &json!({"source":source,"timeout_ms":1_000}).to_string(),
        );
        assert_eq!(response.status, 429, "{}", response.body);
        assert!(response.body.contains("memory_limit_exceeded"));
    }

    #[test]
    fn configured_worker_quorum_dispatches_and_verifies_execution() {
        let directory = TestDirectory::new();
        let (certificate, private_key, ca_pem) = test_worker_certificate(&directory);
        let first = tls_worker(CLUSTER_PSK, &certificate, &private_key);
        let second = tls_worker(CLUSTER_PSK, &certificate, &private_key);
        let endpoints = vec![first.addr().to_string(), second.addr().to_string()];
        let mut settings = settings();
        configure_cluster(&mut settings, endpoints.clone(), certificate, 2);
        let cluster = ClusterClient::new(endpoints, CLUSTER_PSK).with_tls(&ca_pem, "localhost");
        assert_eq!(cluster.health_check(), 2);
        let tier = settings.free_tier.clone();
        let server = TestHttpsServer::start_with_cluster(settings, "worker-quorum", tier, cluster);

        let (status, body) = server.request(Some(&format!("Bearer {TOKEN}")), "40 + 2");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"number\":42.0"));
        let (status, body) = server.request_body(
            Some(&format!("Bearer {TOKEN}")),
            r#"{"source":"6 * 7","verification":"deterministic"}"#,
        );
        assert_eq!(status, 200);
        assert!(body.contains("\"number\":42.0"));
        let (status, metrics_body) = server.health();
        assert_eq!(status, 200);
        let metrics: serde_json::Value = serde_json::from_str(&metrics_body).unwrap();
        assert_eq!(metrics["metrics"]["worker_dispatches_total"], 2);
        assert_eq!(metrics["metrics"]["worker_quorum_successes_total"], 1);
    }

    #[test]
    fn deterministic_verification_returns_mismatch_and_quorum_errors() {
        let directory = TestDirectory::new();
        let (certificate, private_key, ca_pem) = test_worker_certificate(&directory);
        let honest = tls_worker(CLUSTER_PSK, &certificate, &private_key);
        let (liar_endpoint, liar_thread) =
            lying_tls_worker(CLUSTER_PSK, &certificate, &private_key);
        let endpoints = vec![honest.addr().to_string(), liar_endpoint];
        let mut mismatch_settings = settings();
        configure_cluster(
            &mut mismatch_settings,
            endpoints.clone(),
            certificate.clone(),
            2,
        );
        let cluster = ClusterClient::new(endpoints, CLUSTER_PSK).with_tls(&ca_pem, "localhost");
        let tier = mismatch_settings.free_tier.clone();
        let mismatch_server = TestHttpsServer::start_with_cluster(
            mismatch_settings,
            "worker-mismatch",
            tier,
            cluster,
        );
        let (status, body) = mismatch_server.request_body(
            Some(&format!("Bearer {TOKEN}")),
            r#"{"source":"6 * 7","verification":"deterministic"}"#,
        );
        assert_eq!(status, 409, "{body}");
        assert!(body.contains("deterministic_mismatch"));
        let (status, metrics_body) = mismatch_server.health();
        assert_eq!(status, 200);
        let metrics: serde_json::Value = serde_json::from_str(&metrics_body).unwrap();
        assert_eq!(metrics["metrics"]["worker_quorum_mismatches_total"], 1);
        drop(mismatch_server);
        liar_thread.join().unwrap();

        let unavailable = tls_worker(CLUSTER_PSK, &certificate, &private_key);
        let unavailable_endpoint = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_endpoint = unavailable_endpoint.local_addr().unwrap().to_string();
        drop(unavailable_endpoint);
        let endpoints = vec![unavailable.addr().to_string(), dead_endpoint];
        let mut unavailable_settings = settings();
        configure_cluster(&mut unavailable_settings, endpoints.clone(), certificate, 2);
        let cluster = ClusterClient::new(endpoints, CLUSTER_PSK).with_tls(&ca_pem, "localhost");
        let tier = unavailable_settings.free_tier.clone();
        let server = TestHttpsServer::start_with_cluster(
            unavailable_settings,
            "worker-unavailable",
            tier,
            cluster,
        );
        let (status, body) = server.request_body(
            Some(&format!("Bearer {TOKEN}")),
            r#"{"source":"6 * 7","verification":"deterministic"}"#,
        );
        assert_eq!(status, 503);
        assert!(body.contains("verification_quorum_unavailable"));
        let (status, metrics_body) = server.health();
        assert_eq!(status, 200);
        let metrics: serde_json::Value = serde_json::from_str(&metrics_body).unwrap();
        assert_eq!(metrics["metrics"]["worker_quorum_unavailable_total"], 1);
    }

    #[test]
    fn remote_workers_enforce_request_cpu_and_memory_caps() {
        let directory = TestDirectory::new();
        let (certificate, private_key, ca_pem) = test_worker_certificate(&directory);
        let worker = omni_cluster::WorkerNode::start(omni_cluster::WorkerConfig {
            psk: CLUSTER_PSK,
            tls_cert_pem: Some(certificate.clone()),
            tls_key_pem: Some(private_key),
            ..Default::default()
        })
        .unwrap();
        let endpoints = vec![worker.addr().to_string()];
        let mut settings = settings();
        settings.free_tier.sandbox_memory_mib = 1;
        settings.config.resource_limits.sandbox_memory_mib = 1;
        configure_cluster(&mut settings, endpoints.clone(), certificate, 1);
        let cluster = ClusterClient::new(endpoints, CLUSTER_PSK).with_tls(&ca_pem, "localhost");
        let tier = settings.free_tier.clone();
        let server = TestHttpsServer::start_with_cluster(settings, "worker-limits", tier, cluster);

        let source = numeric_chain(2_000);
        let (status, body) = server.request_body(
            Some(&format!("Bearer {TOKEN}")),
            &json!({"source":source,"timeout_ms":1}).to_string(),
        );
        assert_eq!(status, 429, "{body}");
        assert!(body.contains("cpu_limit_exceeded"));

        let source = string_chain(1_600);
        let (status, body) = server.request_body(
            Some(&format!("Bearer {TOKEN}")),
            &json!({"source":source,"timeout_ms":50}).to_string(),
        );
        assert_eq!(status, 429);
        assert!(body.contains("memory_limit_exceeded"));
    }

    #[test]
    fn missing_and_invalid_api_keys_are_unauthorized() {
        let settings = settings();
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "test-tenant", settings.free_tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        assert_eq!(request(&gateway, None, "1").status, 401);
        assert_eq!(
            request(&gateway, Some("Bearer invalid-token"), "1").status,
            401
        );
    }

    #[test]
    fn enforces_monthly_quota_and_rate_limit() {
        let mut settings = settings();
        settings.config.rate_limit.free_max_requests_per_window = 1;
        let mut tier = settings.free_tier.clone();
        tier.api_requests_per_month = 2;
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings,
            vec![ApiKey::new(TOKEN, "quota-tenant", tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        assert_eq!(
            request(&gateway, Some(&format!("Bearer {TOKEN}")), "1").status,
            200
        );
        assert_eq!(
            request(&gateway, Some(&format!("Bearer {TOKEN}")), "2").status,
            429
        );
        assert!(
            request(&gateway, Some(&format!("Bearer {TOKEN}")), "2")
                .body
                .contains("rate_limited")
        );
        let metrics = health(&gateway);
        let metrics: serde_json::Value = serde_json::from_str(&metrics.body).unwrap();
        assert_eq!(metrics["metrics"]["rate_limit_rejections_total"], 2);
    }

    #[test]
    fn enforces_monthly_quota_independently_of_window_rate() {
        let mut settings = settings();
        settings.config.rate_limit.free_max_requests_per_window = 0;
        let mut tier = settings.free_tier.clone();
        tier.api_requests_per_month = 1;
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings,
            vec![ApiKey::new(TOKEN, "monthly-tenant", tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        assert_eq!(
            request(&gateway, Some(&format!("Bearer {TOKEN}")), "1").status,
            200
        );
        let response = request(&gateway, Some(&format!("Bearer {TOKEN}")), "2");
        assert_eq!(response.status, 429);
        assert!(response.body.contains("monthly_quota_exceeded"));
        let metrics = health(&gateway);
        let metrics: serde_json::Value = serde_json::from_str(&metrics.body).unwrap();
        assert_eq!(metrics["metrics"]["quota_rejections_total"], 1);
    }

    #[test]
    fn monthly_quota_persists_after_gateway_recreation() {
        let mut settings = settings();
        settings.config.rate_limit.free_max_requests_per_window = 0;
        let mut tier = settings.free_tier.clone();
        tier.api_requests_per_month = 1;
        let test_dir = TestDirectory::new();
        {
            let gateway = Gateway::new_with_state_path(
                settings.clone(),
                vec![ApiKey::new(TOKEN, "persistent-quota", tier.clone()).unwrap()],
                test_dir.state_path(),
            )
            .unwrap();
            assert_eq!(
                request(&gateway, Some(&format!("Bearer {TOKEN}")), "1").status,
                200
            );
        }
        let reloaded = Gateway::new_with_state_path(
            settings,
            vec![ApiKey::new(TOKEN, "persistent-quota", tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        let response = request(&reloaded, Some(&format!("Bearer {TOKEN}")), "1");
        assert_eq!(response.status, 429);
        assert!(response.body.contains("monthly_quota_exceeded"));
    }

    #[test]
    fn quota_state_lock_serializes_a_second_process() {
        const STATE_ENV: &str = "OMNI_GATEWAY_TEST_LOCK_STATE";
        const READY_ENV: &str = "OMNI_GATEWAY_TEST_LOCK_READY";

        if std::env::var_os(STATE_ENV).is_some() {
            let state_path = PathBuf::from(std::env::var_os(STATE_ENV).unwrap());
            let ready_path = PathBuf::from(std::env::var_os(READY_ENV).unwrap());
            fs::write(ready_path, b"ready").unwrap();
            let mut settings = settings();
            settings.config.rate_limit.free_max_requests_per_window = 0;
            let mut tier = settings.free_tier.clone();
            tier.api_requests_per_month = 1;
            let gateway = Gateway::new_with_state_path(
                settings,
                vec![ApiKey::new(TOKEN, "cross-process", tier).unwrap()],
                state_path,
            )
            .unwrap();
            let response = request(&gateway, Some(&format!("Bearer {TOKEN}")), "1");
            println!("child-status={}", response.status);
            assert_eq!(response.status, 429);
            assert!(response.body.contains("monthly_quota_exceeded"));
            return;
        }

        let mut settings = settings();
        settings.config.rate_limit.free_max_requests_per_window = 0;
        let mut tier = settings.free_tier.clone();
        tier.api_requests_per_month = 1;
        let test_dir = TestDirectory::new();
        let state_path = test_dir.state_path();
        let gateway = Gateway::new_with_state_path(
            settings,
            vec![ApiKey::new(TOKEN, "cross-process", tier).unwrap()],
            &state_path,
        )
        .unwrap();
        assert_eq!(
            request(&gateway, Some(&format!("Bearer {TOKEN}")), "1").status,
            200
        );

        let state_lock = lock_usage_state_file(&state_path).unwrap();
        let ready_path = test_dir.0.join("child-ready");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::quota_state_lock_serializes_a_second_process",
                "--nocapture",
            ])
            .env(STATE_ENV, &state_path)
            .env(READY_ENV, &ready_path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();

        let ready_deadline = Instant::now() + Duration::from_secs(5);
        while !ready_path.exists() && Instant::now() < ready_deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready_path.exists(), "child process did not become ready");
        std::thread::sleep(Duration::from_millis(150));
        assert!(
            child.try_wait().unwrap().is_none(),
            "child bypassed the state-file lock"
        );

        drop(state_lock);
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "child process failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("child-status=429"));
    }

    #[test]
    fn request_window_persists_after_gateway_recreation() {
        let mut settings = settings();
        settings.config.rate_limit.free_max_requests_per_window = 1;
        let tier = settings.free_tier.clone();
        let test_dir = TestDirectory::new();
        {
            let gateway = Gateway::new_with_state_path(
                settings.clone(),
                vec![ApiKey::new(TOKEN, "persistent-rate", tier.clone()).unwrap()],
                test_dir.state_path(),
            )
            .unwrap();
            assert_eq!(
                request(&gateway, Some(&format!("Bearer {TOKEN}")), "1").status,
                200
            );
        }
        let reloaded = Gateway::new_with_state_path(
            settings,
            vec![ApiKey::new(TOKEN, "persistent-rate", tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        let response = request(&reloaded, Some(&format!("Bearer {TOKEN}")), "1");
        assert_eq!(response.status, 429);
        assert!(response.body.contains("rate_limited"));
    }

    #[test]
    fn active_concurrency_is_reset_after_gateway_recreation() {
        let mut settings = settings();
        settings.config.rate_limit.free_max_requests_per_window = 0;
        settings.config.rate_limit.free_max_concurrent = 1;
        let tier = settings.free_tier.clone();
        let test_dir = TestDirectory::new();
        let first = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "restart-active", tier.clone()).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        let permit = first.admit(&first.keys[0]).unwrap();
        let reloaded = Gateway::new_with_state_path(
            settings,
            vec![ApiKey::new(TOKEN, "restart-active", tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        assert_eq!(
            request(&reloaded, Some(&format!("Bearer {TOKEN}")), "1").status,
            200
        );
        drop(permit);
    }

    #[test]
    fn persistence_io_errors_reject_admission() {
        let test_dir = TestDirectory::new();
        let blocked_parent = test_dir.0.join("state-parent");
        fs::create_dir(&blocked_parent).unwrap();
        let state_path = blocked_parent.join("usage-state.json");
        let settings = settings();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "io-error", settings.free_tier).unwrap()],
            &state_path,
        )
        .unwrap();
        fs::remove_file(usage_lock_path(&state_path)).unwrap();
        fs::remove_dir(&blocked_parent).unwrap();
        fs::write(&blocked_parent, "not a directory").unwrap();
        let response = request(&gateway, Some(&format!("Bearer {TOKEN}")), "1");
        assert_eq!(response.status, 503);
        assert!(response.body.contains("usage_state_unavailable"));
    }

    #[test]
    fn rejects_concurrency_limit_for_an_admitted_key() {
        let mut settings = settings();
        settings.config.rate_limit.free_max_concurrent = 1;
        let test_dir = TestDirectory::new();
        let gateway = Arc::new(
            Gateway::new_with_state_path(
                settings.clone(),
                vec![ApiKey::new(TOKEN, "concurrent-tenant", settings.free_tier).unwrap()],
                test_dir.state_path(),
            )
            .unwrap(),
        );
        let key = Arc::clone(&gateway.keys[0]);
        let _permit = gateway.admit(&key).unwrap();
        let response = request(&gateway, Some(&format!("Bearer {TOKEN}")), "1");
        assert_eq!(response.status, 429);
        assert!(response.body.contains("concurrency_limit_exceeded"));
        let metrics = health(&gateway);
        let metrics: serde_json::Value = serde_json::from_str(&metrics.body).unwrap();
        assert_eq!(metrics["metrics"]["concurrency_rejections_total"], 1);
    }

    #[test]
    fn rejects_unknown_route_and_bad_requests() {
        let settings = settings();
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "test-tenant", settings.free_tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        assert_eq!(
            gateway
                .handle("GET", "/v1/execute", &HashMap::new(), b"")
                .status,
            405
        );
        let mut headers = HashMap::new();
        headers.insert("Authorization".into(), format!("Bearer {TOKEN}"));
        assert_eq!(
            gateway.handle("POST", "/v1/nope", &headers, b"").status,
            404
        );
        assert_eq!(
            gateway.handle("POST", "/v1/execute", &headers, b"{").status,
            415
        );
        headers.insert("Content-Type".into(), "application/json".into());
        assert_eq!(
            gateway.handle("POST", "/v1/execute", &headers, b"{").status,
            400
        );
    }

    #[test]
    fn rejects_deterministic_verification_until_cluster_dispatch_is_available() {
        let settings = settings();
        let test_dir = TestDirectory::new();
        let gateway = Gateway::new_with_state_path(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "test-tenant", settings.free_tier).unwrap()],
            test_dir.state_path(),
        )
        .unwrap();
        let mut headers = HashMap::new();
        headers.insert("Authorization".into(), format!("Bearer {TOKEN}"));
        headers.insert("Content-Type".into(), "application/json".into());
        let response = gateway.handle(
            "POST",
            "/v1/execute",
            &headers,
            br#"{"source":"40 + 2","verification":"deterministic"}"#,
        );
        assert_eq!(response.status, 501);
        assert!(response.body.contains("verification_unavailable"));
    }

    #[test]
    fn https_executes_valid_authenticated_request() {
        let settings = settings();
        let tier = settings.free_tier.clone();
        let server = TestHttpsServer::start(settings, "tls-valid", tier);
        let (status, body) = server.request(Some(&format!("Bearer {TOKEN}")), "40 + 2");
        assert_eq!(status, 200);
        assert!(body.contains("\"number\":42.0"));
    }

    #[test]
    fn https_executes_selected_lua_runtime() {
        let settings = settings();
        let tier = settings.free_tier.clone();
        let server = TestHttpsServer::start(settings, "tls-lua", tier);
        let request = json!({
            "language":"lua",
            "source":"return \"বাংলা — 日本語 — 😀\""
        })
        .to_string();

        let (status, body) = server.request_body(Some(&format!("Bearer {TOKEN}")), &request);

        assert_eq!(status, 200, "{body}");
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["runtime"]["language"], "lua");
        assert_eq!(body["value"]["text"], "বাংলা — 日本語 — 😀");
    }

    #[test]
    fn https_rejects_missing_and_invalid_credentials() {
        let settings = settings();
        let tier = settings.free_tier.clone();
        let server = TestHttpsServer::start(settings, "tls-auth", tier);
        assert_eq!(server.request(None, "1").0, 401);
        assert_eq!(server.request(Some("Bearer invalid-token"), "1").0, 401);
    }

    #[test]
    fn https_enforces_rate_and_monthly_quota_limits() {
        let mut rate_settings = settings();
        rate_settings.config.rate_limit.free_max_requests_per_window = 1;
        let rate_tier = rate_settings.free_tier.clone();
        let rate_server = TestHttpsServer::start(rate_settings, "tls-rate", rate_tier);
        assert_eq!(
            rate_server.request(Some(&format!("Bearer {TOKEN}")), "1").0,
            200
        );
        let (status, body) = rate_server.request(Some(&format!("Bearer {TOKEN}")), "1");
        assert_eq!(status, 429);
        assert!(body.contains("rate_limited"));
        drop(rate_server);

        let mut quota_settings = settings();
        quota_settings
            .config
            .rate_limit
            .free_max_requests_per_window = 0;
        let mut quota_tier = quota_settings.free_tier.clone();
        quota_tier.api_requests_per_month = 1;
        let quota_server = TestHttpsServer::start(quota_settings, "tls-quota", quota_tier);
        assert_eq!(
            quota_server
                .request(Some(&format!("Bearer {TOKEN}")), "1")
                .0,
            200
        );
        let (status, body) = quota_server.request(Some(&format!("Bearer {TOKEN}")), "1");
        assert_eq!(status, 429);
        assert!(body.contains("monthly_quota_exceeded"));
    }
}
