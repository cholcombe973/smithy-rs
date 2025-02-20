/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 * SPDX-License-Identifier: Apache-2.0
 */

//! Credential provider augmentation through the AWS Security Token Service (STS).

pub(crate) mod util;

pub use assume_role::{AssumeRoleProvider, AssumeRoleProviderBuilder};

mod assume_role;

use crate::connector::expect_connector;
use aws_credential_types::cache::CredentialsCache;
use aws_sdk_sts::config::Builder as StsConfigBuilder;
use aws_smithy_types::retry::RetryConfig;

impl crate::provider_config::ProviderConfig {
    pub(crate) fn sts_client_config(&self) -> StsConfigBuilder {
        let mut builder = aws_sdk_sts::Config::builder()
            .http_connector(expect_connector(self.connector(&Default::default())))
            .retry_config(RetryConfig::standard())
            .region(self.region())
            .credentials_cache(CredentialsCache::no_caching());
        builder.set_sleep_impl(self.sleep());
        builder
    }
}
