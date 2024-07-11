// Copyright 2024 The Tari Project
// SPDX-License-Identifier: BSD-3-Clause

use config::Config;

use serde::{Deserialize, Serialize};
use tari_common::{
    configuration::{CommonConfig, Network},
    ConfigurationError, DefaultConfigLoader,
};
use tari_p2p::PeerSeedsConfig;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ApplicationConfig {
    pub common: CommonConfig,
    pub peers_seed: PeerSeedsConfig,
    pub network: Network,
}

impl ApplicationConfig {
    pub fn init(cfg: &Config) -> Result<Self, ConfigurationError> {
        let config = Self {
            common: CommonConfig::load_from(cfg)?,
            peers_seed: PeerSeedsConfig::load_from(cfg)?,
            network: cfg.get("network")?,
        };

        Ok(config)
    }
}
