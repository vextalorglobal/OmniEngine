//! Fetching, verifying and installing a newer `omni`.
//!
//! The engine itself never touches the network. It parses a manifest and
//! compares versions ([`omni_core::update`]); everything that moves bytes lives
//! here, in the CLI, where a user can see what was downloaded and where it went.
//!
//! The rules this module keeps:
//!
//! * HTTPS only, unless [`ALLOW_INSECURE_ENV`] is set. A release check that can
//!   be redirected to plain HTTP is an upgrade path an attacker on the network
//!   can choose for you.
//! * Every downloaded binary is checked against the SHA-256 the manifest names
//!   before it is written anywhere. A mismatched download is refused, not
//!   quarantined and not retried blindly.
//! * Replacing a binary is a rename dance, never an in-place overwrite: write
//!   `<name>.new`, move the old file to `<name>.bak`, move the new file into
//!   place, and put the backup back if that last move fails. On Windows the
//!   running image cannot be deleted but can be renamed, which is why this
//!   works for `omni --update` updating the binary that is executing.
//! * Nothing is installed without consent. `--yes` is that consent, and without
//!   it the interactive prompt has to be answered.

use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use omni_core::update::{
    self, ReleaseAsset, ReleaseManifest, UpdateError as ManifestError, UpdateStatus,
};

pub mod sha256;

/// Where a release check looks when nothing else is said.
pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/fahim5536/omniengine/releases/latest/download/omni-update.json";

/// Names another manifest source: a URL or a `file://` path.
pub const MANIFEST_URL_ENV: &str = "OMNI_UPDATE_URL";

/// Set to `1` to permit plain HTTP manifest and asset URLs.
pub const ALLOW_INSECURE_ENV: &str = "OMNI_ALLOW_INSECURE";

/// The ABI this build speaks. Kept next to the engine's constant by
/// `scripts/check-versions.py` so a release cannot ship two answers.
pub const ABI_VERSION: u32 = 5;

/// The version of the running binary, and the reference a manifest is compared
/// against. `omni` and `omniup` are built from this crate and released
/// together, so their version is the version of the tool being replaced.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `check-update` exit code: a newer release exists.
pub const EXIT_UPDATE_AVAILABLE: u8 = 10;

/// `check-update` exit code: the release speaks a different ABI.
pub const EXIT_ABI_MISMATCH: u8 = 11;

/// The exit code a check's verdict maps to.
pub fn status_exit_code(status: UpdateStatus) -> u8 {
    match status {
        UpdateStatus::UpToDate => 0,
        UpdateStatus::Available => EXIT_UPDATE_AVAILABLE,
        UpdateStatus::AbiMismatch => EXIT_ABI_MISMATCH,
    }
}

/// One line a check can print, and the reason its exit code is what it is.
pub fn check_summary(outcome: &CheckOutcome) -> String {
    match outcome.status {
        UpdateStatus::UpToDate => {
            format!("omni {CURRENT_VERSION} is up to date (ABI {ABI_VERSION})")
        }
        UpdateStatus::Available => format!(
            "omni {} is available; this build is {CURRENT_VERSION}",
            outcome.manifest.version
        ),
        UpdateStatus::AbiMismatch => format!(
            "release {} speaks ABI {}; this build speaks ABI {ABI_VERSION}",
            outcome.manifest.version, outcome.manifest.abi
        ),
    }
}

/// How large a downloaded engine binary may be. Releases are a few megabytes;
/// this bounds a hostile or broken server rather than a real build.
pub const MAX_BINARY_LEN: usize = 128 * 1024 * 1024;

/// How long curl or wget may spend on one transfer.
const FETCH_TIMEOUT_SECS: &str = "60";

/// Everything that can go wrong between "check for an update" and "the new
/// binary is in place".
#[derive(Debug)]
pub enum UpdateError {
    /// The command line did not make sense.
    Usage(String),
    /// The manifest or binary could not be fetched.
    Fetch(String),
    /// A remote address that is not HTTPS, with no opt-in.
    InsecureUrl(String),
    /// The manifest was fetched but is not a release manifest.
    Manifest(ManifestError),
    /// The downloaded bytes are not the bytes the manifest named.
    Checksum { expected: String, actual: String },
    /// This platform has no build in the release.
    NotPublished { platform: String, version: String },
    /// The running platform has no release name at all.
    UnsupportedPlatform(String),
    /// The file could not be written or moved into place.
    Install(String),
    /// The user declined at the prompt.
    Cancelled,
}

impl UpdateError {
    /// The exit code `omni` and `omniup` use for this failure.
    pub fn exit_code(&self) -> u8 {
        match self {
            UpdateError::Usage(_) => 2,
            UpdateError::Fetch(_)
            | UpdateError::InsecureUrl(_)
            | UpdateError::Manifest(_)
            | UpdateError::NotPublished { .. }
            | UpdateError::UnsupportedPlatform(_)
            | UpdateError::Cancelled => 2,
            UpdateError::Checksum { .. } | UpdateError::Install(_) => 3,
        }
    }
}

impl fmt::Display for UpdateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UpdateError::Usage(message) => write!(f, "{message}"),
            UpdateError::Fetch(message) => write!(f, "could not fetch the update: {message}"),
            UpdateError::InsecureUrl(url) => write!(
                f,
                "refusing plain HTTP for '{url}'; use https, or set {ALLOW_INSECURE_ENV}=1 to allow it"
            ),
            UpdateError::Manifest(error) => write!(f, "{error}"),
            UpdateError::Checksum { expected, actual } => write!(
                f,
                "checksum mismatch: the manifest says {expected}, the download is {actual}; nothing was installed"
            ),
            UpdateError::NotPublished { platform, version } => write!(
                f,
                "release {version} has no binary for {platform}; install it from the release page instead"
            ),
            UpdateError::UnsupportedPlatform(what) => {
                write!(f, "no release assets are published for {what}")
            }
            UpdateError::Install(message) => write!(f, "could not install the update: {message}"),
            UpdateError::Cancelled => write!(f, "update cancelled; nothing was changed"),
        }
    }
}

impl std::error::Error for UpdateError {}

impl From<ManifestError> for UpdateError {
    fn from(error: ManifestError) -> Self {
        UpdateError::Manifest(error)
    }
}

/// What a check found.
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    /// Up to date, available, or speaking a different ABI.
    pub status: UpdateStatus,
    /// The parsed manifest, with the version that was compared.
    pub manifest: ReleaseManifest,
    /// Where the manifest came from.
    pub source: String,
    /// This host's release key, when it has one.
    pub platform: Option<&'static str>,
    /// The binary entry for this host, when the release has one.
    pub asset: Option<ReleaseAsset>,
}

/// The manifest source: `--manifest`, then `OMNI_UPDATE_URL`, then the default
/// release address.
pub fn default_manifest_source() -> String {
    std::env::var(MANIFEST_URL_ENV)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_MANIFEST_URL.to_string())
}

/// Fetch a manifest and compare it with this build.
pub fn check(source: &str) -> Result<CheckOutcome, UpdateError> {
    let bytes = fetch(source, update::MAX_MANIFEST_LEN)?;
    let text = String::from_utf8(bytes).map_err(|_| {
        UpdateError::Manifest(ManifestError::Json(
            "the manifest is not valid UTF-8".to_string(),
        ))
    })?;
    let manifest = update::parse_manifest(&text)?;
    let status = update::check_update(CURRENT_VERSION, ABI_VERSION, &manifest);
    let platform = update::host_platform();
    let asset = platform.and_then(|key| manifest.asset_for(key)).cloned();
    Ok(CheckOutcome {
        status,
        manifest,
        source: source.to_string(),
        platform,
        asset,
    })
}

/// Fetch `source`, failing if it is larger than `limit`.
///
/// `file://` is read directly, so a local mirror and the verification tests do
/// not need a server. Everything else goes through curl, or wget when curl is
/// not installed. Plain HTTP is refused unless [`ALLOW_INSECURE_ENV`] is set.
pub fn fetch(source: &str, limit: usize) -> Result<Vec<u8>, UpdateError> {
    if source.starts_with("file://") {
        let path = file_url_path(source)
            .ok_or_else(|| UpdateError::Fetch(format!("'{source}' is not a local file URL")))?;
        return read_limited(&path, limit);
    }
    fetch_remote(source, limit)
}

fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>, UpdateError> {
    let file = fs::File::open(path)
        .map_err(|e| UpdateError::Fetch(format!("cannot open '{}': {e}", path.display())))?;
    let mut bytes = Vec::new();
    // Read one byte past the limit so "exactly at the limit" and "over it" are
    // distinguishable without a second stat that could race.
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| UpdateError::Fetch(format!("cannot read '{}': {e}", path.display())))?;
    if bytes.len() > limit {
        return Err(UpdateError::Fetch(format!(
            "'{}' is larger than the {limit}-byte limit",
            path.display()
        )));
    }
    Ok(bytes)
}

fn fetch_remote(url: &str, limit: usize) -> Result<Vec<u8>, UpdateError> {
    let https = url.starts_with("https://");
    if !https {
        if !url.starts_with("http://") {
            return Err(UpdateError::Fetch(format!(
                "unsupported URL scheme in '{url}'; expected https:// or file://"
            )));
        }
        if !env_flag(ALLOW_INSECURE_ENV) {
            return Err(UpdateError::InsecureUrl(url.to_string()));
        }
    }

    let protocols = if https { "=https" } else { "=http,=https" };
    let limit_arg = limit.to_string();
    let output = match Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            FETCH_TIMEOUT_SECS,
            "--proto",
            protocols,
            "--proto-redir",
            protocols,
            "--max-filesize",
            &limit_arg,
            url,
        ])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return fetch_with_wget(url, limit);
        }
        Err(error) => {
            return Err(UpdateError::Fetch(format!("could not run curl: {error}")));
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        return Err(UpdateError::Fetch(if stderr.is_empty() {
            format!("curl exited with {}", output.status)
        } else {
            stderr.to_string()
        }));
    }
    check_len(output.stdout, limit, url)
}

fn fetch_with_wget(url: &str, limit: usize) -> Result<Vec<u8>, UpdateError> {
    // wget has no reliable size cap, so it writes to a scratch file first and
    // the size is checked before anything is read into memory.
    let scratch = std::env::temp_dir().join(format!(
        "omni-fetch-{}-{}.part",
        std::process::id(),
        unique_suffix()
    ));
    let output = Command::new("wget")
        .args(["-q", "--timeout", FETCH_TIMEOUT_SECS, "--tries", "1", "-O"])
        .arg(&scratch)
        .arg(url)
        .output();
    let result = match output {
        Ok(output) if output.status.success() => read_limited(&scratch, limit),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stderr = stderr.trim();
            Err(UpdateError::Fetch(if stderr.is_empty() {
                format!("wget exited with {}", output.status)
            } else {
                stderr.to_string()
            }))
        }
        Err(error) => Err(UpdateError::Fetch(format!(
            "neither curl nor wget could be run: {error}"
        ))),
    };
    let _ = fs::remove_file(&scratch);
    result
}

fn check_len(bytes: Vec<u8>, limit: usize, source: &str) -> Result<Vec<u8>, UpdateError> {
    if bytes.len() > limit {
        return Err(UpdateError::Fetch(format!(
            "'{source}' is larger than the {limit}-byte limit"
        )));
    }
    Ok(bytes)
}

/// Download the asset and verify its SHA-256 before returning a byte.
pub fn download_verified(asset: &ReleaseAsset) -> Result<Vec<u8>, UpdateError> {
    let bytes = fetch(&asset.url, MAX_BINARY_LEN)?;
    let actual = sha256::hex_digest(&bytes);
    if actual != asset.sha256 {
        return Err(UpdateError::Checksum {
            expected: asset.sha256.clone(),
            actual,
        });
    }
    Ok(bytes)
}

/// What [`run_update`] did.
#[derive(Debug)]
pub struct UpdateReport {
    /// The version this tool was built as.
    pub from: String,
    /// The version the release names.
    pub to: String,
    /// Up to date, available, or ABI mismatch -- before any download.
    pub status: UpdateStatus,
    /// The file that was (or would be) replaced.
    pub target: PathBuf,
    /// Whether the target was actually replaced.
    pub replaced: bool,
    /// The old binary, when it could not be deleted after a successful replace
    /// (Windows keeps a running image alive).
    pub backup: Option<PathBuf>,
}

/// The line an update command prints when it finishes, and the exit code it
/// exits with. An ABI mismatch belongs on stderr, so [`EXIT_ABI_MISMATCH`]
/// tells the caller which stream to use.
pub fn update_summary(report: &UpdateReport) -> (u8, String) {
    match report.status {
        UpdateStatus::UpToDate => (0, format!("omni {} is already up to date", report.from)),
        UpdateStatus::AbiMismatch => (
            EXIT_ABI_MISMATCH,
            format!(
                "release {} speaks a different ABI than this build ({ABI_VERSION}); nothing was installed",
                report.to
            ),
        ),
        // A dry run answers the same question `check-update` does -- is there an
        // update? -- so it uses the same exit code. Calling it a success (0)
        // would make `--dry-run` indistinguishable from "already current" to a
        // script that only reads the code.
        UpdateStatus::Available if !report.replaced => (
            EXIT_UPDATE_AVAILABLE,
            format!(
                "would replace {} with omni {}",
                report.target.display(),
                report.to
            ),
        ),
        UpdateStatus::Available => {
            let mut text = format!(
                "updated {} from {} to {}",
                report.target.display(),
                report.from,
                report.to
            );
            if let Some(backup) = &report.backup {
                text.push_str(&format!(
                    "; the previous binary is at {} and can be deleted once you are satisfied",
                    backup.display()
                ));
            }
            (0, text)
        }
    }
}

/// One update attempt, as the command line describes it.
#[derive(Debug)]
pub struct UpdateOptions {
    pub source: String,
    pub target: PathBuf,
    /// Consent given on the command line.
    pub assume_yes: bool,
    /// Check and stop before downloading.
    pub dry_run: bool,
}

impl UpdateOptions {
    pub fn new(target: impl Into<PathBuf>) -> Self {
        UpdateOptions {
            source: default_manifest_source(),
            target: target.into(),
            assume_yes: false,
            dry_run: false,
        }
    }
}

/// Check, consent, download, verify, install.
///
/// Returns a report rather than an error when there is nothing to do: "already
/// up to date" is a successful check. An ABI mismatch is also reported, with
/// `replaced` false -- upgrading across a wire-format change is not something
/// this tool will do behind the user's back.
pub fn run_update(options: &UpdateOptions) -> Result<UpdateReport, UpdateError> {
    let outcome = check(&options.source)?;
    let mut report = UpdateReport {
        from: CURRENT_VERSION.to_string(),
        to: outcome.manifest.version.clone(),
        status: outcome.status,
        target: options.target.clone(),
        replaced: false,
        backup: None,
    };
    if outcome.status != UpdateStatus::Available {
        return Ok(report);
    }

    let platform = outcome
        .platform
        .ok_or_else(|| UpdateError::UnsupportedPlatform(host_description()))?;
    let asset = outcome
        .asset
        .clone()
        .ok_or_else(|| UpdateError::NotPublished {
            platform: platform.to_string(),
            version: outcome.manifest.version.clone(),
        })?;
    if options.dry_run {
        return Ok(report);
    }

    if !options.assume_yes {
        let consent = confirm(&consent_prompt(&options.target, &outcome)).map_err(|e| {
            UpdateError::Usage(format!("cannot read the confirmation from stdin: {e}"))
        })?;
        if !consent {
            return Err(UpdateError::Cancelled);
        }
    }

    let bytes = download_verified(&asset)?;
    report.backup = install(&options.target, &bytes)?;
    report.replaced = true;
    Ok(report)
}

fn consent_prompt(target: &Path, outcome: &CheckOutcome) -> String {
    let mut prompt = format!(
        "replace {} ({CURRENT_VERSION}) with {}?",
        target.display(),
        outcome.manifest.version
    );
    if let Some(notes) = &outcome.manifest.notes {
        prompt.push_str(&format!("\n{notes}"));
    }
    prompt
}

fn confirm(prompt: &str) -> io::Result<bool> {
    let mut stderr = io::stderr();
    writeln!(stderr, "{prompt}")?;
    write!(stderr, "Continue? [y/N] ")?;
    stderr.flush()?;
    let mut line = String::new();
    if io::stdin().read_line(&mut line)? == 0 {
        return Ok(false);
    }
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Replace `target` with `bytes`, atomically as far as the platform allows.
///
/// The new bytes land in a sibling `.new` file, the old binary moves aside to
/// `.bak`, and only then does the new file take the name. A failure at the last
/// rename restores the backup. The backup is deleted afterwards when the
/// platform allows it; a Windows process updating itself cannot delete the
/// image it is still running from, so the `.bak` is left behind and reported.
pub fn install(target: &Path, bytes: &[u8]) -> Result<Option<PathBuf>, UpdateError> {
    let name = target
        .file_name()
        .ok_or_else(|| UpdateError::Install(format!("'{}' has no file name", target.display())))?
        .to_owned();
    let parent = match target.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let temp = parent.join(format!(".{}.new", name.to_string_lossy()));
    let backup = parent.join(format!("{}.bak", name.to_string_lossy()));

    {
        let mut file = fs::File::create(&temp).map_err(|e| {
            UpdateError::Install(format!("cannot create '{}': {e}", temp.display()))
        })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| UpdateError::Install(format!("cannot write '{}': {e}", temp.display())))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&temp, fs::Permissions::from_mode(0o755));
    }

    let had_target = target.exists();
    if had_target {
        // A stale backup from an earlier self-update is ours to replace.
        let _ = fs::remove_file(&backup);
        if let Err(error) = fs::rename(target, &backup) {
            let _ = fs::remove_file(&temp);
            return Err(UpdateError::Install(format!(
                "cannot move '{}' aside: {error}",
                target.display()
            )));
        }
    }

    if let Err(error) = fs::rename(&temp, target) {
        if had_target {
            let _ = fs::rename(&backup, target);
        }
        let _ = fs::remove_file(&temp);
        return Err(UpdateError::Install(format!(
            "cannot move the new binary into '{}': {error}",
            target.display()
        )));
    }

    if had_target && fs::remove_file(&backup).is_err() {
        return Ok(Some(backup));
    }
    Ok(None)
}

fn host_description() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// A `file://` URL as a path. Windows accepts `file:///D:/x` and
/// `file://server/share`; Unix accepts `file:///srv/x`.
fn file_url_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    if rest.is_empty() {
        return None;
    }
    let rest = percent_decode(rest);
    if let Some(unc) = rest.strip_prefix("//") {
        return Some(PathBuf::from(format!("\\\\{}", unc.replace('/', "\\"))));
    }
    if cfg!(windows) {
        if let Some(trimmed) = rest.strip_prefix('/') {
            if trimmed.as_bytes().get(1) == Some(&b':') {
                return Some(PathBuf::from(trimmed));
            }
        }
    }
    Some(PathBuf::from(rest))
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let high = hex_value(bytes[i + 1]);
            let low = hex_value(bytes[i + 2]);
            if let (Some(high), Some(low)) = (high, low) {
                out.push(high << 4 | low);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// A file name component that will not collide between two tests (or two runs)
/// in the same second.
fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "omniup-{}-{}-{}",
            std::process::id(),
            label,
            unique_suffix()
        ));
        fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn file_url(path: &Path) -> String {
        let text = path.to_string_lossy().replace('\\', "/");
        if text.starts_with('/') {
            format!("file://{text}")
        } else {
            format!("file:///{text}")
        }
    }

    fn manifest_for(version: &str, abi: u32, asset: &Path) -> String {
        let digest = sha256::hex_digest(&fs::read(asset).unwrap());
        format!(
            r#"{{"version":"{version}","abi":{abi},"assets":{{"{}":{{"url":"{}","sha256":"{digest}"}}}}}}"#,
            update::host_platform().expect("tests run on a released platform"),
            file_url(asset)
        )
    }

    #[test]
    fn a_file_url_fetches_without_a_server() {
        let dir = scratch("fetch");
        let path = dir.join("manifest.json");
        fs::write(&path, br#"{"version":"0.1.0"}"#).unwrap();
        let bytes = fetch(&file_url(&path), 1024).unwrap();
        assert_eq!(bytes, br#"{"version":"0.1.0"}"#);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_over_the_limit_is_refused() {
        let dir = scratch("limit");
        let path = dir.join("big.json");
        fs::write(&path, vec![b'x'; 64]).unwrap();
        let error = fetch(&file_url(&path), 16).unwrap_err();
        assert!(
            matches!(error, UpdateError::Fetch(ref m) if m.contains("larger than")),
            "{error}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_http_is_refused_without_the_opt_in() {
        // The environment variable is read at call time; the test does not set
        // it, so the refusal is the default path.
        let error = fetch("http://127.0.0.1:1/omni-update.json", 1024).unwrap_err();
        assert!(matches!(error, UpdateError::InsecureUrl(_)), "{error}");
    }

    #[test]
    fn a_check_reports_up_to_date_and_available() {
        let dir = scratch("check-status");
        let asset = dir.join("omni.bin");
        fs::write(&asset, b"engine").unwrap();

        let same = dir.join("same.json");
        fs::write(&same, manifest_for(CURRENT_VERSION, ABI_VERSION, &asset)).unwrap();
        let outcome = check(&file_url(&same)).unwrap();
        assert_eq!(outcome.status, UpdateStatus::UpToDate);
        assert!(outcome.asset.is_some());

        let newer = dir.join("newer.json");
        fs::write(&newer, manifest_for("99.0.0", ABI_VERSION, &asset)).unwrap();
        assert_eq!(
            check(&file_url(&newer)).unwrap().status,
            UpdateStatus::Available
        );

        let future = dir.join("future.json");
        fs::write(&future, manifest_for("99.0.0", ABI_VERSION + 1, &asset)).unwrap();
        assert_eq!(
            check(&file_url(&future)).unwrap().status,
            UpdateStatus::AbiMismatch
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_download_that_does_not_match_its_checksum_is_refused() {
        let dir = scratch("checksum");
        let asset = dir.join("omni.bin");
        fs::write(&asset, b"real bytes").unwrap();
        let wrong = ReleaseAsset {
            url: file_url(&asset),
            sha256: "0".repeat(64),
        };
        let error = download_verified(&wrong).unwrap_err();
        assert!(matches!(error, UpdateError::Checksum { .. }), "{error}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_replaces_the_target_and_keeps_a_backup() {
        let dir = scratch("install");
        let target = dir.join("omni");
        fs::write(&target, b"old engine").unwrap();
        let backup = install(&target, b"new engine").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new engine");
        // On Windows the rename dance deletes the backup; either way the old
        // bytes are gone from the target and no temp file remains.
        let leftover = dir.join(".omni.new");
        assert!(!leftover.exists());
        if backup.is_some() {
            assert!(dir.join("omni.bak").exists());
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dry_run_downloads_nothing_and_changes_nothing() {
        let dir = scratch("dry-run");
        let asset = dir.join("omni.bin");
        fs::write(&asset, b"engine").unwrap();
        let manifest = dir.join("manifest.json");
        fs::write(&manifest, manifest_for("99.0.0", ABI_VERSION, &asset)).unwrap();

        let target = dir.join("omni");
        fs::write(&target, b"still here").unwrap();
        let mut options = UpdateOptions::new(&target);
        options.source = file_url(&manifest);
        options.dry_run = true;
        let report = run_update(&options).unwrap();
        assert_eq!(report.status, UpdateStatus::Available);
        assert!(!report.replaced);
        assert_eq!(fs::read(&target).unwrap(), b"still here");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_url_decodes_its_escapes() {
        let decoded = file_url_path("file:///tmp/a%20b.json").unwrap();
        assert!(decoded.ends_with("a b.json"), "{decoded:?}");
    }

    #[test]
    fn a_report_maps_to_a_message_and_an_exit_code() {
        let base = |status: UpdateStatus, replaced: bool| UpdateReport {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            status,
            target: PathBuf::from("omni"),
            replaced,
            backup: None,
        };

        let (code, text) = update_summary(&base(UpdateStatus::UpToDate, false));
        assert_eq!(code, 0);
        assert!(text.contains("already up to date"), "{text}");

        let (code, text) = update_summary(&base(UpdateStatus::Available, false));
        assert_eq!(code, EXIT_UPDATE_AVAILABLE);
        assert!(text.starts_with("would replace"), "{text}");

        let (code, text) = update_summary(&base(UpdateStatus::Available, true));
        assert_eq!(code, 0);
        assert!(text.contains("to 0.2.0"), "{text}");

        let (code, text) = update_summary(&base(UpdateStatus::AbiMismatch, false));
        assert_eq!(code, EXIT_ABI_MISMATCH);
        assert!(text.contains("different ABI"), "{text}");

        assert_eq!(status_exit_code(UpdateStatus::Available), 10);
        assert_eq!(status_exit_code(UpdateStatus::UpToDate), 0);
    }
}
