// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetworkMode;
    use crate::config::NetworkProxyConfig;
    use pretty_assertions::assert_eq;
    use rama_http::Body;
    use rama_http::Method;
    use tempfile::NamedTempFile;

    fn base_config() -> NetworkProxyConfig {
        NetworkProxyConfig {
            mitm: true,
            mode: NetworkMode::Limited,
            ..NetworkProxyConfig::default()
        }
    }

    fn github_hook() -> MitmHookConfig {
        MitmHookConfig {
            host: "api.github.com".to_string(),
            matcher: MitmHookMatchConfig {
                methods: vec!["POST".to_string(), "PUT".to_string()],
                path_prefixes: vec!["/repos/openai/".to_string()],
                ..MitmHookMatchConfig::default()
            },
            actions: MitmHookActionsConfig {
                strip_request_headers: vec!["authorization".to_string()],
                inject_request_headers: vec![InjectedHeaderConfig {
                    name: "authorization".to_string(),
                    secret_env_var: Some("CODEX_GITHUB_TOKEN".to_string()),
                    secret_file: None,
                    prefix: Some("Bearer ".to_string()),
                }],
            },
        }
    }

    #[test]
    fn validate_requires_mitm_for_hooks() {
        let mut config = base_config();
        config.mitm = false;
        config.mitm_hooks = vec![github_hook()];

        let err = validate_mitm_hook_config(&config).expect_err("hooks require mitm");
        assert!(
            err.to_string()
                .contains("network.mitm_hooks requires network.mitm = true")
        );
    }

    #[test]
    fn validate_allows_hooks_in_full_mode() {
        let mut config = base_config();
        config.mode = NetworkMode::Full;
        config.mitm_hooks = vec![github_hook()];

        validate_mitm_hook_config(&config).expect("hooks should be allowed in full mode");
    }

    #[test]
    fn validate_rejects_body_matchers_for_now() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.body = Some(MitmHookBodyConfig(serde_json::json!({
            "repository": "openai/codex"
        })));
        config.mitm_hooks = vec![hook];

        let err = validate_mitm_hook_config(&config).expect_err("body matchers are reserved");
        assert!(err.to_string().contains("match.body is reserved"));
    }

    #[test]
    fn validate_rejects_relative_secret_file() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.actions.inject_request_headers[0].secret_env_var = None;
        hook.actions.inject_request_headers[0].secret_file = Some("token.txt".to_string());
        config.mitm_hooks = vec![hook];

        let err = validate_mitm_hook_config(&config).expect_err("secret file must be absolute");
        assert!(format!("{err:#}").contains("secret_file must be an absolute path"));
    }

    #[test]
    fn validate_rejects_dual_secret_sources() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.actions.inject_request_headers[0].secret_file = Some("/tmp/github-token".to_string());
        config.mitm_hooks = vec![hook];

        let err = validate_mitm_hook_config(&config).expect_err("dual secret sources invalid");
        assert!(format!("{err:#}").contains("exactly one of secret_env_var or secret_file"));
    }

    #[test]
    fn compile_resolves_env_backed_injected_headers() {
        let mut config = base_config();
        config.mitm_hooks = vec![github_hook()];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |name| (name == "CODEX_GITHUB_TOKEN").then(|| "ghp-secret".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();

        let compiled = hooks.get("api.github.com").unwrap();
        assert_eq!(compiled.len(), 1);
        assert_eq!(
            compiled[0].actions.inject_request_headers[0].source,
            SecretSource::EnvVar("CODEX_GITHUB_TOKEN".to_string())
        );
        assert_eq!(
            compiled[0].actions.inject_request_headers[0].value,
            HeaderValue::from_static("Bearer ghp-secret")
        );
    }

    #[test]
    fn compile_resolves_file_backed_injected_headers() {
        let secret_file = NamedTempFile::new().unwrap();
        std::fs::write(secret_file.path(), "ghp-file-secret\n").unwrap();

        let mut config = base_config();
        let mut hook = github_hook();
        hook.actions.inject_request_headers[0].secret_env_var = None;
        hook.actions.inject_request_headers[0].secret_file =
            Some(secret_file.path().display().to_string());
        config.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks(&config).unwrap();
        let compiled = hooks.get("api.github.com").unwrap();
        assert_eq!(
            compiled[0].actions.inject_request_headers[0].value,
            HeaderValue::from_static("Bearer ghp-file-secret")
        );
    }

    #[test]
    fn evaluate_returns_first_matching_hook() {
        let mut config = base_config();
        let mut first = github_hook();
        first.matcher.path_prefixes = vec!["/repos/openai/".to_string()];
        let mut second = github_hook();
        second.actions.inject_request_headers[0].prefix = Some("Token ".to_string());
        config.mitm_hooks = vec![first, second];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues")
            .header("x-trace", "1")
            .body(Body::empty())
            .unwrap();

        let evaluation = evaluate_mitm_hooks(&hooks, "api.github.com", &req);
        let HookEvaluation::Matched { actions } = evaluation else {
            panic!("expected a matching hook");
        };

        assert_eq!(
            actions.inject_request_headers[0].value,
            HeaderValue::from_static("Bearer abc")
        );
    }

    #[test]
    fn evaluate_matches_query_and_header_constraints() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.query = BTreeMap::from([(
            "state".to_string(),
            vec!["open".to_string(), "triage".to_string()],
        )]);
        hook.matcher.headers = BTreeMap::from([(
            "x-github-api-version".to_string(),
            vec!["2022-11-28".to_string()],
        )]);
        config.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues?state=open&per_page=10")
            .header("x-github-api-version", "2022-11-28")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &req),
            HookEvaluation::Matched {
                actions: hooks.get("api.github.com").unwrap()[0].actions.clone(),
            }
        );
    }

    #[test]
    fn evaluate_matches_wildcard_path_query_and_header_constraints() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.path_prefixes = vec!["pattern:/repos/*/codex/issues*".to_string()];
        hook.matcher.query =
            BTreeMap::from([("state".to_string(), vec!["pattern:op*".to_string()])]);
        hook.matcher.headers = BTreeMap::from([(
            "x-github-api-version".to_string(),
            vec!["pattern:2022*preview".to_string()],
        )]);
        config.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues?state=open")
            .header("x-github-api-version", "2022-11-28-preview")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &req),
            HookEvaluation::Matched {
                actions: hooks.get("api.github.com").unwrap()[0].actions.clone(),
            }
        );
    }

    #[test]
    fn validate_rejects_invalid_wildcard_path_pattern() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.path_prefixes = vec!["pattern:/repos/[".to_string()];
        config.mitm_hooks = vec![hook];

        let err = validate_mitm_hook_config(&config).expect_err("invalid glob should fail");
        assert!(format!("{err:#}").contains("invalid glob pattern"));
    }

    #[test]
    fn evaluate_path_wildcard_does_not_cross_segment_boundaries() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.path_prefixes = vec!["pattern:/repos/*/codex/issues*".to_string()];
        config.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let nested_req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/private/codex/issues")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &nested_req),
            HookEvaluation::HookedHostNoMatch
        );
    }

    #[test]
    fn evaluate_rejects_paths_that_upstream_may_normalize() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.methods = vec!["GET".to_string()];
        hook.matcher.path_prefixes = vec!["pattern:/openai/openai/**".to_string()];
        config.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let paths = [
            "/openai/openai/../codex",
            "/openai/openai/%2e%2e/codex",
            "/openai/openai/%2E%2E/codex",
            "/openai/openai/.%2e/codex",
            "/openai/openai/%2e./codex",
            "/openai/openai/%252e%252e/codex",
            "/openai/openai/%2f..%2fcodex",
            "/openai/openai/%5c..%5ccodex",
            "/openai/openai/%2e%2e/%2e%2e/microsoft/vscode",
        ];
        let actual = paths
            .iter()
            .map(|path| {
                let req = Request::builder()
                    .method(Method::GET)
                    .uri(*path)
                    .body(Body::empty())
                    .unwrap();
                evaluate_mitm_hooks(&hooks, "api.github.com", &req)
            })
            .collect::<Vec<_>>();

        assert_eq!(actual, vec![HookEvaluation::HookedHostNoMatch; paths.len()]);
    }

    #[test]
    fn evaluate_treats_glob_metacharacters_as_literal_without_glob_prefix() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.path_prefixes = vec!["/repos/[draft]/".to_string()];
        hook.matcher.query = BTreeMap::from([("state".to_string(), vec!["op*".to_string()])]);
        hook.matcher.headers = BTreeMap::from([(
            "x-github-api-version".to_string(),
            vec!["2022-11-28[preview]".to_string()],
        )]);
        config.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let exact_req = Request::builder()
            .method(Method::POST)
            .uri("/repos/[draft]/codex/issues?state=op*")
            .header("x-github-api-version", "2022-11-28[preview]")
            .body(Body::empty())
            .unwrap();
        let non_literal_req = Request::builder()
            .method(Method::POST)
            .uri("/repos/draft/codex/issues?state=open")
            .header("x-github-api-version", "2022-11-28-preview")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &exact_req),
            HookEvaluation::Matched {
                actions: hooks.get("api.github.com").unwrap()[0].actions.clone(),
            }
        );
        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &non_literal_req),
            HookEvaluation::HookedHostNoMatch
        );
    }

    #[test]
    fn evaluate_allows_literal_values_with_reserved_prefixes() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.query =
            BTreeMap::from([("state".to_string(), vec!["literal:pattern:*".to_string()])]);
        hook.matcher.headers = BTreeMap::from([(
            "x-github-api-version".to_string(),
            vec!["literal:pattern:*".to_string()],
        )]);
        config.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let exact_req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues?state=pattern%3A%2A")
            .header("x-github-api-version", "pattern:*")
            .body(Body::empty())
            .unwrap();
        let non_literal_req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues?state=pattern%3Aopen")
            .header("x-github-api-version", "pattern:preview")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &exact_req),
            HookEvaluation::Matched {
                actions: hooks.get("api.github.com").unwrap()[0].actions.clone(),
            }
        );
        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &non_literal_req),
            HookEvaluation::HookedHostNoMatch
        );
    }

    #[test]
    fn evaluate_returns_hooked_host_no_match_when_query_constraint_fails() {
        let mut config = base_config();
        let mut hook = github_hook();
        hook.matcher.query = BTreeMap::from([("state".to_string(), vec!["open".to_string()])]);
        config.mitm_hooks = vec![hook];

        let hooks = compile_mitm_hooks_with_resolvers(
            &config,
            |_| Some("abc".to_string()),
            |_| Err(anyhow!("unexpected file lookup")),
        )
        .unwrap();
        let req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues?state=closed")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&hooks, "api.github.com", &req),
            HookEvaluation::HookedHostNoMatch
        );
    }

    #[test]
    fn evaluate_returns_no_hooks_for_unconfigured_host() {
        let req = Request::builder()
            .method(Method::POST)
            .uri("/repos/openai/codex/issues")
            .body(Body::empty())
            .unwrap();

        assert_eq!(
            evaluate_mitm_hooks(&MitmHooksByHost::new(), "api.github.com", &req),
            HookEvaluation::NoHooksForHost
        );
    }
}
