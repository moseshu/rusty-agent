// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#[cfg(test)]
mod tests {
    use super::*;

    use pretty_assertions::assert_eq;
    use ra_tls::ensure_rustls_crypto_provider;
    use tempfile::tempdir;

    #[test]
    fn managed_ca_private_key_is_not_persisted() {
        ensure_rustls_crypto_provider();
        let dir = tempdir().unwrap();
        let ca = ManagedMitmCa::create(dir.path()).unwrap();
        ca.tls_acceptor_data_for_host("example.com").unwrap();
        let mut persisted_files = fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        persisted_files.sort();
        let mut expected_files = vec![
            ca.certificate_path().to_path_buf(),
            managed_ca_certificate_lock_path(ca.certificate_path()).unwrap(),
            dir.path().join(MANAGED_MITM_CA_ARTIFACT_LOCK),
        ];
        expected_files.sort();

        assert_eq!(persisted_files, expected_files);
        assert_eq!(
            fs::read(managed_ca_certificate_lock_path(ca.certificate_path()).unwrap()).unwrap(),
            Vec::<u8>::new()
        );
    }

    #[test]
    fn external_ca_certificate_rejects_private_key_material() {
        ensure_rustls_crypto_provider();
        let dir = tempdir().unwrap();
        let certificate_path = dir.path().join("certificate.pem");
        let private_key_path = dir.path().join("private-key.pem");
        let (certificate, private_key) = generate_ca().unwrap();
        let private_key = private_key.serialize_pem();
        fs::write(&certificate_path, format!("{certificate}{private_key}")).unwrap();
        fs::write(&private_key_path, private_key).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&private_key_path, fs::Permissions::from_mode(0o600)).unwrap();
        }

        let error = ManagedMitmCa::load_from_files(&NetworkMitmCaConfig {
            certificate_file: certificate_path.display().to_string(),
            private_key_file: private_key_path.display().to_string(),
        })
        .err()
        .expect("combined certificate and private key must be rejected");

        assert!(
            error
                .to_string()
                .contains("certificate_file must not contain a private key")
        );
    }

    #[cfg(unix)]
    #[test]
    fn external_ca_rejects_mismatched_private_key() {
        use std::os::unix::fs::PermissionsExt as _;

        ensure_rustls_crypto_provider();
        let dir = tempdir().unwrap();
        let certificate_path = dir.path().join("certificate.pem");
        let private_key_path = dir.path().join("private-key.pem");
        let (certificate, _) = generate_ca().unwrap();
        let (_, private_key) = generate_ca().unwrap();
        fs::write(&certificate_path, certificate).unwrap();
        fs::write(&private_key_path, private_key.serialize_pem()).unwrap();
        fs::set_permissions(&private_key_path, fs::Permissions::from_mode(0o600)).unwrap();

        let error = ManagedMitmCa::load_from_files(&NetworkMitmCaConfig {
            certificate_file: certificate_path.display().to_string(),
            private_key_file: private_key_path.display().to_string(),
        })
        .err()
        .expect("a mismatched external CA private key must be rejected");

        assert!(
            error
                .to_string()
                .contains("certificate and private key do not match")
        );
    }

    #[cfg(not(unix))]
    #[test]
    fn external_ca_private_key_fails_closed_without_unix_permissions() {
        let error = read_external_ca_file(
            Path::new("external-ca-key.pem"),
            "network.mitm_ca.private_key_file",
            /*private*/ true,
        )
        .expect_err("an external CA private key must fail closed on non-Unix platforms");

        assert!(
            error
                .to_string()
                .contains("private-file permissions cannot be verified")
        );
    }

    #[test]
    fn external_ca_trust_bundle_uses_the_proxy_artifact_directory() {
        ensure_rustls_crypto_provider();
        let certificate_dir = tempdir().unwrap();
        let artifact_dir = tempdir().unwrap();
        let certificate_path = certificate_dir.path().join("certificate.pem");
        let (certificate, private_key) = generate_ca().unwrap();
        let private_key = private_key.serialize_pem();
        fs::write(&certificate_path, format!("{certificate}{private_key}")).unwrap();

        let trust_bundle = managed_ca_trust_bundle_for_cert_path_in_dir(
            &certificate_path,
            artifact_dir.path(),
            &HashMap::new(),
        )
        .unwrap();

        assert_eq!(trust_bundle.path.parent(), Some(artifact_dir.path()));
        assert!(
            !fs::read_to_string(&trust_bundle.path)
                .unwrap()
                .contains("PRIVATE KEY"),
            "a derived trust bundle must contain certificates only"
        );
        assert_eq!(
            fs::read_dir(certificate_dir.path()).unwrap().count(),
            1,
            "building the trust bundle must not write next to the external CA"
        );
    }

    #[test]
    fn external_ca_trust_bundle_skips_inherited_generated_bundle() {
        let certificate_dir = tempdir().unwrap();
        let artifact_dir = tempdir().unwrap();
        let certificate_path = certificate_dir.path().join("certificate.pem");
        let (certificate, _) = generate_ca().unwrap();
        let (stale_certificate, _) = generate_ca().unwrap();
        fs::write(&certificate_path, &certificate).unwrap();
        let inherited_bundle_path = persist_managed_ca_trust_bundle(
            artifact_dir.path(),
            &format!("{stale_certificate}{certificate}"),
        )
        .unwrap();
        let env = HashMap::from([(
            "REQUESTS_CA_BUNDLE",
            inherited_bundle_path.display().to_string(),
        )]);

        let trust_bundle = managed_ca_trust_bundle_for_cert_path_in_dir(
            &certificate_path,
            artifact_dir.path(),
            &env,
        )
        .unwrap();
        let trust_bundle = fs::read_to_string(trust_bundle.path).unwrap();
        let certificates = CertificateDer::pem_slice_iter(trust_bundle.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let certificate = CertificateDer::from_pem_slice(certificate.as_bytes()).unwrap();
        let stale_certificate =
            CertificateDer::from_pem_slice(stale_certificate.as_bytes()).unwrap();

        assert!(certificates.contains(&certificate));
        assert!(
            !certificates.contains(&stale_certificate),
            "an inherited generated bundle must not preserve stale roots"
        );
    }

    #[test]
    fn managed_ca_artifact_pruning_preserves_only_active_certificates() {
        let dir = tempdir().unwrap();
        let mut artifacts = Vec::new();
        let mut active_lease = None;
        for index in 0..3 {
            let certificate = format!("certificate {index}\n");
            let certificate_path =
                persist_managed_ca_certificate(dir.path(), &certificate).unwrap();
            let lease = lock_managed_ca_certificate(&certificate_path).unwrap();
            if index == 0 {
                active_lease = Some(lease);
            } else {
                drop(lease);
            }
            let bundle_path =
                persist_managed_ca_trust_bundle(dir.path(), &format!("roots\n{certificate}"))
                    .unwrap();
            artifacts.push((certificate_path, bundle_path));
        }
        let unrelated_path = dir.path().join("ca-user.pem");
        fs::write(&unrelated_path, "user managed").unwrap();

        prune_managed_ca_artifacts(dir.path());

        let remaining_certificate_count =
            generated_managed_ca_artifact_paths(dir.path(), MANAGED_MITM_CA_CERT_PREFIX).len();
        assert_eq!(remaining_certificate_count, 1);
        assert!(artifacts[0].0.exists());
        assert!(artifacts[0].1.exists());
        assert!(!artifacts[1].0.exists());
        assert!(!artifacts[1].1.exists());
        assert!(!artifacts[2].0.exists());
        assert!(!artifacts[2].1.exists());
        assert!(unrelated_path.exists());

        drop(active_lease.take());
        prune_managed_ca_artifacts(dir.path());

        let remaining_certificates =
            generated_managed_ca_artifact_paths(dir.path(), MANAGED_MITM_CA_CERT_PREFIX);
        assert!(remaining_certificates.is_empty());
        assert!(!artifacts[0].0.exists());
        assert!(!artifacts[0].1.exists());
    }

    #[test]
    fn current_generated_trust_bundle_path_rejects_stale_bundle() {
        let dir = tempdir().unwrap();
        let managed_ca_cert_path = dir.path().join("ca.pem");
        let trust_bundle_path = dir.path().join("ca-bundle-123.pem");
        fs::write(&managed_ca_cert_path, "managed ca\n").unwrap();
        fs::write(&trust_bundle_path, "stale managed bundle\n").unwrap();
        assert!(!is_current_generated_trust_bundle_path(
            &trust_bundle_path,
            &managed_ca_cert_path,
            dir.path(),
        ));
    }

    #[test]
    fn generated_trust_bundle_path_requires_matching_content_hash() {
        let dir = tempdir().unwrap();
        let trust_bundle_path =
            persist_managed_ca_trust_bundle(dir.path(), "trusted roots").unwrap();

        assert!(is_generated_trust_bundle_path(
            &trust_bundle_path,
            dir.path()
        ));
        fs::write(&trust_bundle_path, "tampered roots").unwrap();
        assert!(!is_generated_trust_bundle_path(
            &trust_bundle_path,
            dir.path()
        ));
    }

    #[test]
    fn managed_ca_trust_bundle_appends_startup_file_and_directory_certificates() {
        let dir = tempdir().unwrap();
        let managed_ca_cert_path = dir.path().join("ca.pem");
        let startup_ca_bundle_path = dir.path().join("startup-ca.pem");
        let startup_ca_dir = dir.path().join("startup-certs");
        let (managed_ca_cert, _) = generate_ca().unwrap();
        let (startup_ca_cert, startup_ca_key) = generate_ca().unwrap();
        let startup_ca_key = startup_ca_key.serialize_pem();
        let (directory_ca_cert, _) = generate_ca().unwrap();
        let mut trusted_ca_der = CertificateDer::from_pem_slice(startup_ca_cert.as_bytes())
            .unwrap()
            .as_ref()
            .to_vec();
        trusted_ca_der.extend_from_slice(&[0x30, 0x00]);
        let mut trusted_ca_cert = String::new();
        push_certificate_pem(&mut trusted_ca_cert, &trusted_ca_der);
        let trusted_ca_cert = trusted_ca_cert.replace("CERTIFICATE", "TRUSTED CERTIFICATE");
        fs::write(&managed_ca_cert_path, &managed_ca_cert).unwrap();
        fs::write(
            &startup_ca_bundle_path,
            format!("{trusted_ca_cert}{startup_ca_key}"),
        )
        .unwrap();
        fs::create_dir(&startup_ca_dir).unwrap();
        fs::write(startup_ca_dir.join("directory-ca.pem"), &directory_ca_cert).unwrap();
        let startup_ca_bundle_path = startup_ca_bundle_path.display().to_string();
        let env = HashMap::from([
            ("SSL_CERT_FILE", startup_ca_bundle_path.clone()),
            (SSL_CERT_DIR_ENV_KEY, startup_ca_dir.display().to_string()),
        ]);

        let trust_bundle =
            managed_ca_trust_bundle_for_cert_path(&managed_ca_cert_path, &env).unwrap();
        assert_eq!(
            trust_bundle.startup_env_values,
            HashMap::from([("SSL_CERT_FILE", startup_ca_bundle_path)])
        );
        let baseline_bundle = fs::read_to_string(&trust_bundle.path).unwrap();
        let baseline_certs = CertificateDer::pem_slice_iter(baseline_bundle.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let expected_certs = [&startup_ca_cert, &directory_ca_cert, &managed_ca_cert]
            .map(|cert| CertificateDer::from_pem_slice(cert.as_bytes()).unwrap());

        assert!(
            expected_certs
                .iter()
                .all(|cert| baseline_certs.contains(cert))
        );
        assert!(!baseline_bundle.contains(&startup_ca_key));
        assert!(!baseline_bundle.contains("TRUSTED CERTIFICATE"));
    }

    #[test]
    fn managed_ca_trust_bundle_skips_inherited_current_bundle() {
        let dir = tempdir().unwrap();
        let managed_ca_cert_path = dir.path().join("ca.pem");
        let inherited_bundle_path = dir.path().join("ca-bundle-parent.pem");
        let (managed_ca_cert, _) = generate_ca().unwrap();
        fs::write(&managed_ca_cert_path, &managed_ca_cert).unwrap();
        fs::write(
            &inherited_bundle_path,
            format!("parent roots\n{managed_ca_cert}"),
        )
        .unwrap();
        let env = HashMap::from([(
            "REQUESTS_CA_BUNDLE",
            inherited_bundle_path.display().to_string(),
        )]);

        let trust_bundle =
            managed_ca_trust_bundle_for_cert_path(&managed_ca_cert_path, &env).unwrap();
        let baseline_bundle = fs::read_to_string(&trust_bundle.path).unwrap();
        let baseline_certs = CertificateDer::pem_slice_iter(baseline_bundle.as_bytes())
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        let managed_ca_cert = CertificateDer::from_pem_slice(managed_ca_cert.as_bytes()).unwrap();

        assert_eq!(
            baseline_certs
                .iter()
                .filter(|cert| *cert == &managed_ca_cert)
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_create_new_or_reuse_rejects_matching_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let target = dir.path().join("real-bundle.pem");
        let link = dir.path().join("ca-bundle.pem");
        fs::write(&target, "bundle").unwrap();
        symlink(&target, &link).unwrap();

        let err = write_atomic_create_new_or_reuse(&link, b"bundle", /*mode*/ 0o644).unwrap_err();

        assert_eq!(
            err.to_string(),
            format!("refusing to reuse symlink {}", link.display())
        );
    }
}
