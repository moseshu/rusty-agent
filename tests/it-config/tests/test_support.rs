// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use it_config::support::*;
use ra_config::CloudRequirementsFragment;

#[test]
fn adds_enterprise_requirements_in_order() {
    let bundle = CloudConfigBundleFixture::enterprise_requirement("first")
        .add_enterprise_requirement("second")
        .into_bundle();

    assert_eq!(
        bundle.requirements_toml.enterprise_managed,
        vec![
            CloudRequirementsFragment {
                id: "req_1".to_string(),
                name: "Base requirements".to_string(),
                contents: "first".to_string(),
            },
            CloudRequirementsFragment {
                id: "req_2".to_string(),
                name: "Requirements 2".to_string(),
                contents: "second".to_string(),
            },
        ]
    );
}
