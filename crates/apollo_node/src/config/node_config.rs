use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::sync::LazyLock;
use std::vec::Vec;

use apollo_batcher::config::BatcherConfig;
use apollo_batcher::VersionedConstantsOverrides;
use apollo_class_manager::config::FsClassManagerConfig;
use apollo_compile_to_casm::config::SierraCompilationConfig;
use apollo_config::dumping::{
    generate_struct_pointer,
    prepend_sub_config_name,
    ser_pointer_target_param,
    set_pointing_param_paths,
    ConfigPointers,
    Pointers,
    SerializeConfig,
};
use apollo_config::loading::load_and_process_config;
use apollo_config::{ConfigError, ParamPath, SerializedParam};
use apollo_consensus_manager::config::ConsensusManagerConfig;
use apollo_gateway::config::GatewayConfig;
use apollo_http_server::config::HttpServerConfig;
use apollo_infra_utils::path::resolve_project_relative_path;
use apollo_l1_endpoint_monitor::monitor::L1EndpointMonitorConfig;
use apollo_l1_gas_price::l1_gas_price_provider::L1GasPriceProviderConfig;
use apollo_l1_gas_price::l1_gas_price_scraper::L1GasPriceScraperConfig;
use apollo_l1_provider::l1_scraper::L1ScraperConfig;
use apollo_l1_provider::L1ProviderConfig;
use apollo_mempool::config::MempoolConfig;
use apollo_mempool_p2p::config::MempoolP2pConfig;
use apollo_monitoring_endpoint::config::MonitoringEndpointConfig;
use apollo_reverts::RevertConfig;
use apollo_state_sync::config::StateSyncConfig;
use clap::Command;
use papyrus_base_layer::ethereum_base_layer_contract::EthereumBaseLayerConfig;
use serde::{Deserialize, Serialize};
use validator::Validate;

use crate::config::component_config::ComponentConfig;
use crate::config::monitoring::MonitoringConfig;
use crate::version::VERSION_FULL;

// The path of the default configuration file, provided as part of the crate.
pub const CONFIG_SCHEMA_PATH: &str = "crates/apollo_node/resources/config_schema.json";
pub const CONFIG_SECRETS_SCHEMA_PATH: &str =
    "crates/apollo_node/resources/config_secrets_schema.json";
pub(crate) const POINTER_TARGET_VALUE: &str = "PointerTarget";

// Configuration parameters that share the same value across multiple components.
pub static CONFIG_POINTERS: LazyLock<ConfigPointers> = LazyLock::new(|| {
    let mut pointers = vec![
        (
            ser_pointer_target_param(
                "chain_id",
                &POINTER_TARGET_VALUE.to_string(),
                "The chain to follow. For more details see https://docs.starknet.io/documentation/architecture_and_concepts/Blocks/transactions/#chain-id.",
            ),
            set_pointing_param_paths(&[
                "batcher_config.block_builder_config.chain_info.chain_id",
                "batcher_config.storage.db_config.chain_id",
                "consensus_manager_config.context_config.chain_id",
                "consensus_manager_config.network_config.chain_id",
                "gateway_config.chain_info.chain_id",
                "l1_scraper_config.chain_id",
                "l1_gas_price_scraper_config.chain_id",
                "mempool_p2p_config.network_config.chain_id",
                "state_sync_config.storage_config.db_config.chain_id",
                "state_sync_config.network_config.chain_id",
                "state_sync_config.rpc_config.chain_id",
            ]),
        ),
        (
            ser_pointer_target_param(
                "eth_fee_token_address",
                &POINTER_TARGET_VALUE.to_string(),
                "Address of the ETH fee token.",
            ),
            set_pointing_param_paths(&[
                "batcher_config.block_builder_config.chain_info.fee_token_addresses.\
                 eth_fee_token_address",
                "gateway_config.chain_info.fee_token_addresses.eth_fee_token_address",
                "state_sync_config.rpc_config.execution_config.eth_fee_contract_address",
            ]),
        ),
        (
            ser_pointer_target_param(
                "starknet_url",
                &POINTER_TARGET_VALUE.to_string(),
                "URL for communicating with Starknet.",
            ),
            set_pointing_param_paths(&[
                "state_sync_config.central_sync_client_config.central_source_config.starknet_url",
                "state_sync_config.rpc_config.starknet_url",
            ]),
        ),
        (
            ser_pointer_target_param(
                "strk_fee_token_address",
                &POINTER_TARGET_VALUE.to_string(),
                "Address of the STRK fee token.",
            ),
            set_pointing_param_paths(&[
                "batcher_config.block_builder_config.chain_info.fee_token_addresses.\
                 strk_fee_token_address",
                "gateway_config.chain_info.fee_token_addresses.strk_fee_token_address",
                "state_sync_config.rpc_config.execution_config.strk_fee_contract_address",
            ]),
        ),
        (
            ser_pointer_target_param(
                "validator_id",
                &POINTER_TARGET_VALUE.to_string(),
                "The ID of the validator. \
                 Also the address of this validator as a starknet contract.",
            ),
            set_pointing_param_paths(&["consensus_manager_config.consensus_config.validator_id"]),
        ),
        (
            ser_pointer_target_param(
                "recorder_url",
                &POINTER_TARGET_VALUE.to_string(),
                "The URL of the Pythonic cende_recorder",
            ),
            set_pointing_param_paths(&[
                "consensus_manager_config.cende_config.recorder_url",
                "batcher_config.pre_confirmed_cende_config.recorder_url",
            ]),
        ),
    ];
    let mut common_execution_config = generate_struct_pointer(
        "versioned_constants_overrides".to_owned(),
        &VersionedConstantsOverrides::default(),
        set_pointing_param_paths(&[
            "batcher_config.block_builder_config.versioned_constants_overrides",
            "gateway_config.stateful_tx_validator_config.versioned_constants_overrides",
        ]),
    );
    pointers.append(&mut common_execution_config);

    let mut common_execution_config = generate_struct_pointer(
        "revert_config".to_owned(),
        &RevertConfig::default(),
        set_pointing_param_paths(&[
            "state_sync_config.revert_config",
            "consensus_manager_config.revert_config",
        ]),
    );
    pointers.append(&mut common_execution_config);
    pointers
});

// Parameters that should 1) not be pointers, and 2) have a name matching a pointer target param.
pub static CONFIG_NON_POINTERS_WHITELIST: LazyLock<Pointers> =
    LazyLock::new(HashSet::<ParamPath>::new);

/// The configurations of the various components of the node.
#[derive(Debug, Default, Deserialize, Serialize, Clone, PartialEq, Validate)]
pub struct SequencerNodeConfig {
    // Infra related configs.
    #[validate]
    pub components: ComponentConfig,
    #[validate]
    pub monitoring_config: MonitoringConfig,

    // Business-logic component configs.
    #[validate]
    pub base_layer_config: EthereumBaseLayerConfig,
    #[validate]
    pub batcher_config: BatcherConfig,
    #[validate]
    pub class_manager_config: FsClassManagerConfig,
    #[validate]
    pub consensus_manager_config: ConsensusManagerConfig,
    #[validate]
    pub gateway_config: GatewayConfig,
    #[validate]
    pub http_server_config: HttpServerConfig,
    #[validate]
    pub compiler_config: SierraCompilationConfig,
    #[validate]
    pub l1_endpoint_monitor_config: L1EndpointMonitorConfig,
    #[validate]
    pub l1_provider_config: L1ProviderConfig,
    #[validate]
    pub l1_gas_price_provider_config: L1GasPriceProviderConfig,
    #[validate]
    pub l1_scraper_config: L1ScraperConfig,
    #[validate]
    pub mempool_config: MempoolConfig,
    #[validate]
    pub l1_gas_price_scraper_config: L1GasPriceScraperConfig,
    #[validate]
    pub mempool_p2p_config: MempoolP2pConfig,
    #[validate]
    pub monitoring_endpoint_config: MonitoringEndpointConfig,
    #[validate]
    pub state_sync_config: StateSyncConfig,
}

impl SerializeConfig for SequencerNodeConfig {
    fn dump(&self) -> BTreeMap<ParamPath, SerializedParam> {
        let sub_configs = vec![
            prepend_sub_config_name(self.components.dump(), "components"),
            prepend_sub_config_name(self.monitoring_config.dump(), "monitoring_config"),
            prepend_sub_config_name(self.base_layer_config.dump(), "base_layer_config"),
            prepend_sub_config_name(self.batcher_config.dump(), "batcher_config"),
            prepend_sub_config_name(self.class_manager_config.dump(), "class_manager_config"),
            prepend_sub_config_name(
                self.consensus_manager_config.dump(),
                "consensus_manager_config",
            ),
            prepend_sub_config_name(self.gateway_config.dump(), "gateway_config"),
            prepend_sub_config_name(self.http_server_config.dump(), "http_server_config"),
            prepend_sub_config_name(self.compiler_config.dump(), "compiler_config"),
            prepend_sub_config_name(self.mempool_config.dump(), "mempool_config"),
            prepend_sub_config_name(self.mempool_p2p_config.dump(), "mempool_p2p_config"),
            prepend_sub_config_name(
                self.monitoring_endpoint_config.dump(),
                "monitoring_endpoint_config",
            ),
            prepend_sub_config_name(self.state_sync_config.dump(), "state_sync_config"),
            prepend_sub_config_name(
                self.l1_endpoint_monitor_config.dump(),
                "l1_endpoint_monitor_config",
            ),
            prepend_sub_config_name(self.l1_provider_config.dump(), "l1_provider_config"),
            prepend_sub_config_name(self.l1_scraper_config.dump(), "l1_scraper_config"),
            prepend_sub_config_name(
                self.l1_gas_price_provider_config.dump(),
                "l1_gas_price_provider_config",
            ),
            prepend_sub_config_name(
                self.l1_gas_price_scraper_config.dump(),
                "l1_gas_price_scraper_config",
            ),
        ];

        sub_configs.into_iter().flatten().collect()
    }
}

impl SequencerNodeConfig {
    /// Creates a config object, using the config schema and provided resources.
    pub fn load_and_process(args: Vec<String>) -> Result<Self, ConfigError> {
        let config_file_name = &resolve_project_relative_path(CONFIG_SCHEMA_PATH)?;
        let default_config_file = File::open(config_file_name)?;
        load_and_process_config(default_config_file, node_command(), args, true)
    }
}

/// The command line interface of this node.
pub(crate) fn node_command() -> Command {
    Command::new("Sequencer")
        .version(VERSION_FULL)
        .about("A Starknet sequencer node written in Rust.")
}
