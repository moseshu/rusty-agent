//! Amazon Bedrock credential sources, endpoints, catalogs, and credential export.
//!
//! Ported from the tests in Codex `codex-rs/model-provider/src/amazon_bedrock/`
//! (openai/codex @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052): `auth.rs`, `mantle.rs`,
//! `catalog.rs`, `runtime_catalog_tests.rs`, `error_tests.rs`, and `credential_export_tests.rs`.
//! Codex's sign-in managed Bedrock credentials have no local counterpart, so their cases are
//! omitted from the source-precedence table.

use std::io;
use std::num::NonZeroU64;
use std::time::{Duration, SystemTime};

use http::{HeaderMap, HeaderValue, StatusCode};
use ra_coding::config::amazon_bedrock_test_api as bedrock;
use ra_coding::config::{ModelsResponse, catalog::ModelVisibility, catalog::WebSearchToolType};
use ra_config::{
    AwsCredentialExportConfig, ModelProviderAuthInfo, ModelProviderAwsAuthInfo, ModelProviderInfo,
};
use serde_json::json;

const SECRET: &str = "credential-export-secret";
const VALID_OUTPUT: &[u8] = br#"{"AccessKeyId":"exported","SecretAccessKey":"credential-export-secret","SessionToken":"credential-export-secret"}"#;
const MAX_CREDENTIAL_OUTPUT_BYTES: usize = 64 * 1024;

fn aws(region: Option<&str>) -> ModelProviderAwsAuthInfo {
    ModelProviderAwsAuthInfo {
        profile: None,
        region: region.map(str::to_owned),
        credential_export: None,
        auth_refresh: None,
    }
}

fn bedrock_provider(aws: Option<ModelProviderAwsAuthInfo>) -> ModelProviderInfo {
    ModelProviderInfo {
        name: "Amazon Bedrock".to_owned(),
        aws,
        ..ModelProviderInfo::default()
    }
}

fn missing_env_var(_: &'static str) -> Result<String, std::env::VarError> {
    Err(std::env::VarError::NotPresent)
}

#[test]
fn bedrock_auth_source_distinguishes_static_environment_credentials() {
    let provider = bedrock_provider(None);
    let configured_profile = bedrock_provider(Some(ModelProviderAwsAuthInfo {
        profile: Some("configured-profile".to_owned()),
        ..aws(Some("us-west-2"))
    }));
    let credential_export = bedrock_provider(Some(ModelProviderAwsAuthInfo {
        credential_export: Some(AwsCredentialExportConfig {
            command: "export-credentials".to_owned(),
            args: Vec::new(),
            timeout_ms: NonZeroU64::new(30_000).unwrap(),
        }),
        ..aws(Some("us-west-2"))
    }));
    let command = ModelProviderInfo {
        auth: Some(ModelProviderAuthInfo {
            command: "token-fetcher".to_owned(),
            args: Vec::new(),
            timeout_ms: NonZeroU64::new(5_000).unwrap(),
            refresh_interval_ms: 300_000,
            cwd: std::env::current_dir().unwrap().try_into().unwrap(),
        }),
        ..credential_export.clone()
    };
    let all = [
        "AWS_BEARER_TOKEN_BEDROCK",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
    ];
    let cases: &[(&ModelProviderInfo, &[&str], &str)] = &[
        (&command, &all, "CommandBearerToken"),
        (&credential_export, &all, "CredentialExport"),
        (&configured_profile, &all, "ConfiguredAwsProfile"),
        (&provider, &all, "EnvBearerToken"),
        (
            &provider,
            &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"],
            "EnvAwsCredentials",
        ),
        (&provider, &["AWS_ACCESS_KEY_ID"], "AwsSdk"),
        (&provider, &["AWS_PROFILE", "AWS_REGION"], "AwsSdk"),
    ];
    for (provider, variables, expected) in cases {
        let actual = bedrock::auth_source(provider, |name| {
            variables
                .contains(&name)
                .then(|| "configured".to_owned())
                .ok_or(std::env::VarError::NotPresent)
        });
        assert_eq!(actual, *expected, "{variables:?}");
    }
    let blank = bedrock::auth_source(&provider, |name| {
        (name == "AWS_BEARER_TOKEN_BEDROCK")
            .then(|| "   ".to_owned())
            .ok_or(std::env::VarError::NotPresent)
    });
    assert_eq!(blank, "AwsSdk");
}

#[test]
fn bedrock_bearer_region_prefers_configuration_then_environment() {
    let configured = bedrock::bearer_token_region(&aws(Some(" us-west-2 ")), |name| match name {
        "AWS_REGION" => Ok("eu-west-1".to_owned()),
        _ => Err(std::env::VarError::NotPresent),
    });
    assert_eq!(configured.as_deref(), Ok("us-west-2"));
    let region = bedrock::bearer_token_region(&aws(None), |name| match name {
        "AWS_REGION" => Ok(" eu-central-1 ".to_owned()),
        _ => Err(std::env::VarError::NotPresent),
    });
    assert_eq!(region.as_deref(), Ok("eu-central-1"));
    let default_region = bedrock::bearer_token_region(&aws(None), |name| match name {
        "AWS_DEFAULT_REGION" => Ok("ap-northeast-1".to_owned()),
        _ => Err(std::env::VarError::NotPresent),
    });
    assert_eq!(default_region.as_deref(), Ok("ap-northeast-1"));
    let missing = bedrock::bearer_token_region(&aws(None), missing_env_var).unwrap_err();
    assert!(
        missing.contains(
            "Amazon Bedrock bearer token auth requires \
`model_providers.amazon-bedrock.aws.region`, `AWS_REGION`, or `AWS_DEFAULT_REGION`"
        ),
        "{missing}"
    );
}

#[test]
fn endpoints_use_region_and_reject_unsupported_mantle_regions() {
    for (region, expected) in [
        (
            "ap-northeast-1",
            "https://bedrock-mantle.ap-northeast-1.api.aws/openai/v1",
        ),
        (
            "eu-central-1",
            "https://bedrock-mantle.eu-central-1.api.aws/openai/v1",
        ),
        (
            "us-gov-east-1",
            "https://bedrock-mantle.us-gov-east-1.api.aws/openai/v1",
        ),
        (
            "us-gov-west-1",
            "https://bedrock-mantle.us-gov-west-1.api.aws/openai/v1",
        ),
    ] {
        assert_eq!(bedrock::mantle_base_url(region).as_deref(), Ok(expected));
    }
    let error = bedrock::mantle_base_url("us-west-1").unwrap_err();
    assert!(
        error.contains("Amazon Bedrock does not support region `us-west-1`"),
        "{error}"
    );
    assert_eq!(
        bedrock::runtime_base_url("eu-west-1"),
        "https://bedrock-runtime.eu-west-1.amazonaws.com/openai/v1"
    );
}

#[test]
fn aws_auth_config_uses_profile_region_and_endpoint_service() {
    let profile = ModelProviderAwsAuthInfo {
        profile: Some("codex-bedrock".to_owned()),
        ..aws(None)
    };
    assert_eq!(
        bedrock::aws_auth_config(false, &profile),
        (
            Some("codex-bedrock".to_owned()),
            None,
            "bedrock-mantle".to_owned()
        )
    );
    assert_eq!(
        bedrock::aws_auth_config(true, &aws(Some(" us-west-2 "))),
        (None, Some("us-west-2".to_owned()), "bedrock".to_owned())
    );
}

#[test]
fn bedrock_mantle_sigv4_strips_headers_not_preserved_by_mantle() {
    let mut headers = HeaderMap::new();
    for name in ["session_id", "thread_id", "future_identity_header"] {
        headers.insert(
            name,
            HeaderValue::from_static("019dae79-15c3-70c3-8736-3219b8602b37"),
        );
    }
    headers.insert(
        "x-client-request-id",
        HeaderValue::from_static("request-id"),
    );

    bedrock::strip_mantle_headers(&mut headers);

    assert!(!headers.contains_key("session_id"));
    assert!(!headers.contains_key("thread_id"));
    assert!(!headers.contains_key("future_identity_header"));
    assert_eq!(
        headers
            .get("x-client-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("request-id")
    );
}

#[test]
fn catalog_uses_mantle_model_ids_in_priority_order_with_bedrock_metadata() {
    let [mantle, gov, runtime] = bedrock::static_catalogs().unwrap();
    assert_eq!(
        mantle
            .models
            .iter()
            .map(|model| model.slug.as_str())
            .collect::<Vec<_>>(),
        [
            "openai.gpt-6.1-sol",
            "openai.gpt-6-astra",
            "openai.gpt-6-sol",
            "openai.gpt-6-luna",
            "openai.gpt-5.6-sol",
            "openai.gpt-5.6-terra",
            "openai.gpt-5.6-luna",
            "openai.gpt-5.5",
        ]
    );
    for (priority, model) in (0..).zip(&mantle.models) {
        assert_eq!(model.priority, priority, "{}", model.slug);
        assert_eq!(model.visibility, ModelVisibility::List);
        assert_eq!((&model.availability_nux, &model.upgrade), (&None, &None));
        assert_eq!(model.web_search_tool_type, WebSearchToolType::Text);
        assert_eq!(model.additional_speed_tiers, Vec::<String>::new());
        assert_eq!(model.default_service_tier, None);
        assert_eq!(model.context_window, Some(272_000), "{}", model.slug);
        let tiers = model
            .service_tiers
            .iter()
            .map(|tier| tier.id.as_str())
            .collect::<Vec<_>>();
        let ultrafast = matches!(
            model.slug.as_str(),
            "openai.gpt-6-astra" | "openai.gpt-6.1-sol"
        );
        assert_eq!(tiers, if ultrafast { vec!["ultrafast"] } else { vec![] });
    }
    assert_eq!(
        mantle
            .models
            .last()
            .and_then(|model| model.max_context_window),
        Some(272_000)
    );

    let bundled = bedrock::bundled_catalog().unwrap();
    let source = bundled
        .models
        .iter()
        .find(|model| model.slug == "gpt-6-luna")
        .unwrap();
    let ported = mantle
        .models
        .iter()
        .find(|model| model.slug == "openai.gpt-6-luna")
        .unwrap();
    assert_eq!(ported.display_name, "GPT-6 Luna");
    assert_eq!(ported.max_context_window, source.max_context_window);
    assert_eq!(ported.model_messages, source.model_messages);
    assert!(!ported.use_responses_lite);

    assert_eq!(
        gov.models
            .iter()
            .map(|model| model.slug.as_str())
            .collect::<Vec<_>>(),
        ["openai.gpt-5.6-terra", "openai.gpt-5.6-luna"]
    );

    assert_eq!(runtime.models.len(), 14);
    assert_eq!(runtime.models[0].slug, "global.openai.gpt-6.1-sol");
    assert_eq!(runtime.models[0].display_name, "GPT-6.1 Sol (Global)");
    assert_eq!(runtime.models[7].slug, "us.openai.gpt-6.1-sol");
    assert_eq!(
        runtime.models[7].display_name,
        "GPT-6.1 Sol (US cross-region)"
    );
    assert_eq!(runtime.models[7].priority, 7);
    assert!(
        runtime
            .models
            .iter()
            .all(|model| !model.supports_search_tool)
    );
    assert!(
        runtime
            .models
            .iter()
            .all(|model| !model.slug.ends_with("gpt-5.5"))
    );
    let runtime_ultrafast = runtime
        .models
        .iter()
        .filter(|model| !model.service_tiers.is_empty())
        .map(|model| model.slug.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        runtime_ultrafast,
        [
            "global.openai.gpt-6.1-sol",
            "global.openai.gpt-6-astra",
            "us.openai.gpt-6.1-sol",
            "us.openai.gpt-6-astra",
        ]
    );
}

#[test]
fn configured_and_govcloud_catalogs_follow_the_provider() {
    let bundled = bedrock::bundled_catalog().unwrap();
    let configured = ModelsResponse {
        models: bundled
            .models
            .iter()
            .filter(|model| model.slug == "gpt-5.5")
            .cloned()
            .collect(),
    };
    let normalized = bedrock::provider_catalog(&bedrock_provider(None), Some(&configured)).unwrap();
    let mut expected = configured.clone();
    for model in &mut expected.models {
        model.web_search_tool_type = WebSearchToolType::Text;
    }
    assert_eq!(normalized, expected);
    assert_eq!(bedrock::normalize_catalog(configured), expected);

    let gov = ModelProviderInfo {
        base_url: Some("https://bedrock-mantle.us-gov-west-1.api.aws/openai/v1".to_owned()),
        ..bedrock_provider(None)
    };
    let [_, gov_catalog, runtime_catalog] = bedrock::static_catalogs().unwrap();
    assert_eq!(bedrock::provider_catalog(&gov, None).unwrap(), gov_catalog);
    let runtime = ModelProviderInfo {
        name: "Amazon Bedrock Runtime".to_owned(),
        ..bedrock_provider(None)
    };
    assert_eq!(
        bedrock::provider_catalog(&runtime, None).unwrap(),
        runtime_catalog
    );
}

#[test]
fn classifies_only_refreshable_bedrock_auth_failures() {
    let cases = [
        (StatusCode::UNAUTHORIZED, "", true),
        (
            StatusCode::FORBIDDEN,
            "{\"__type\":\"ExpiredTokenException\"}",
            true,
        ),
        (StatusCode::FORBIDDEN, "UnrecognizedClientException", true),
        (StatusCode::FORBIDDEN, "InvalidClientTokenId", true),
        (StatusCode::FORBIDDEN, "AccessDeniedException", false),
        (StatusCode::TOO_MANY_REQUESTS, "ExpiredToken", false),
        (StatusCode::INTERNAL_SERVER_ERROR, "", false),
    ];
    for (status, body, expected) in cases {
        assert_eq!(
            bedrock::is_refreshable_response(status, body),
            expected,
            "{status}: {body}"
        );
    }
}

#[test]
fn expired_signature_has_actionable_guidance_only_for_401() {
    let message = bedrock::failure_message(
        StatusCode::UNAUTHORIZED,
        "Signature expired: 20260609T133205Z is now earlier than 20260614T062525Z",
    )
    .unwrap();
    assert!(
        message.starts_with(
            "Amazon Bedrock rejected the request because its AWS signature has expired."
        )
    );
    assert!(message.contains("`AWS_BEARER_TOKEN_BEDROCK`"));
    assert_eq!(
        bedrock::failure_message(
            StatusCode::UNAUTHORIZED,
            "The security token included in the request is invalid"
        ),
        None
    );
    assert_eq!(
        bedrock::failure_message(
            StatusCode::FORBIDDEN,
            "Signature expired: old is now earlier than new"
        ),
        None
    );
}

#[test]
fn credential_output_formats_expiration_and_sanitized_errors() {
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    for (optional_fields, refresh_after) in [
        (json!({}), 3_600),
        (json!({"SessionToken": null, "Expiration": null}), 3_600),
        (
            json!({"SessionToken": SECRET, "Expiration": "2023-11-14T23:13:20+00:00"}),
            3_300,
        ),
        (
            json!({"SessionToken": SECRET, "Expiration": "2023-11-14T22:18:20Z"}),
            0,
        ),
        (json!({"Expiration": "2023-11-14T22:15:20Z"}), 0),
    ] {
        let mut credentials = json!({"AccessKeyId": "exported", "SecretAccessKey": SECRET});
        credentials
            .as_object_mut()
            .unwrap()
            .extend(optional_fields.as_object().unwrap().clone());
        let mut process_output = credentials.clone();
        process_output["Version"] = json!(1);
        for output in [
            process_output,
            json!({"Credentials": credentials, "AssumedRoleUser": {"Arn": "test-role"}}),
        ] {
            let parsed = bedrock::parse_credential_export(output.to_string().as_bytes(), now)
                .expect("valid credentials should parse");
            assert_eq!(
                parsed,
                (
                    "exported".to_owned(),
                    SECRET.to_owned(),
                    optional_fields
                        .get("SessionToken")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    now + Duration::from_secs(refresh_after)
                )
            );
        }
    }

    let mut invalid_outputs = vec![(
        format!("{{\"SecretAccessKey\":\"{SECRET}\""),
        io::ErrorKind::InvalidData,
    )];
    for (field, value) in [
        ("AccessKeyId", None),
        ("SecretAccessKey", None),
        ("AccessKeyId", Some(json!(" "))),
        ("SecretAccessKey", Some(json!(" "))),
        ("AccessKeyId", Some(json!([SECRET]))),
        ("SecretAccessKey", Some(json!([SECRET]))),
        ("SessionToken", Some(json!([SECRET]))),
        ("Expiration", Some(json!([SECRET]))),
        ("Expiration", Some(json!(SECRET))),
    ] {
        let mut credentials: serde_json::Value = serde_json::from_slice(VALID_OUTPUT).unwrap();
        match value {
            Some(value) => credentials[field] = value,
            None => {
                credentials.as_object_mut().unwrap().remove(field);
            }
        }
        invalid_outputs.extend(
            [
                credentials.to_string(),
                json!({"Credentials": credentials}).to_string(),
            ]
            .map(|output| (output, io::ErrorKind::InvalidData)),
        );
    }
    for expiration in ["2023-11-14T22:13:20Z", "2023-11-14T22:00:00Z"] {
        let credentials =
            json!({"AccessKeyId": "exported", "SecretAccessKey": SECRET, "Expiration": expiration});
        invalid_outputs.extend(
            [
                credentials.to_string(),
                json!({"Credentials": credentials}).to_string(),
            ]
            .map(|output| (output, io::ErrorKind::Other)),
        );
    }
    for (output, expected_kind) in invalid_outputs {
        let error = bedrock::parse_credential_export(output.as_bytes(), now)
            .expect_err("invalid credentials should be rejected");
        assert_eq!(error.kind(), expected_kind, "{output}");
        assert!(!error.to_string().contains(SECRET));
    }
}

#[cfg(unix)]
mod process {
    use super::*;

    /// An exporter script that logs invocations, prints `output.json`, then follows `mode`.
    fn exporter_fixture(
        output: &[u8],
        mode: &str,
        timeout_ms: u64,
    ) -> io::Result<(tempfile::TempDir, bedrock::CredentialExporter)> {
        let directory = tempfile::tempdir()?;
        let exporter = exporter_in(&directory, output, mode, timeout_ms, "sh")?;
        Ok((directory, exporter))
    }

    fn exporter_in(
        directory: &tempfile::TempDir,
        output: &[u8],
        mode: &str,
        timeout_ms: u64,
        command: &str,
    ) -> io::Result<bedrock::CredentialExporter> {
        let output_path = directory.path().join("output.json");
        std::fs::write(&output_path, output)?;
        let script = directory.path().join("export.sh");
        std::fs::write(
            &script,
            r#"printf 'invoked\n' >> "$1"
printf '%s\n' "$4" >&2
cat "$2"
case "$3" in
  fail) exit 7 ;;
  wait) while :; do :; done ;;
esac
"#,
        )?;
        Ok(bedrock::CredentialExporter::new(
            command.to_owned(),
            vec![
                script.to_string_lossy().into_owned(),
                directory
                    .path()
                    .join("invocations")
                    .to_string_lossy()
                    .into_owned(),
                output_path.to_string_lossy().into_owned(),
                mode.to_owned(),
                SECRET.to_owned(),
            ],
            timeout_ms,
        ))
    }

    fn invocations(directory: &tempfile::TempDir) -> usize {
        std::fs::read_to_string(directory.path().join("invocations"))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn credential_export_process_errors_are_bounded_and_sanitized() -> io::Result<()> {
        let oversized = format!("{SECRET}{}", "x".repeat(MAX_CREDENTIAL_OUTPUT_BYTES));
        let directory = tempfile::tempdir()?;
        let missing = directory
            .path()
            .join("missing")
            .to_string_lossy()
            .into_owned();
        for (mode, output, timeout_ms, command, expected_kind) in [
            (
                "ok",
                VALID_OUTPUT,
                5_000,
                missing.as_str(),
                io::ErrorKind::NotFound,
            ),
            (
                "ok",
                VALID_OUTPUT,
                5_000,
                "./export-credentials",
                io::ErrorKind::InvalidInput,
            ),
            (
                "ok",
                VALID_OUTPUT,
                5_000,
                "scripts/.",
                io::ErrorKind::InvalidInput,
            ),
            ("fail", VALID_OUTPUT, 5_000, "sh", io::ErrorKind::Other),
            ("wait", VALID_OUTPUT, 100, "sh", io::ErrorKind::TimedOut),
            (
                "wait",
                oversized.as_bytes(),
                5_000,
                "sh",
                io::ErrorKind::InvalidData,
            ),
        ] {
            let case = tempfile::tempdir()?;
            let exporter = exporter_in(&case, output, mode, timeout_ms, command)?;
            let error = exporter
                .credentials()
                .await
                .expect_err("export should fail");
            assert_eq!(error.kind(), expected_kind, "{mode} {command}");
            assert!(!error.to_string().contains(SECRET));
            assert!(!exporter.debug().contains(SECRET));
            assert!(!exporter.is_cached().await);
        }
        Ok(())
    }

    #[tokio::test]
    async fn credential_export_reuses_cache_and_coalesces_refreshes() -> io::Result<()> {
        let mut output = VALID_OUTPUT.to_vec();
        output.resize(MAX_CREDENTIAL_OUTPUT_BYTES, b' ');
        let (directory, exporter) = exporter_fixture(&output, "success", 5_000)?;
        let original = (
            "exported".to_owned(),
            SECRET.to_owned(),
            Some(SECRET.to_owned()),
        );
        let (first, second) = tokio::join!(exporter.credentials(), exporter.credentials());
        assert_eq!([first?, second?], [original.clone(), original.clone()]);
        assert_eq!(invocations(&directory), 1);

        let rotated_output = json!({"AccessKeyId": "rotated", "SecretAccessKey": SECRET});
        std::fs::write(
            directory.path().join("output.json"),
            rotated_output.to_string(),
        )?;
        assert_eq!(exporter.credentials().await?, original);
        exporter.expire().await;
        let rotated = ("rotated".to_owned(), SECRET.to_owned(), None);
        assert_eq!(exporter.credentials().await?, rotated);
        assert_eq!(invocations(&directory), 2);

        // A restricted application policy must not launch the exporter during recovery.
        let controller = ra_http::NetworkPolicyController::default();
        let policy = controller.policy();
        assert!(controller.publish(
            policy.revision(),
            ra_http::DestinationPolicy::Restricted {
                allowed_hosts: Default::default(),
            },
        ));
        let denied = exporter
            .recover(None, policy.clone().for_current_account())
            .await
            .expect_err("restricted recovery must not launch the exporter");
        assert!(
            denied.contains("disabled by application network restrictions"),
            "{denied}"
        );
        assert_eq!(invocations(&directory), 2);

        // Concurrent recoveries share a single export.
        assert!(controller.publish(policy.revision(), ra_http::DestinationPolicy::Unrestricted));
        let account = policy.for_current_account();
        let (first, second) = tokio::join!(
            exporter.recover(None, account.clone()),
            exporter.recover(None, account.clone())
        );
        assert_eq!([first, second], [Ok(true), Ok(true)]);
        assert_eq!(invocations(&directory), 3);
        assert_eq!(exporter.credentials().await?, rotated);
        assert!(!exporter.debug().contains(SECRET));

        // A queued `aws login` recovery still runs and its failure empties the cache.
        let failed = exporter
            .recover(Some("not-aws"), account)
            .await
            .expect_err("only `aws` is accepted as a refresh command");
        assert!(
            failed.contains("AWS auth refresh command must be `aws`"),
            "{failed}"
        );
        assert!(!exporter.is_cached().await);
        assert_eq!(exporter.credentials().await?, rotated);
        Ok(())
    }
}

#[cfg(unix)]
mod recovery {
    use super::*;
    use ra_model::openai::responses::{
        ResponsesAuthFailure, ResponsesSignableRequest, ResponsesSignedRequest,
    };

    fn request() -> ResponsesSignableRequest {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        ResponsesSignableRequest::new(
            http::Method::POST,
            "https://bedrock-mantle.us-west-2.api.aws/openai/v1/responses".to_owned(),
            headers,
            br#"{"model":"openai.gpt-6.1-sol"}"#.to_vec(),
        )
    }

    fn authorization(signed: &ResponsesSignedRequest) -> String {
        signed.headers()[http::header::AUTHORIZATION]
            .to_str()
            .unwrap()
            .to_owned()
    }

    fn unauthorized(request: &ResponsesSignedRequest) -> ResponsesAuthFailure<'_> {
        ResponsesAuthFailure::Response {
            request,
            status: StatusCode::UNAUTHORIZED,
            body: "",
        }
    }

    #[tokio::test]
    async fn sigv4_recovery_reloads_the_signing_context() {
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("output.json");
        std::fs::write(&output, VALID_OUTPUT).unwrap();
        let script = directory.path().join("export.sh");
        std::fs::write(&script, "cat \"$1\"\n").unwrap();
        let exporter = bedrock::CredentialExporter::new(
            "sh".to_owned(),
            vec![
                script.to_string_lossy().into_owned(),
                output.to_string_lossy().into_owned(),
            ],
            5_000,
        );
        let signer = exporter
            .reloading_signer(vec!["stale-key".to_owned(), "fresh-key".to_owned()])
            .await;

        let first = signer.sign(request()).await.unwrap();
        assert!(authorization(&first).contains("Credential=stale-key/"));
        // A second attempt before recovery keeps the loaded context.
        let again = signer.sign(request()).await.unwrap();
        assert!(authorization(&again).contains("Credential=stale-key/"));

        assert!(signer.recover(unauthorized(&first)).await.unwrap());
        let retried = signer.sign(request()).await.unwrap();
        assert!(
            authorization(&retried).contains("Credential=fresh-key/"),
            "{}",
            authorization(&retried)
        );
    }

    #[tokio::test]
    async fn command_bearer_recovery_refreshes_only_the_rejected_token() {
        let directory = tempfile::tempdir().unwrap();
        // Prints token-N for the Nth run and fails from the third run on.
        std::fs::write(
            directory.path().join("token.sh"),
            "printf 'invoked\\n' >> invocations\n\
count=$(wc -l < invocations | tr -d ' ')\n\
[ \"$count\" -gt 2 ] && exit 9\n\
printf 'token-%s\\n' \"$count\"\n",
        )
        .unwrap();
        let signer = bedrock::command_signer(ModelProviderAuthInfo {
            command: "sh".to_owned(),
            args: vec!["token.sh".into()],
            timeout_ms: NonZeroU64::new(5_000).unwrap(),
            refresh_interval_ms: 0,
            cwd: directory.path().canonicalize().unwrap().try_into().unwrap(),
        });
        let invocations = || {
            std::fs::read_to_string(directory.path().join("invocations"))
                .unwrap()
                .lines()
                .count()
        };

        // Two concurrent requests are signed with the same token.
        let first = signer.sign(request()).await.unwrap();
        let second = signer.sign(request()).await.unwrap();
        assert_eq!(authorization(&first), "Bearer token-1");
        assert_eq!(authorization(&second), "Bearer token-1");
        assert_eq!(invocations(), 1);

        assert!(signer.recover(unauthorized(&first)).await.unwrap());
        assert_eq!(invocations(), 2);
        // The late 401 for the same rejected token reuses the refreshed token; the command,
        // which would now fail, does not run again.
        assert!(signer.recover(unauthorized(&second)).await.unwrap());
        assert_eq!(invocations(), 2);
        let retried = signer.sign(request()).await.unwrap();
        assert_eq!(authorization(&retried), "Bearer token-2");
    }
}
