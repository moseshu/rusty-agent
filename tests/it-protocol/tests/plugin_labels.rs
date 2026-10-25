// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Closed wire-policy labels and the shared product restriction contract.

use ra_coding::{
    plugins::marketplace::{MarketplacePluginAuthPolicy, MarketplacePluginInstallPolicy},
    skills::SkillProduct,
};
use ra_protocol::server::protocol::{PluginAuthPolicy, PluginInstallPolicy, Product};
use serde_json::json;

#[test]
fn installation_policy_labels_round_trip_and_domain_defaults_match() {
    for (domain, wire, label) in [
        (
            MarketplacePluginInstallPolicy::NotAvailable,
            PluginInstallPolicy::NotAvailable,
            "NOT_AVAILABLE",
        ),
        (
            MarketplacePluginInstallPolicy::Available,
            PluginInstallPolicy::Available,
            "AVAILABLE",
        ),
        (
            MarketplacePluginInstallPolicy::InstalledByDefault,
            PluginInstallPolicy::InstalledByDefault,
            "INSTALLED_BY_DEFAULT",
        ),
    ] {
        assert_eq!(PluginInstallPolicy::from(domain), wire);
        assert_eq!(serde_json::to_value(wire).unwrap(), json!(label));
        assert_eq!(
            serde_json::from_value::<PluginInstallPolicy>(json!(label)).unwrap(),
            wire
        );
    }
    assert_eq!(
        MarketplacePluginInstallPolicy::default(),
        MarketplacePluginInstallPolicy::Available
    );
    for label in ["available", "UNKNOWN", "", "INSTALLED"] {
        assert!(serde_json::from_value::<PluginInstallPolicy>(json!(label)).is_err());
    }
}

#[test]
fn authentication_policy_labels_round_trip_and_domain_default_matches() {
    for (domain, wire, label) in [
        (
            MarketplacePluginAuthPolicy::OnInstall,
            PluginAuthPolicy::OnInstall,
            "ON_INSTALL",
        ),
        (
            MarketplacePluginAuthPolicy::OnUse,
            PluginAuthPolicy::OnUse,
            "ON_USE",
        ),
    ] {
        assert_eq!(PluginAuthPolicy::from(domain), wire);
        assert_eq!(serde_json::to_value(wire).unwrap(), json!(label));
        assert_eq!(
            serde_json::from_value::<PluginAuthPolicy>(json!(label)).unwrap(),
            wire
        );
    }
    assert_eq!(
        MarketplacePluginAuthPolicy::default(),
        MarketplacePluginAuthPolicy::OnInstall
    );
    for label in ["on_use", "UNKNOWN", ""] {
        assert!(serde_json::from_value::<PluginAuthPolicy>(json!(label)).is_err());
    }
}

#[test]
fn product_and_skill_labels_share_aliases_and_canonical_serialization() {
    for (product, label, platform) in [
        (Product::Chatgpt, "chatgpt", "chat"),
        (Product::Codex, "codex", "codex"),
        (Product::Atlas, "atlas", "atlas"),
    ] {
        let skill_product: SkillProduct = product;
        assert_eq!(skill_product, product);
        assert_eq!(product.to_app_platform(), platform);
        assert_eq!(serde_json::to_value(product).unwrap(), json!(label));
        for spelling in [label.to_owned(), label.to_ascii_uppercase()] {
            assert_eq!(
                serde_json::from_value::<Product>(json!(spelling)).unwrap(),
                product
            );
            assert_eq!(
                serde_json::from_value::<SkillProduct>(json!(spelling)).unwrap(),
                product
            );
        }
        assert_eq!(
            Product::from_session_source_name(&format!(" {} ", label.to_uppercase())),
            Some(product)
        );
    }
    for label in ["", "Codex", "unknown", "chat", " codex "] {
        assert!(serde_json::from_value::<Product>(json!(label)).is_err());
    }
    assert_eq!(Product::from_session_source_name("unknown"), None);
}

#[test]
fn shared_empty_product_restrictions_remain_unrestricted() {
    assert!(Product::Codex.matches_product_restriction(&[]));
    assert!(Product::Codex.matches_product_restriction(&[Product::Codex, Product::Atlas]));
    assert!(!Product::Codex.matches_product_restriction(&[Product::Chatgpt]));
}
