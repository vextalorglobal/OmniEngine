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
    let raw_keys = std::env::var(variable).map_err(|_| {
        format!(
            "required API key environment variable {variable} is not set; provide tenant:key entries. For local development, run .\\scripts\\dev-up.ps1."
        )
    })?;
    if raw_keys.trim().is_empty() {
        return Err(format!(
            "{variable} is empty; provide comma-separated tenant:key entries. For local development, run .\\scripts\\dev-up.ps1."
        ));
    }
    let keys = raw_keys
        .split(',')
        .enumerate()
        .map(|(index, entry)| {
            let entry = entry.trim();
            let (tenant, token) = entry.split_once(':').ok_or_else(|| {
                format!(
                    "{variable} entry {} must use tenant:key format (for example local:<token>)",
                    index + 1
                )
            })?;
            if tenant.trim().is_empty() || token.trim().is_empty() {
                return Err(format!(
                    "{variable} entry {} has an empty tenant or key; expected tenant:key",
                    index + 1
                ));
            }
            ApiKey::new(token.trim(), tenant.trim(), settings.free_tier.clone()).map_err(|error| {
                format!(
                    "{variable} entry {} contains an invalid API key: {error}",
                    index + 1
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let gateway = Arc::new(Gateway::new(settings, keys)?);
    gateway
        .serve()
        .map_err(|e| format!("gateway listener failed: {e}"))
}
