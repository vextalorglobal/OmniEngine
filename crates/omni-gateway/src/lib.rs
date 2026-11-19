//! Managed REST API gateway for policy-bounded OmniEngine execution.

use omni_core::{DEFAULT_STEP_LIMIT, Engine, ErrorKind, Value};
use ring::digest::{SHA256, digest};
use rustls::ServerConfig;
use rustls::pki_types::PrivateKeyDer;
use rustls_pemfile::{certs, private_key};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_OVERHEAD: usize = 4096;

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
    pub execute_endpoint: String,
    pub health_endpoint: String,
    pub rate_limit: RateLimitConfig,
    pub resource_limits: ResourceLimitConfig,
    pub sandbox: SandboxConfig,
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
        Self::parse(&policy_text, &config_text)
    }

    pub fn parse(policy_text: &str, config_text: &str) -> Result<Self, String> {
        let policy: TierPolicyFile =
            toml::from_str(policy_text).map_err(|e| format!("invalid tier policy: {e}"))?;
        let example: ExampleConfig =
            toml::from_str(config_text).map_err(|e| format!("invalid cloud config: {e}"))?;
        let config = example.gateway;
        if policy.gateway.rest_execute_endpoint != config.execute_endpoint {
            return Err("REST execute endpoint differs between policy and config".to_string());
        }
        if policy.gateway.tls_required
            && (config.tls_cert.as_os_str().is_empty() || config.tls_key.as_os_str().is_empty())
        {
            return Err("TLS is required but certificate or key path is empty".to_string());
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
    gas_limit: Option<u64>,
    timeout_ms: Option<u64>,
    verification: Option<String>,
}

struct KeyUsage {
    month: i64,
    monthly_requests: u64,
    window_started: Instant,
    window_requests: u64,
    active: usize,
}

struct KeyState {
    digest: Vec<u8>,
    tenant_id: String,
    limits: TierPolicy,
    usage: Mutex<KeyUsage>,
}

struct Permit(Arc<KeyState>);

impl Drop for Permit {
    fn drop(&mut self) {
        if let Ok(mut usage) = self.0.usage.lock() {
            usage.active = usage.active.saturating_sub(1);
        }
    }
}

pub struct Gateway {
    settings: GatewaySettings,
    keys: Vec<Arc<KeyState>>,
    next_request: AtomicU64,
}

impl Gateway {
    pub fn new(settings: GatewaySettings, api_keys: Vec<ApiKey>) -> Result<Self, String> {
        if api_keys.is_empty() {
            return Err("at least one API key must be provisioned".to_string());
        }
        let now = Instant::now();
        let month = current_month();
        let keys = api_keys
            .into_iter()
            .map(|key| {
                Arc::new(KeyState {
                    digest: key.token_digest,
                    tenant_id: key.tenant_id,
                    limits: key.limits,
                    usage: Mutex::new(KeyUsage {
                        month,
                        monthly_requests: 0,
                        window_started: now,
                        window_requests: 0,
                        active: 0,
                    }),
                })
            })
            .collect();
        Ok(Self {
            settings,
            keys,
            next_request: AtomicU64::new(1),
        })
    }

    pub fn settings(&self) -> &GatewaySettings {
        &self.settings
    }

    pub fn handle(
        &self,
        method: &str,
        path: &str,
        headers: &HashMap<String, String>,
        body: &[u8],
    ) -> HttpResponse {
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
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
        if let Some(verification) = &request.verification {
            if verification != "deterministic" {
                return HttpResponse::error(
                    400,
                    "invalid_request",
                    "unsupported verification mode",
                    None,
                );
            }
            return HttpResponse::error(
                501,
                "verification_unavailable",
                "deterministic verification is not available in this gateway",
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
        let permit = match self.admit(&key) {
            Ok(permit) => permit,
            Err(AdmissionError::Quota) => {
                return HttpResponse::error(
                    429,
                    "monthly_quota_exceeded",
                    "monthly API request quota exceeded",
                    None,
                );
            }
            Err(AdmissionError::Rate) => {
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
                return HttpResponse::error(
                    429,
                    "concurrency_limit_exceeded",
                    "maximum concurrent requests reached",
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
        let max_cpu = Duration::from_millis(timeout);
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
        let mut engine = Engine::new();
        engine.set_step_limit(max_steps);
        let started = Instant::now();
        let execution = engine.exec(&request.source);
        let elapsed = started.elapsed();
        let stats = engine.stats();
        let peak_memory = stats
            .arena_bytes_reserved
            .saturating_add(stats.egress_bytes_reserved);
        if elapsed > max_cpu {
            return HttpResponse::error(
                429,
                "cpu_limit_exceeded",
                "execution exceeded the request CPU budget",
                None,
            );
        }
        if peak_memory > max_memory.saturating_mul(1024 * 1024) {
            return HttpResponse::error(
                429,
                "memory_limit_exceeded",
                "execution exceeded the sandbox memory budget",
                None,
            );
        }
        match execution {
            Ok(value) => self.success(request_id, &key, &engine, value, elapsed, peak_memory),
            Err(error) => {
                let (status, code) = match error.kind {
                    ErrorKind::Lex | ErrorKind::Parse => (400, "compile_error"),
                    _ => (422, "runtime_error"),
                };
                HttpResponse {
                    status,
                    body: json!({
                        "request_id": format!("req_{request_id}"),
                        "status": "error",
                        "error": {"code":code,"message":error.to_string()}
                    })
                    .to_string(),
                    retry_after_seconds: None,
                }
            }
        }
    }

    fn success(
        &self,
        request_id: u64,
        key: &KeyState,
        engine: &Engine,
        value: Value,
        elapsed: Duration,
        peak_memory: u64,
    ) -> HttpResponse {
        let rendered = engine.render(value);
        if rendered.len() > self.settings.config.resource_limits.max_output_bytes {
            return HttpResponse::error(
                413,
                "output_too_large",
                "result exceeds configured output limit",
                None,
            );
        }
        let mut value_json = json!({"tag": value_tag(value), "text": rendered});
        if let Some(number) = value.as_f64() {
            value_json["number"] = json!(number);
        } else if let Some(boolean) = value.as_bool() {
            value_json["boolean"] = json!(boolean);
        }
        HttpResponse {
            status: 200,
            body: json!({
                "request_id":format!("req_{request_id}"),
                "tenant_id":key.tenant_id,
                "status":"ok",
                "value":value_json,
                "usage":{
                    "cpu_ms":elapsed.as_millis(),
                    "peak_memory_bytes":peak_memory
                }
            })
            .to_string(),
            retry_after_seconds: None,
        }
    }

    fn admit(&self, key: &Arc<KeyState>) -> Result<Permit, AdmissionError> {
        let mut usage = key.usage.lock().map_err(|_| AdmissionError::Internal)?;
        let month = current_month();
        if usage.month != month {
            usage.month = month;
            usage.monthly_requests = 0;
        }
        if usage.monthly_requests >= key.limits.api_requests_per_month {
            return Err(AdmissionError::Quota);
        }
        let window = Duration::from_secs(self.settings.config.rate_limit.free_rate_window_seconds);
        if window.is_zero() || usage.window_started.elapsed() >= window {
            usage.window_started = Instant::now();
            usage.window_requests = 0;
        }
        let rate_limit = self.settings.config.rate_limit.free_max_requests_per_window;
        if rate_limit > 0 && usage.window_requests >= rate_limit {
            return Err(AdmissionError::Rate);
        }
        let max_concurrent = key
            .limits
            .max_concurrent_requests
            .min(self.settings.config.rate_limit.free_max_concurrent);
        if max_concurrent > 0 && usage.active >= max_concurrent {
            return Err(AdmissionError::Concurrency);
        }
        usage.monthly_requests += 1;
        usage.window_requests += 1;
        usage.active += 1;
        drop(usage);
        Ok(Permit(Arc::clone(key)))
    }

    /// Serve TLS-protected HTTP/1.1 connections. TLS termination is mandatory
    /// when the loaded policy declares `tls_required`.
    pub fn serve(self: Arc<Self>) -> io::Result<()> {
        if !self.settings.tls_required {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "gateway requires TLS",
            ));
        }
        let tls = Arc::new(load_tls_config(
            &self.settings.config.tls_cert,
            &self.settings.config.tls_key,
        )?);
        let listener = TcpListener::bind(&self.settings.config.bind)?;
        for incoming in listener.incoming() {
            match incoming {
                Ok(stream) => {
                    let gateway = Arc::clone(&self);
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
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdmissionError {
    Quota,
    Rate,
    Concurrency,
    Internal,
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

fn value_tag(value: Value) -> &'static str {
    if value.is_string() {
        "string"
    } else if value.as_f64().is_some() {
        "number"
    } else if value.as_bool().is_some() {
        "boolean"
    } else if value == Value::UNDEF {
        "undefined"
    } else {
        "null"
    }
}

fn load_tls_config(cert_path: &Path, key_path: &Path) -> io::Result<ServerConfig> {
    let mut cert_reader = BufReader::new(File::open(cert_path)?);
    let certificates = certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if certificates.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TLS certificate file is empty",
        ));
    }
    let mut key_reader = BufReader::new(File::open(key_path)?);
    let key: PrivateKeyDer<'static> = private_key(&mut key_reader)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "TLS key file contains no private key",
            )
        })?;
    ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
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
    let mut reader = BufReader::new(&mut stream);
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
    let content_length_header = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.as_str());
    let content_length = match content_length_header {
        Some(value) => match value.parse::<usize>() {
            Ok(length) => length,
            Err(_) => {
                return write_http_response(
                    reader.get_mut(),
                    HttpResponse::error(400, "bad_request", "invalid Content-Length", None),
                );
            }
        },
        None => 0,
    };
    let max_body = gateway.settings.config.resource_limits.max_source_bytes + MAX_BODY_OVERHEAD;
    if content_length > max_body {
        return write_http_response(
            reader.get_mut(),
            HttpResponse::error(
                413,
                "payload_too_large",
                "request body exceeds configured limit",
                None,
            ),
        );
    }
    let mut body = vec![0; content_length];
    reader.read_exact(&mut body)?;
    let response = gateway.handle(method, path, &headers, &body);
    write_http_response(reader.get_mut(), response)
}

fn write_http_response(stream: &mut impl Write, response: HttpResponse) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
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
    write!(stream, "\r\n{}", response.body)?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = include_str!("../../../policies/usage-tiers.toml");
    const CONFIG: &str = include_str!("../../../config/cloud-api.example.toml");
    const TOKEN: &str = "omni_test_key_which_is_long_enough_01";

    fn settings() -> GatewaySettings {
        GatewaySettings::parse(POLICY, CONFIG).unwrap()
    }

    fn request(gateway: &Gateway, auth: Option<&str>, source: &str) -> HttpResponse {
        let mut headers = HashMap::new();
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        if let Some(auth) = auth {
            headers.insert("Authorization".to_string(), auth.to_string());
        }
        let body = json!({"source":source}).to_string();
        gateway.handle("POST", "/v1/execute", &headers, body.as_bytes())
    }

    #[test]
    fn parses_and_validates_free_tier_and_gateway_config() {
        let settings = settings();
        assert_eq!(settings.free_tier.api_requests_per_month, 100_000);
        assert_eq!(settings.free_tier.cpu_ms_per_request, 50);
        assert_eq!(settings.free_tier.sandbox_memory_mib, 128);
        assert_eq!(settings.free_tier.max_concurrent_requests, 10);
        assert_eq!(settings.config.execute_endpoint, "/v1/execute");
    }

    #[test]
    fn rejects_policy_and_config_drift() {
        let changed = CONFIG.replace("free_max_concurrent = 10", "free_max_concurrent = 11");
        assert!(GatewaySettings::parse(POLICY, &changed).is_err());
    }

    #[test]
    fn executes_valid_sandbox_request() {
        let settings = settings();
        let gateway = Gateway::new(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "test-tenant", settings.free_tier).unwrap()],
        )
        .unwrap();
        let response = request(&gateway, Some(&format!("Bearer {TOKEN}")), "40 + 2");
        assert_eq!(response.status, 200);
        assert!(response.body.contains("\"number\":42.0"));
        assert!(response.body.contains("\"tenant_id\":\"test-tenant\""));
    }

    #[test]
    fn missing_and_invalid_api_keys_are_unauthorized() {
        let settings = settings();
        let gateway = Gateway::new(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "test-tenant", settings.free_tier).unwrap()],
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
        let gateway = Gateway::new(
            settings,
            vec![ApiKey::new(TOKEN, "quota-tenant", tier).unwrap()],
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
    }

    #[test]
    fn enforces_monthly_quota_independently_of_window_rate() {
        let mut settings = settings();
        settings.config.rate_limit.free_max_requests_per_window = 0;
        let mut tier = settings.free_tier.clone();
        tier.api_requests_per_month = 1;
        let gateway = Gateway::new(
            settings,
            vec![ApiKey::new(TOKEN, "monthly-tenant", tier).unwrap()],
        )
        .unwrap();
        assert_eq!(
            request(&gateway, Some(&format!("Bearer {TOKEN}")), "1").status,
            200
        );
        let response = request(&gateway, Some(&format!("Bearer {TOKEN}")), "2");
        assert_eq!(response.status, 429);
        assert!(response.body.contains("monthly_quota_exceeded"));
    }

    #[test]
    fn rejects_concurrency_limit_for_an_admitted_key() {
        let mut settings = settings();
        settings.config.rate_limit.free_max_concurrent = 1;
        let gateway = Arc::new(
            Gateway::new(
                settings.clone(),
                vec![ApiKey::new(TOKEN, "concurrent-tenant", settings.free_tier).unwrap()],
            )
            .unwrap(),
        );
        let key = Arc::clone(&gateway.keys[0]);
        let _permit = gateway.admit(&key).unwrap();
        let response = request(&gateway, Some(&format!("Bearer {TOKEN}")), "1");
        assert_eq!(response.status, 429);
        assert!(response.body.contains("concurrency_limit_exceeded"));
    }

    #[test]
    fn rejects_unknown_route_and_bad_requests() {
        let settings = settings();
        let gateway = Gateway::new(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "test-tenant", settings.free_tier).unwrap()],
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
        let gateway = Gateway::new(
            settings.clone(),
            vec![ApiKey::new(TOKEN, "test-tenant", settings.free_tier).unwrap()],
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
}
