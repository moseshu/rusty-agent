// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Bedrock Runtime endpoint and service name.

use ra_aws_auth::AwsAuthConfig;
use ra_config::ModelProviderAwsAuthInfo;

use super::mantle::region_from_config;

const BEDROCK_RUNTIME_SERVICE_NAME: &str = "bedrock";

pub(super) fn aws_auth_config(aws: &ModelProviderAwsAuthInfo) -> AwsAuthConfig {
    AwsAuthConfig {
        profile: aws.profile.clone(),
        region: region_from_config(aws),
        service: BEDROCK_RUNTIME_SERVICE_NAME.to_string(),
    }
}

pub(super) fn base_url(region: &str) -> String {
    format!("https://bedrock-runtime.{region}.amazonaws.com/openai/v1")
}
