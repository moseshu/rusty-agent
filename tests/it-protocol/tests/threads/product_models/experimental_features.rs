//! Feature listing and process-wide runtime defaults through the coding app server.
use super::*;

fn feature<'a>(result: &'a Value, name: &str) -> &'a Value {
    result["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|record| record["name"] == name)
        .unwrap_or_else(|| panic!("missing feature {name}"))
}

#[tokio::test]
async fn experimental_feature_list_reports_metadata_and_validates_pagination() {
    let product = ProductModels::new();
    product.write("config.toml", "model = 'gpt-5'");
    let server = product.server_with_feature_config(product.parts().2).await;
    let mut client = server.connect(1).await;
    let list = "experimentalFeature/list";
    let full = server.call(&mut client, list, json!({})).await;
    let entries = full["data"].as_array().unwrap();
    let beta = entries.iter().find(|item| item["stage"] == "beta").unwrap();
    assert!(beta["displayName"].is_string());
    assert!(beta["description"].is_string());
    assert!(beta["announcement"].is_string());
    assert_eq!(
        feature(&full, "api_key_model_discovery"),
        &json!({
            "name": "api_key_model_discovery",
            "stage": "stable",
            "displayName": null,
            "description": null,
            "announcement": null,
            "enabled": true,
            "defaultEnabled": true,
        })
    );
    assert!(full["nextCursor"].is_null());
    let first = server.call(&mut client, list, json!({"limit": 0})).await;
    assert_eq!(first, json!({"data": [entries[0]], "nextCursor": "1"}));
    let second = server
        .call(
            &mut client,
            list,
            json!({"cursor": first["nextCursor"], "limit": 2}),
        )
        .await;
    assert_eq!(second, json!({"data": entries[1..3], "nextCursor": "3"}));
    let last = server
        .call(
            &mut client,
            list,
            json!({"cursor": entries.len().to_string()}),
        )
        .await;
    assert_eq!(last, json!({"data": [], "nextCursor": null}));
    for (params, message) in [
        (json!({"cursor": "invalid"}), "invalid cursor: invalid"),
        (
            json!({"cursor": (entries.len() + 1).to_string()}),
            "exceeds total feature flags",
        ),
        (json!({"threadId": "unknown"}), "thread not found: unknown"),
    ] {
        let error = server.call_error(&mut client, list, params).await;
        assert_eq!(error["code"], -32600);
        assert!(
            error["message"].as_str().unwrap().contains(message),
            "{error}"
        );
    }
}

#[tokio::test]
async fn experimental_feature_defaults_respect_explicit_layers_and_stay_in_process() {
    let product = ProductModels::new();
    let workspace = product.workspace.path().canonicalize().unwrap();
    let config = format!(
        "model = 'gpt-5'\n[features]\nmemories = false\n[projects.{workspace:?}]\ntrust_level = 'trusted'\n"
    );
    product.write("config.toml", &config);
    product.write("requirements.toml", "[features]\nremote_plugin = false");
    std::fs::create_dir(workspace.join(".rusty-agent")).unwrap();
    let project_config = workspace.join(".rusty-agent/config.toml");
    std::fs::write(&project_config, "[features]\nauth_elicitation = true").unwrap();
    let builder = product.parts().2;
    let server = product
        .server_with_feature_config(
            builder
                .clone()
                .cli_overrides(vec![("features.mentions_v2".into(), true.into())]),
        )
        .await;
    let mut client = server.connect(1).await;
    let (thread_id, _) = start(&server, &mut client, json!({})).await;
    let set = "experimentalFeature/enablement/set";
    let update = json!({
        "auth_elicitation": false,
        "memories": true,
        "mentions_v2": false,
        "remote_plugin": true,
        "tool_suggest": true
    });
    let mut input = update.clone();
    input["unknown_feature"] = json!(true);
    input["plugins"] = json!(false);
    input["tool_call_mcp_elicitation"] = json!(false);
    let accepted = server
        .call(&mut client, set, json!({"enablement": input}))
        .await;
    assert_eq!(accepted, json!({"enablement": update}));
    let next = json!({"enablement": {"remote_control": false}});
    assert_eq!(server.call(&mut client, set, next.clone()).await, next);
    for enablement in [json!({}), json!({"unknown_feature": true})] {
        let no_op = server
            .call(&mut client, set, json!({"enablement":enablement}))
            .await;
        assert_eq!(no_op, json!({"enablement": {}}));
    }
    let list = "experimentalFeature/list";
    let global = server.call(&mut client, list, json!({})).await;
    assert_eq!(feature(&global, "auth_elicitation")["enabled"], false);
    assert_eq!(feature(&global, "memories")["enabled"], false);
    assert_eq!(feature(&global, "remote_plugin")["enabled"], false);
    assert_eq!(feature(&global, "mentions_v2")["enabled"], false);
    assert_eq!(feature(&global, "tool_suggest")["enabled"], true);

    let thread_params = json!({"threadId": thread_id});
    let scoped = server.call(&mut client, list, thread_params.clone()).await;
    assert_eq!(feature(&scoped, "auth_elicitation")["enabled"], true);
    assert_eq!(feature(&scoped, "mentions_v2")["enabled"], true);
    assert_eq!(feature(&scoped, "tool_suggest")["enabled"], true);

    let read = server
        .call(&mut client, "config/read", json!({"includeLayers":true}))
        .await;
    assert_eq!(read["config"]["features"]["auth_elicitation"], false);
    assert_eq!(read["config"]["features"]["tool_suggest"], true);
    assert!(read["origins"].get("features.tool_suggest").is_none());
    let scoped_read = server
        .call(&mut client, "config/read", json!({"cwd": workspace}))
        .await;
    assert_eq!(scoped_read["config"]["features"]["auth_elicitation"], true);

    std::fs::write(&project_config, "[features]\nauth_elicitation = false").unwrap();
    let updated = server.call(&mut client, list, thread_params.clone()).await;
    assert_eq!(feature(&updated, "auth_elicitation")["enabled"], false);
    unload(&server, &mut client, &thread_id).await;
    let error = server.call_error(&mut client, list, thread_params).await;
    assert_eq!(error["message"], format!("thread not found: {thread_id}"));
    assert_eq!(
        std::fs::read_to_string(product.home.path().join("config.toml")).unwrap(),
        config
    );
    let restarted = product.server_with_feature_config(builder).await;
    let mut other = restarted.connect(1).await;
    let reset = restarted.call(&mut other, list, json!({})).await;
    assert_eq!(feature(&reset, "auth_elicitation")["enabled"], true);
}
