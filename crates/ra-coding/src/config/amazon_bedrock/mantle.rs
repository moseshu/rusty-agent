// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Bedrock Mantle endpoint, service name, and supported regions.

use ra_aws_auth::AwsAuthConfig;
use ra_config::ModelProviderAwsAuthInfo;
use ra_core::error::{Error, Result};

const BEDROCK_MANTLE_SERVICE_NAME: &str = "bedrock-mantle";
const BEDROCK_MANTLE_SUPPORTED_REGIONS: [&str; 12] = [
    "us-east-2",
    "us-east-1",
    "us-west-2",
    "ap-southeast-3",
    "ap-south-1",
    "ap-northeast-1",
    "eu-central-1",
    "eu-west-1",
    "eu-west-2",
    "eu-south-1",
    "eu-north-1",
    "sa-east-1",
];
const BEDROCK_GOV_CLOUD_SUPPORTED_REGIONS: [&str; 2] = ["us-gov-east-1", "us-gov-west-1"];

pub(super) fn aws_auth_config(aws: &ModelProviderAwsAuthInfo) -> AwsAuthConfig {
    AwsAuthConfig {
        profile: aws.profile.clone(),
        region: region_from_config(aws),
        service: BEDROCK_MANTLE_SERVICE_NAME.to_string(),
    }
}

pub(super) fn region_from_config(aws: &ModelProviderAwsAuthInfo) -> Option<String> {
    aws.region
        .as_deref()
        .map(str::trim)
        .filter(|region| !region.is_empty())
        .map(str::to_string)
}

/// Returns whether Amazon Bedrock Mantle is available in `region`.
pub(crate) fn is_supported_amazon_bedrock_region(region: &str) -> bool {
    BEDROCK_MANTLE_SUPPORTED_REGIONS.contains(&region) || is_amazon_bedrock_gov_cloud_region(region)
}

/// Returns whether `region` is a supported Amazon Bedrock `GovCloud` region.
pub(crate) fn is_amazon_bedrock_gov_cloud_region(region: &str) -> bool {
    BEDROCK_GOV_CLOUD_SUPPORTED_REGIONS.contains(&region)
}

pub(super) fn base_url(region: &str) -> Result<String> {
    if is_supported_amazon_bedrock_region(region) {
        Ok(format!("https://bedrock-mantle.{region}.api.aws/openai/v1"))
    } else {
        Err(Error::config(format!(
            "Amazon Bedrock does not support region `{region}`"
        )))
    }
}
