// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    clippy::case_sensitive_file_extension_comparisons,
    clippy::items_after_statements,
    clippy::verbose_bit_mask,
    reason = "Preserve the upstream implementation and callback contracts."
)]
use crate::config::NetworkMitmCaConfig;
use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use base64::Engine as _;
use ra_config::find_ra_home;
use rama_net::tls::ApplicationProtocol;
use rama_tls_rustls::dep::pki_types::CertificateDer;
use rama_tls_rustls::dep::pki_types::PrivateKeyDer;
use rama_tls_rustls::dep::pki_types::pem::PemObject;
use rama_tls_rustls::dep::rcgen::BasicConstraints;
use rama_tls_rustls::dep::rcgen::CertificateParams;
use rama_tls_rustls::dep::rcgen::DistinguishedName;
use rama_tls_rustls::dep::rcgen::DnType;
use rama_tls_rustls::dep::rcgen::ExtendedKeyUsagePurpose;
use rama_tls_rustls::dep::rcgen::IsCa;
use rama_tls_rustls::dep::rcgen::Issuer;
use rama_tls_rustls::dep::rcgen::KeyPair;
use rama_tls_rustls::dep::rcgen::KeyUsagePurpose;
use rama_tls_rustls::dep::rcgen::PKCS_ECDSA_P256_SHA256;
use rama_tls_rustls::dep::rcgen::SanType;
use rama_tls_rustls::dep::rustls;
use rama_tls_rustls::server::TlsAcceptorData;
use sha2::Digest as _;
use sha2::Sha256;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read as _;
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tracing::info;
use tracing::warn;

pub(super) struct ManagedMitmCa {
    issuer: Issuer<'static, KeyPair>,
    certificate_path: PathBuf,
    _artifact_lease: Option<File>,
}

static MANAGED_MITM_CAS: LazyLock<Mutex<HashMap<PathBuf, Arc<ManagedMitmCa>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const MAX_EXTERNAL_CA_FILE_BYTES: u64 = 1024 * 1024;

impl ManagedMitmCa {
    pub(super) fn load_or_create() -> Result<Arc<Self>> {
        let proxy_dir = managed_ca_dir()?;
        let mut managed_cas = MANAGED_MITM_CAS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ca) = managed_cas.get(&proxy_dir) {
            return Ok(ca.clone());
        }

        let ca = Arc::new(Self::create(&proxy_dir)?);
        managed_cas.insert(proxy_dir, ca.clone());
        Ok(ca)
    }

    fn create(proxy_dir: &Path) -> Result<Self> {
        fs::create_dir_all(proxy_dir)
            .with_context(|| format!("failed to create {}", proxy_dir.display()))?;

        let (certificate_pem, private_key) = generate_ca()?;
        let artifact_lock = match lock_managed_ca_artifacts(proxy_dir) {
            Ok(lock) => Some(lock),
            Err(err) => {
                warn!("failed to lock managed MITM CA artifacts; skipping pruning: {err}");
                None
            }
        };
        let certificate_path = persist_managed_ca_certificate(proxy_dir, &certificate_pem)?;
        let issuer = Issuer::from_ca_cert_pem(&certificate_pem, private_key)
            .context("failed to parse managed MITM CA certificate")?;
        let artifact_lease = lock_managed_ca_certificate(&certificate_path)?;
        if artifact_lock.is_some() {
            prune_managed_ca_artifacts(proxy_dir);
        }
        info!(
            cert_path = %certificate_path.display(),
            "generated process-local MITM CA"
        );
        Ok(Self {
            issuer,
            certificate_path,
            _artifact_lease: Some(artifact_lease),
        })
    }

    pub(super) fn load_from_files(config: &NetworkMitmCaConfig) -> Result<Arc<Self>> {
        let certificate_path = Path::new(&config.certificate_file);
        let private_key_path = Path::new(&config.private_key_file);
        anyhow::ensure!(
            certificate_path.is_absolute(),
            "network.mitm_ca.certificate_file must be an absolute path"
        );
        anyhow::ensure!(
            private_key_path.is_absolute(),
            "network.mitm_ca.private_key_file must be an absolute path"
        );
        let certificate_pem = read_external_ca_file(
            certificate_path,
            "network.mitm_ca.certificate_file",
            /*private*/ false,
        )?;
        anyhow::ensure!(
            !certificate_pem
                .lines()
                .any(|line| line.trim().starts_with("-----BEGIN ")
                    && line.contains("PRIVATE KEY-----")),
            "network.mitm_ca.certificate_file must not contain a private key"
        );
        let private_key_pem = read_external_ca_file(
            private_key_path,
            "network.mitm_ca.private_key_file",
            /*private*/ true,
        )?;
        let certificate = CertificateDer::from_pem_slice(certificate_pem.as_bytes())
            .context("failed to parse network.mitm_ca.certificate_file")?;
        let private_key_der = PrivateKeyDer::from_pem_slice(private_key_pem.as_bytes())
            .context("failed to parse network.mitm_ca.private_key_file")?;
        let provider = rustls::crypto::CryptoProvider::get_default()
            .context("rustls crypto provider is not installed")?;
        let certified_key =
            rustls::sign::CertifiedKey::from_der(vec![certificate], private_key_der, provider)
                .context("network.mitm_ca certificate and private key do not match")?;
        certified_key
            .keys_match()
            .context("network.mitm_ca certificate and private key do not match")?;
        let private_key = KeyPair::from_pem(&private_key_pem)
            .context("failed to parse network.mitm_ca.private_key_file")?;
        let issuer = Issuer::from_ca_cert_pem(&certificate_pem, private_key)
            .context("failed to parse network.mitm_ca certificate and private key")?;
        Ok(Arc::new(Self {
            issuer,
            certificate_path: certificate_path.to_path_buf(),
            _artifact_lease: None,
        }))
    }

    pub(super) fn certificate_path(&self) -> &Path {
        &self.certificate_path
    }

    pub(super) fn tls_acceptor_data_for_host(&self, host: &str) -> Result<TlsAcceptorData> {
        let (cert_pem, key_pem) = issue_host_certificate_pem(host, &self.issuer)?;
        let cert = CertificateDer::from_pem_slice(cert_pem.as_bytes())
            .context("failed to parse host cert PEM")?;
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
            .context("failed to parse host key PEM")?;
        let mut server_config =
            rustls::ServerConfig::builder_with_protocol_versions(rustls::ALL_VERSIONS)
                .with_no_client_auth()
                .with_single_cert(vec![cert], key)
                .context("failed to build rustls server config")?;
        server_config.alpn_protocols = vec![
            ApplicationProtocol::HTTP_2.as_bytes().to_vec(),
            ApplicationProtocol::HTTP_11.as_bytes().to_vec(),
        ];

        Ok(TlsAcceptorData::from(server_config))
    }
}

fn read_external_ca_file(path: &Path, field: &str, private: bool) -> Result<String> {
    // Do not accept a caller-provided signing key where its access controls cannot be verified.
    #[cfg(not(unix))]
    anyhow::ensure!(
        !private,
        "{field} is not supported on this platform because private-file permissions cannot be verified"
    );

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to open {field}"))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("failed to inspect {field}"))?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "{field} must be a regular file"
    );
    anyhow::ensure!(
        metadata.len() <= MAX_EXTERNAL_CA_FILE_BYTES,
        "{field} exceeds {MAX_EXTERNAL_CA_FILE_BYTES} bytes"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        anyhow::ensure!(
            !private || metadata.permissions().mode() & 0o077 == 0,
            "{field} must not be accessible by group or other users"
        );
    }
    let mut pem = String::new();
    file.take(MAX_EXTERNAL_CA_FILE_BYTES + 1)
        .read_to_string(&mut pem)
        .with_context(|| format!("failed to read {field}"))?;
    anyhow::ensure!(
        u64::try_from(pem.len())? <= MAX_EXTERNAL_CA_FILE_BYTES,
        "{field} exceeds {MAX_EXTERNAL_CA_FILE_BYTES} bytes"
    );
    Ok(pem)
}

fn issue_host_certificate_pem(
    host: &str,
    issuer: &Issuer<'_, KeyPair>,
) -> Result<(String, String)> {
    let mut params = if let Ok(ip) = host.parse::<IpAddr>() {
        let mut params = CertificateParams::new(Vec::new())
            .map_err(|err| anyhow!("failed to create cert params: {err}"))?;
        params.subject_alt_names.push(SanType::IpAddress(ip));
        params
    } else {
        CertificateParams::new(vec![host.to_string()])
            .map_err(|err| anyhow!("failed to create cert params: {err}"))?
    };

    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];

    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|err| anyhow!("failed to generate host key pair: {err}"))?;
    let cert = params
        .signed_by(&key_pair, issuer)
        .map_err(|err| anyhow!("failed to sign host cert: {err}"))?;

    Ok((cert.pem(), key_pair.serialize_pem()))
}

const MANAGED_MITM_CA_DIR: &str = "proxy";
const MANAGED_MITM_CA_ARTIFACT_LOCK: &str = ".artifacts.lock";
const MANAGED_MITM_CA_CERT_PREFIX: &str = "ca";
const MANAGED_MITM_CA_TRUST_BUNDLE_PREFIX: &str = "ca-bundle";
const MANAGED_MITM_EXTERNAL_CA_DIR: &str = "external";
pub(crate) const SSL_CERT_DIR_ENV_KEY: &str = "SSL_CERT_DIR";

// Best-effort compatibility set for common child toolchains that accept a CA bundle path.
// This is intentionally curated rather than pretending to cover every TLS client.
pub const CUSTOM_CA_ENV_KEYS: [&str; 11] = [
    "CODEX_CA_CERTIFICATE",
    "SSL_CERT_FILE",
    "REQUESTS_CA_BUNDLE",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "GIT_SSL_CAINFO",
    "CARGO_HTTP_CAINFO",
    "PIP_CERT",
    "BUNDLE_SSL_CA_CERT",
    "npm_config_cafile",
    "NPM_CONFIG_CAFILE",
];

pub(crate) fn ca_env_from_process() -> HashMap<&'static str, String> {
    CUSTOM_CA_ENV_KEYS
        .into_iter()
        .chain([SSL_CERT_DIR_ENV_KEY])
        .filter_map(|key| std::env::var(key).ok().map(|value| (key, value)))
        .collect()
}

/// Immutable managed MITM CA bundle path plus startup TLS env values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedMitmCaTrustBundle {
    pub(crate) path: PathBuf,
    pub(crate) startup_env_values: HashMap<&'static str, String>,
}

fn managed_ca_dir() -> Result<PathBuf> {
    let codex_home = find_ra_home().context("failed to resolve RA_HOME for managed MITM CA")?;
    Ok(codex_home.join(MANAGED_MITM_CA_DIR).to_path_buf())
}

pub(crate) fn managed_ca_trust_bundle(
    env: &HashMap<&'static str, String>,
) -> Result<ManagedMitmCaTrustBundle> {
    let ca = ManagedMitmCa::load_or_create()?;
    managed_ca_trust_bundle_for_cert_path(ca.certificate_path(), env)
}

pub(crate) fn managed_ca_trust_bundle_for_cert_path(
    cert_path: &Path,
    env: &HashMap<&'static str, String>,
) -> Result<ManagedMitmCaTrustBundle> {
    let proxy_dir = cert_path
        .parent()
        .ok_or_else(|| anyhow!("managed MITM CA cert path is missing a parent"))?;
    managed_ca_trust_bundle_for_cert_path_in_dir(cert_path, proxy_dir, env)
}

pub(crate) fn managed_ca_trust_bundle_for_external_cert_path(
    cert_path: &Path,
    env: &HashMap<&'static str, String>,
) -> Result<ManagedMitmCaTrustBundle> {
    let proxy_dir = managed_ca_dir()?.join(MANAGED_MITM_EXTERNAL_CA_DIR);
    managed_ca_trust_bundle_for_cert_path_in_dir(cert_path, &proxy_dir, env)
}

fn managed_ca_trust_bundle_for_cert_path_in_dir(
    cert_path: &Path,
    proxy_dir: &Path,
    env: &HashMap<&'static str, String>,
) -> Result<ManagedMitmCaTrustBundle> {
    let startup_env_values = startup_ca_file_env_values(env);
    let startup_cert_dir = env
        .get(SSL_CERT_DIR_ENV_KEY)
        .filter(|value| !value.is_empty())
        .map(String::as_str);
    let trust_bundle =
        build_managed_ca_trust_bundle(cert_path, proxy_dir, &startup_env_values, startup_cert_dir)?;
    let path = persist_managed_ca_trust_bundle(proxy_dir, &trust_bundle)?;

    Ok(ManagedMitmCaTrustBundle {
        path,
        startup_env_values,
    })
}

pub(crate) fn upstream_tls_root_store_for_cert_path(
    managed_ca_cert_path: &Path,
    env: &HashMap<&'static str, String>,
) -> Result<Arc<rustls::RootCertStore>> {
    let startup_env_values = startup_ca_file_env_values(env);
    let startup_cert_dir = env
        .get(SSL_CERT_DIR_ENV_KEY)
        .filter(|value| !value.is_empty())
        .map(String::as_str);
    let external_trust_bundle_dir = managed_ca_dir()?.join(MANAGED_MITM_EXTERNAL_CA_DIR);
    let certificates = load_platform_and_startup_root_certificates(
        managed_ca_cert_path,
        &external_trust_bundle_dir,
        &startup_env_values,
        startup_cert_dir,
    )?;
    let mut roots = rustls::RootCertStore::empty();
    let (_, ignored) = roots.add_parsable_certificates(certificates);
    if ignored > 0 {
        warn!(
            ignored_root_count = ignored,
            "ignored invalid platform or startup roots for MITM upstream TLS"
        );
    }
    Ok(Arc::new(roots))
}

fn startup_ca_file_env_values(
    env: &HashMap<&'static str, String>,
) -> HashMap<&'static str, String> {
    CUSTOM_CA_ENV_KEYS
        .into_iter()
        .filter_map(|key| {
            env.get(key)
                .filter(|value| !value.is_empty())
                .map(|value| (key, value.clone()))
        })
        .collect()
}

fn build_managed_ca_trust_bundle(
    managed_ca_cert_path: &Path,
    trust_bundle_dir: &Path,
    startup_env_values: &HashMap<&'static str, String>,
    startup_cert_dir: Option<&str>,
) -> Result<String> {
    let mut trust_bundle = String::new();
    for cert in load_platform_and_startup_root_certificates(
        managed_ca_cert_path,
        trust_bundle_dir,
        startup_env_values,
        startup_cert_dir,
    )? {
        push_certificate_pem(&mut trust_bundle, cert.as_ref());
    }
    for cert in read_ca_certificates(managed_ca_cert_path)? {
        push_certificate_pem(&mut trust_bundle, cert.as_ref());
    }
    Ok(trust_bundle)
}

fn load_platform_and_startup_root_certificates(
    managed_ca_cert_path: &Path,
    trust_bundle_dir: &Path,
    startup_env_values: &HashMap<&'static str, String>,
    startup_cert_dir: Option<&str>,
) -> Result<Vec<CertificateDer<'static>>> {
    let managed_ca_cert = fs::read(managed_ca_cert_path).with_context(|| {
        format!(
            "failed to read managed MITM CA certificate: {}",
            managed_ca_cert_path.display()
        )
    })?;
    let managed_ca_cert = CertificateDer::from_pem_slice(&managed_ca_cert)
        .context("failed to parse managed MITM CA certificate")?;
    let rustls_native_certs::CertificateResult { certs, errors, .. } =
        crate::native_certs::load_platform_native_certs();
    if !errors.is_empty() {
        warn!(
            native_root_error_count = errors.len(),
            "encountered errors while loading native root certificates for MITM trust bundle"
        );
    }
    let mut certificates = certs;
    let mut appended_startup_paths = HashSet::new();
    for path in CUSTOM_CA_ENV_KEYS
        .into_iter()
        .filter_map(|key| startup_env_values.get(key))
        .map(PathBuf::from)
    {
        if path != managed_ca_cert_path
            && !is_current_generated_trust_bundle_path(
                &path,
                managed_ca_cert_path,
                trust_bundle_dir,
            )
            && appended_startup_paths.insert(path.clone())
        {
            certificates.extend(read_ca_certificates(&path)?);
        }
    }
    if let Some(startup_cert_dir) = startup_cert_dir {
        for path in std::env::split_paths(startup_cert_dir) {
            if appended_startup_paths.insert(path.clone()) {
                certificates.extend(load_ca_directory_certificates(&path));
            }
        }
    }
    let mut seen = HashSet::new();
    certificates.retain(|cert| cert != &managed_ca_cert && seen.insert(cert.as_ref().to_vec()));
    Ok(certificates)
}

fn read_ca_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let pem = fs::read(path)
        .with_context(|| format!("failed to read startup CA bundle: {}", path.display()))?;
    let pem = String::from_utf8_lossy(&pem);
    let contains_trusted_certificates = pem.contains("TRUSTED CERTIFICATE");
    let normalized_pem = pem
        .replace("BEGIN TRUSTED CERTIFICATE", "BEGIN CERTIFICATE")
        .replace("END TRUSTED CERTIFICATE", "END CERTIFICATE");
    let certs = CertificateDer::pem_slice_iter(normalized_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse startup CA bundle: {}", path.display()))?;
    if certs.is_empty() {
        return Err(anyhow!(
            "startup CA bundle contained no certificates: {}",
            path.display()
        ));
    }
    certs
        .into_iter()
        .map(|cert| {
            let cert = if contains_trusted_certificates {
                first_der_item(cert.as_ref()).ok_or_else(|| {
                    anyhow!(
                        "startup CA bundle contained an invalid trusted certificate: {}",
                        path.display()
                    )
                })?
            } else {
                cert.as_ref()
            };
            Ok(CertificateDer::from(cert.to_vec()))
        })
        .collect()
}

fn load_ca_directory_certificates(path: &Path) -> Vec<CertificateDer<'static>> {
    let rustls_native_certs::CertificateResult { certs, errors, .. } =
        rustls_native_certs::load_certs_from_paths(None, Some(path));
    if !errors.is_empty() {
        warn!(
            ca_path = %path.display(),
            ca_error_count = errors.len(),
            "encountered errors while loading startup CA directory"
        );
    }
    certs
}

fn first_der_item(der: &[u8]) -> Option<&[u8]> {
    der_item_length(der).map(|length| &der[..length])
}

fn der_item_length(der: &[u8]) -> Option<usize> {
    let &length_octet = der.get(1)?;
    if length_octet & 0x80 == 0 {
        return Some(2 + usize::from(length_octet)).filter(|length| *length <= der.len());
    }

    let length_octets = usize::from(length_octet & 0x7f);
    if length_octets == 0 {
        return None;
    }

    let length_end = 2usize.checked_add(length_octets)?;
    let mut content_length = 0usize;
    for &byte in der.get(2..length_end)? {
        content_length = content_length
            .checked_mul(256)?
            .checked_add(usize::from(byte))?;
    }
    length_end
        .checked_add(content_length)
        .filter(|length| *length <= der.len())
}

fn is_current_generated_trust_bundle_path(
    path: &Path,
    managed_ca_cert_path: &Path,
    trust_bundle_dir: &Path,
) -> bool {
    if is_generated_trust_bundle_path(path, trust_bundle_dir) {
        return true;
    }
    let Some(proxy_dir) = managed_ca_cert_path.parent() else {
        return false;
    };
    if is_generated_trust_bundle_path(path, proxy_dir) {
        return true;
    }
    let Some(file_name) = path.file_name().and_then(|file_name| file_name.to_str()) else {
        return false;
    };
    if path.parent() != Some(proxy_dir)
        || !file_name.starts_with(MANAGED_MITM_CA_TRUST_BUNDLE_PREFIX)
        || !file_name.ends_with(".pem")
    {
        return false;
    }
    let Ok(trust_bundle) = fs::read(path) else {
        return false;
    };
    let Ok(managed_ca_cert) = fs::read(managed_ca_cert_path) else {
        return false;
    };
    !managed_ca_cert.is_empty()
        && trust_bundle
            .windows(managed_ca_cert.len())
            .any(|window| window == managed_ca_cert)
}

fn is_generated_trust_bundle_path(path: &Path, proxy_dir: &Path) -> bool {
    is_generated_managed_ca_artifact_path(path, proxy_dir, MANAGED_MITM_CA_TRUST_BUNDLE_PREFIX)
}

fn is_generated_managed_ca_artifact_path(path: &Path, proxy_dir: &Path, prefix: &str) -> bool {
    let Some(file_name) = path.file_name().and_then(|file_name| file_name.to_str()) else {
        return false;
    };
    let Some(expected_hash) = file_name
        .strip_prefix(prefix)
        .and_then(|suffix| suffix.strip_prefix('-'))
        .and_then(|suffix| suffix.strip_suffix(".pem"))
    else {
        return false;
    };
    if path.parent() != Some(proxy_dir)
        || expected_hash.len() != 64
        || !expected_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return false;
    }
    let Ok(trust_bundle) = fs::read(path) else {
        return false;
    };
    format!("{:x}", Sha256::digest(trust_bundle)) == expected_hash
}

/// Returns whether `path` points at a current Codex-generated MITM CA bundle.
pub fn is_managed_mitm_ca_trust_bundle_path(path: &str) -> bool {
    let Ok(proxy_dir) = managed_ca_dir() else {
        return false;
    };
    let path = Path::new(path);
    is_generated_trust_bundle_path(path, &proxy_dir)
        || is_generated_trust_bundle_path(path, &proxy_dir.join(MANAGED_MITM_EXTERNAL_CA_DIR))
}

fn persist_managed_ca_trust_bundle(proxy_dir: &Path, trust_bundle: &str) -> Result<PathBuf> {
    fs::create_dir_all(proxy_dir)
        .with_context(|| format!("failed to create {}", proxy_dir.display()))?;
    let hash = Sha256::digest(trust_bundle.as_bytes());
    let trust_bundle_path = proxy_dir.join(format!(
        "{MANAGED_MITM_CA_TRUST_BUNDLE_PREFIX}-{hash:x}.pem"
    ));
    write_atomic_create_new_or_reuse(
        &trust_bundle_path,
        trust_bundle.as_bytes(),
        /*mode*/ 0o644,
    )
    .with_context(|| {
        format!(
            "failed to persist managed MITM CA trust bundle {}",
            trust_bundle_path.display()
        )
    })?;
    Ok(trust_bundle_path)
}

fn push_certificate_pem(bundle: &mut String, der: &[u8]) {
    bundle.push_str("-----BEGIN CERTIFICATE-----\n");
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    for chunk in encoded.as_bytes().chunks(64) {
        bundle.push_str(&String::from_utf8_lossy(chunk));
        bundle.push('\n');
    }
    bundle.push_str("-----END CERTIFICATE-----\n");
}

fn persist_managed_ca_certificate(proxy_dir: &Path, cert_pem: &str) -> Result<PathBuf> {
    let hash = Sha256::digest(cert_pem.as_bytes());
    let cert_path = proxy_dir.join(format!("{MANAGED_MITM_CA_CERT_PREFIX}-{hash:x}.pem"));
    write_atomic_create_new_or_reuse(&cert_path, cert_pem.as_bytes(), /*mode*/ 0o644)
        .with_context(|| {
            format!(
                "failed to persist managed MITM CA certificate {}",
                cert_path.display()
            )
        })?;
    Ok(cert_path)
}

fn lock_managed_ca_certificate(certificate_path: &Path) -> Result<File> {
    let lock_path = managed_ca_certificate_lock_path(certificate_path)
        .ok_or_else(|| anyhow!("managed MITM CA certificate path is missing a file name"))?;
    let file = open_managed_ca_lock(&lock_path)?;
    file.lock_shared()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;
    Ok(file)
}

fn lock_managed_ca_artifacts(proxy_dir: &Path) -> Result<File> {
    let lock_path = proxy_dir.join(MANAGED_MITM_CA_ARTIFACT_LOCK);
    let file = open_managed_ca_lock(&lock_path)?;
    file.lock()
        .with_context(|| format!("failed to lock {}", lock_path.display()))?;
    Ok(file)
}

fn managed_ca_certificate_lock_path(certificate_path: &Path) -> Option<PathBuf> {
    let file_name = certificate_path.file_name()?.to_string_lossy();
    Some(certificate_path.with_file_name(format!(".{file_name}.lock")))
}

fn open_managed_ca_lock(path: &Path) -> Result<File> {
    if fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(anyhow!(
            "refusing to use symlink lock file {}",
            path.display()
        ));
    }

    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))
}

fn prune_managed_ca_artifacts(proxy_dir: &Path) {
    for certificate_path in
        generated_managed_ca_artifact_paths(proxy_dir, MANAGED_MITM_CA_CERT_PREFIX)
    {
        remove_inactive_managed_ca_certificate(&certificate_path);
    }

    let remaining_certificates =
        generated_managed_ca_artifact_paths(proxy_dir, MANAGED_MITM_CA_CERT_PREFIX)
            .into_iter()
            .filter_map(|path| fs::read(path).ok())
            .filter(|certificate| !certificate.is_empty())
            .collect::<Vec<_>>();
    let bundle_paths =
        generated_managed_ca_artifact_paths(proxy_dir, MANAGED_MITM_CA_TRUST_BUNDLE_PREFIX);
    for bundle_path in bundle_paths {
        let Ok(contents) = fs::read(&bundle_path) else {
            continue;
        };
        if remaining_certificates.iter().any(|certificate| {
            contents
                .windows(certificate.len())
                .any(|window| window == certificate)
        }) {
            continue;
        }
        if let Err(err) = fs::remove_file(&bundle_path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            warn!(
                path = %bundle_path.display(),
                "failed to prune stale managed MITM CA trust bundle: {err}"
            );
        }
    }
}

fn generated_managed_ca_artifact_paths(proxy_dir: &Path, prefix: &str) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(proxy_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if !is_generated_managed_ca_artifact_path(&path, proxy_dir, prefix) {
                return None;
            }
            Some(path)
        })
        .collect()
}

fn remove_inactive_managed_ca_certificate(certificate_path: &Path) {
    let Some(lock_path) = managed_ca_certificate_lock_path(certificate_path) else {
        return;
    };
    let Ok(lock_file) = open_managed_ca_lock(&lock_path) else {
        return;
    };
    match lock_file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return,
        Err(err) => {
            warn!(
                path = %lock_path.display(),
                "failed to inspect managed MITM CA artifact lease: {err}"
            );
            return;
        }
    }

    let removed = match fs::remove_file(certificate_path) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => true,
        Err(err) => {
            warn!(
                path = %certificate_path.display(),
                "failed to prune stale managed MITM CA certificate: {err}"
            );
            false
        }
    };
    drop(lock_file);
    if removed
        && let Err(err) = fs::remove_file(&lock_path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        warn!(
            path = %lock_path.display(),
            "failed to prune stale managed MITM CA artifact lease: {err}"
        );
    }
}

fn generate_ca() -> Result<(String, KeyPair)> {
    let mut params = CertificateParams::default();
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyEncipherment,
    ];
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, "network_proxy MITM CA");
    params.distinguished_name = dn;

    let key_pair = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)
        .map_err(|err| anyhow!("failed to generate CA key pair: {err}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|err| anyhow!("failed to generate CA cert: {err}"))?;
    Ok((cert.pem(), key_pair))
}

fn write_atomic_create_new(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("missing parent directory"))?;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    let file_name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp_path = parent.join(format!(".{file_name}.tmp.{pid}.{nanos}"));

    let mut file = open_create_new_with_mode(&tmp_path, mode)?;
    file.write_all(contents)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to fsync {}", tmp_path.display()))?;
    drop(file);

    // Create the final file using "create-new" semantics (no overwrite). `rename` on Unix can
    // overwrite existing files, so prefer a hard-link, which fails if the destination exists.
    match fs::hard_link(&tmp_path, path) {
        Ok(()) => {
            fs::remove_file(&tmp_path)
                .with_context(|| format!("failed to remove {}", tmp_path.display()))?;
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&tmp_path);
            return Err(anyhow!(
                "refusing to overwrite existing file {}",
                path.display()
            ));
        }
        Err(_) => {
            // Best-effort fallback for environments where hard links are not supported.
            // This is still subject to a TOCTOU race, but the typical case is a private per-user
            // config directory, where other users cannot create files anyway.
            if path.exists() {
                let _ = fs::remove_file(&tmp_path);
                return Err(anyhow!(
                    "refusing to overwrite existing file {}",
                    path.display()
                ));
            }
            fs::rename(&tmp_path, path).with_context(|| {
                format!(
                    "failed to rename {} -> {}",
                    tmp_path.display(),
                    path.display()
                )
            })?;
        }
    }

    sync_parent_dir(parent)?;

    Ok(())
}

#[cfg(not(windows))]
fn sync_parent_dir(parent: &Path) -> Result<()> {
    // Best-effort durability: ensure the directory entry is persisted too.
    let dir = File::open(parent).with_context(|| format!("failed to open {}", parent.display()))?;
    dir.sync_all()
        .with_context(|| format!("failed to fsync {}", parent.display()))
}

#[cfg(windows)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "Keep the upstream signature shared with the Unix implementation."
)]
fn sync_parent_dir(_parent: &Path) -> Result<()> {
    Ok(())
}

fn write_atomic_create_new_or_reuse(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    if fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(anyhow!("refusing to reuse symlink {}", path.display()));
    }
    if fs::read(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    if path.exists() {
        return Err(anyhow!(
            "refusing to reuse existing mismatched file {}",
            path.display()
        ));
    }
    match write_atomic_create_new(path, contents, mode) {
        Ok(()) => Ok(()),
        Err(_err) if fs::read(path).ok().as_deref() == Some(contents) => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn open_create_new_with_mode(path: &Path, mode: u32) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))
}

#[cfg(not(unix))]
fn open_create_new_with_mode(path: &Path, _mode: u32) -> Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))
}
