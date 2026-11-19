use omni_gateway::{ApiKey, Gateway, GatewaySettings};
use std::path::Path;
use std::sync::Arc;

fn main() {
    if let Err(error) = run() {
        eprintln!("omni-gateway: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let policy = std::env::var_os("OMNI_USAGE_POLICY")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("policies/usage-tiers.toml"));
    let config = std::env::var_os("OMNI_GATEWAY_CONFIG")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| root.join("config/cloud-api.example.toml"));
    let settings = GatewaySettings::load(&policy, &config)?;
    let variable = settings
        .auth
        .token_secret_source
        .strip_prefix("env:")
        .filter(|name| !name.is_empty())
        .ok_or_else(|| {
            "gateway currently supports API key sources using env:VARIABLE".to_string()
        })?;
    let raw_keys = std::env::var(variable)
        .map_err(|_| format!("{variable} must contain tenant:key entries in the secret store"))?;
    let keys = raw_keys
        .split(',')
        .map(|entry| {
            let (tenant, token) = entry
                .split_once(':')
                .ok_or_else(|| "OMNI_API_KEYS entries must use tenant:key format".to_string())?;
            ApiKey::new(token, tenant, settings.free_tier.clone())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let gateway = Arc::new(Gateway::new(settings, keys)?);
    gateway
        .serve()
        .map_err(|e| format!("gateway listener failed: {e}"))
}
