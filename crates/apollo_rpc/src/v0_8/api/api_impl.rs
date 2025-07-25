use std::sync::Arc;

use apollo_class_manager_types::SharedClassManagerClient;
use apollo_rpc_execution::objects::{FeeEstimation, PendingData as ExecutionPendingData};
use apollo_rpc_execution::{
    estimate_fee as exec_estimate_fee,
    execute_call,
    execution_utils,
    simulate_transactions as exec_simulate_transactions,
    ExecutableTransactionInput,
    ExecutionConfig,
};
use apollo_starknet_client::reader::objects::pending_data::{
    DeprecatedPendingBlock,
    PendingBlockOrDeprecated,
    PendingStateUpdate as ClientPendingStateUpdate,
};
use apollo_starknet_client::reader::objects::transaction::{
    Transaction as ClientTransaction,
    TransactionReceipt as ClientTransactionReceipt,
};
use apollo_starknet_client::reader::PendingData;
use apollo_starknet_client::writer::{StarknetWriter, WriterClientError};
use apollo_starknet_client::ClientError;
use apollo_storage::body::events::{EventIndex, EventsReader};
use apollo_storage::body::{BodyStorageReader, TransactionIndex};
use apollo_storage::compiled_class::CasmStorageReader;
use apollo_storage::db::{TransactionKind, RO};
use apollo_storage::state::StateStorageReader;
use apollo_storage::{StorageError, StorageReader, StorageTxn};
use async_trait::async_trait;
use jsonrpsee::core::RpcResult;
use jsonrpsee::types::ErrorObjectOwned;
use jsonrpsee::RpcModule;
use papyrus_common::pending_classes::{PendingClasses, PendingClassesTrait};
use starknet_api::block::{
    BlockHash,
    BlockHeaderWithoutHash,
    BlockNumber,
    BlockStatus,
    GasPricePerToken,
};
use starknet_api::contract_class::SierraVersion;
use starknet_api::core::{
    ChainId,
    ClassHash,
    ContractAddress,
    GlobalRoot,
    Nonce,
    BLOCK_HASH_TABLE_ADDRESS,
};
use starknet_api::execution_utils::format_panic_data;
use starknet_api::hash::StarkHash;
use starknet_api::state::{StateNumber, StorageKey, ThinStateDiff as StarknetApiThinStateDiff};
use starknet_api::transaction::fields::Fee;
use starknet_api::transaction::{
    EventContent,
    EventIndexInTransactionOutput,
    Transaction as StarknetApiTransaction,
    TransactionHash,
    TransactionOffsetInBlock,
    TransactionVersion,
};
use starknet_types_core::felt::Felt;
use tokio::runtime::Handle;
use tokio::sync::RwLock;
use tracing::{instrument, trace, warn};

use super::super::block::{
    get_accepted_block_number,
    get_block_header_by_number,
    Block,
    BlockHeader,
    BlockNotRevertedValidator,
    GeneralBlockHeader,
    PendingBlockHeader,
};
use super::super::broadcasted_transaction::{
    BroadcastedDeclareTransaction,
    BroadcastedTransaction,
};
use super::super::error::{
    ContractError,
    JsonRpcError,
    TransactionExecutionError,
    BLOCK_NOT_FOUND,
    CLASS_HASH_NOT_FOUND,
    CONTRACT_NOT_FOUND,
    INVALID_TRANSACTION_HASH,
    INVALID_TRANSACTION_INDEX,
    NO_BLOCKS,
    PAGE_SIZE_TOO_BIG,
    TOO_MANY_KEYS_IN_FILTER,
    TRANSACTION_HASH_NOT_FOUND,
};
use super::super::execution::TransactionTrace;
use super::super::state::{AcceptedStateUpdate, PendingStateUpdate, StateUpdate};
use super::super::transaction::{
    get_block_tx_hashes_by_number,
    get_block_txs_by_number,
    Event,
    GeneralTransactionReceipt,
    L1HandlerMsgHash,
    L1L2MsgHash,
    MessageFromL1,
    PendingTransactionFinalityStatus,
    PendingTransactionOutput,
    PendingTransactionReceipt,
    Transaction,
    TransactionOutput,
    TransactionReceipt,
    TransactionStatus,
    TransactionWithHash,
    TransactionWithReceipt,
    Transactions,
    TypedDeployAccountTransaction,
    TypedInvokeTransaction,
};
use super::super::write_api_error::{
    starknet_error_to_declare_error,
    starknet_error_to_deploy_account_error,
    starknet_error_to_invoke_error,
};
use super::super::write_api_result::{
    AddDeclareOkResult,
    AddDeployAccountOkResult,
    AddInvokeOkResult,
};
use super::{
    execution_error_to_error_object_owned,
    stored_txn_to_executable_txn,
    BlockHashAndNumber,
    BlockId,
    CallRequest,
    CompiledContractClass,
    ContinuationToken,
    EventFilter,
    EventsChunk,
    GatewayContractClass,
    JsonRpcV0_8Server as JsonRpcServer,
    SimulatedTransaction,
    SimulationFlag,
    TransactionTraceWithHash,
};
use crate::api::{BlockHashOrNumber, JsonRpcServerTrait, Tag};
use crate::pending::client_pending_data_to_execution_pending_data;
use crate::syncing_state::{get_last_synced_block, SyncStatus, SyncingState};
use crate::v0_8::state::ThinStateDiff;
use crate::version_config::VERSION_0_8 as VERSION;
use crate::{
    get_block_status,
    get_latest_block_number,
    internal_server_error,
    internal_server_error_with_msg,
    verify_storage_scope,
    ContinuationTokenAsStruct,
    GENESIS_HASH,
};

const DONT_IGNORE_L1_DA_MODE: bool = false;

/// Rpc server.
pub struct JsonRpcServerImpl {
    pub chain_id: ChainId,
    pub execution_config: ExecutionConfig,
    pub storage_reader: StorageReader,
    pub max_events_chunk_size: usize,
    pub max_events_keys: usize,
    pub starting_block: BlockHashAndNumber,
    pub shared_highest_block: Arc<RwLock<Option<BlockHashAndNumber>>>,
    pub pending_data: Arc<RwLock<PendingData>>,
    pub pending_classes: Arc<RwLock<PendingClasses>>,
    pub writer_client: Arc<dyn StarknetWriter>,
    pub class_manager_client: Option<SharedClassManagerClient>,
}

async fn create_class_manager_client(
    class_manager_client: Option<SharedClassManagerClient>,
) -> Option<(SharedClassManagerClient, Handle)> {
    if let Some(class_manager_client) = class_manager_client {
        return Some((class_manager_client.clone(), Handle::current()));
    }
    None
}

#[async_trait]
impl JsonRpcServer for JsonRpcServerImpl {
    #[instrument(skip(self), level = "debug", err, ret)]
    fn spec_version(&self) -> RpcResult<String> {
        Ok(format!("{VERSION}"))
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    fn block_number(&self) -> RpcResult<BlockNumber> {
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        get_latest_block_number(&txn)?.ok_or_else(|| ErrorObjectOwned::from(NO_BLOCKS))
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    fn block_hash_and_number(&self) -> RpcResult<BlockHashAndNumber> {
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let block_number =
            get_latest_block_number(&txn)?.ok_or_else(|| ErrorObjectOwned::from(NO_BLOCKS))?;
        let header: BlockHeader = get_block_header_by_number(&txn, block_number)?.into();

        Ok(BlockHashAndNumber { hash: header.block_hash, number: block_number })
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_block_w_transaction_hashes(&self, block_id: BlockId) -> RpcResult<Block> {
        self.get_block(
            block_id,
            |pending_data| {
                Ok(Transactions::Hashes(
                    pending_data
                        .block
                        .transactions()
                        .iter()
                        .map(|transaction| transaction.transaction_hash())
                        .collect(),
                ))
            },
            |txn, block_number| {
                Ok(Transactions::Hashes(get_block_tx_hashes_by_number(txn, block_number)?))
            },
        )
        .await
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_block_w_full_transactions(&self, block_id: BlockId) -> RpcResult<Block> {
        self.get_block(
            block_id,
            |mut pending_data| {
                let client_transactions = pending_data.block.transactions_mutable().drain(..);
                Ok(Transactions::Full(
                    client_transactions
                        .map(|client_transaction| {
                            let transaction_hash = client_transaction.transaction_hash();
                            let starknet_api_transaction: StarknetApiTransaction =
                                client_transaction.try_into().map_err(internal_server_error)?;
                            Ok(TransactionWithHash {
                                transaction: starknet_api_transaction
                                    .try_into()
                                    .map_err(internal_server_error)?,
                                transaction_hash,
                            })
                        })
                        .collect::<Result<Vec<_>, ErrorObjectOwned>>()?,
                ))
            },
            |txn, block_number| {
                let transactions = get_block_txs_by_number(txn, block_number)?;
                let transaction_hashes = get_block_tx_hashes_by_number(txn, block_number)?;
                Ok(Transactions::Full(
                    transactions
                        .into_iter()
                        .zip(transaction_hashes)
                        .map(|(transaction, transaction_hash)| TransactionWithHash {
                            transaction,
                            transaction_hash,
                        })
                        .collect(),
                ))
            },
        )
        .await
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_block_w_full_transactions_and_receipts(
        &self,
        block_id: BlockId,
    ) -> RpcResult<Block> {
        self.get_block(
            block_id,
            |mut pending_data| {
                let (client_transactions, client_receipts) =
                    pending_data.block.transactions_and_receipts_mutable();
                let client_transactions_and_receipts =
                    client_transactions.drain(..).zip(client_receipts.drain(..));
                Ok(Transactions::FullWithReceipts(
                    client_transactions_and_receipts
                        .map(|(client_transaction, client_transaction_receipt)| {
                            let receipt = client_receipt_to_rpc_pending_receipt(
                                &client_transaction,
                                client_transaction_receipt,
                            )?
                            .into();

                            let starknet_api_transaction: StarknetApiTransaction =
                                client_transaction.try_into().map_err(internal_server_error)?;
                            Ok(TransactionWithReceipt {
                                transaction: starknet_api_transaction
                                    .try_into()
                                    .map_err(internal_server_error)?,
                                receipt,
                            })
                        })
                        .collect::<Result<Vec<_>, ErrorObjectOwned>>()?,
                ))
            },
            |txn, block_number| {
                let transactions = get_block_txs_by_number(txn, block_number)?;
                let transaction_hashes = get_block_tx_hashes_by_number(txn, block_number)?;
                Ok(Transactions::FullWithReceipts(
                    transactions
                        .into_iter()
                        .zip(transaction_hashes)
                        .enumerate()
                        .map(|(transaction_offset, (transaction, transaction_hash))| {
                            let transaction_index = TransactionIndex(
                                block_number,
                                TransactionOffsetInBlock(transaction_offset),
                            );
                            let msg_hash = match &transaction {
                                Transaction::L1Handler(l1_handler_tx) => {
                                    Some(l1_handler_tx.calc_msg_hash())
                                }
                                _ => None,
                            };
                            let transaction_version = transaction.version();
                            Ok(TransactionWithReceipt {
                                transaction,
                                receipt: get_non_pending_receipt(
                                    txn,
                                    transaction_index,
                                    transaction_hash,
                                    transaction_version,
                                    msg_hash,
                                )?
                                .into(),
                            })
                        })
                        .collect::<Result<Vec<_>, ErrorObjectOwned>>()?,
                ))
            },
        )
        .await
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_storage_at(
        &self,
        contract_address: ContractAddress,
        key: StorageKey,
        block_id: BlockId,
    ) -> RpcResult<Felt> {
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let maybe_pending_storage_diffs = if let BlockId::Tag(Tag::Pending) = block_id {
            Some(
                read_pending_data(&self.pending_data, &txn)
                    .await?
                    .state_update
                    .state_diff
                    .storage_diffs,
            )
        } else {
            None
        };

        // Check that the block is valid and get the state number.
        let block_number = get_accepted_block_number(&txn, block_id)?;
        let state_number = StateNumber::unchecked_right_after_block(block_number);
        let res = execution_utils::get_storage_at(
            &txn,
            state_number,
            maybe_pending_storage_diffs.as_ref(),
            contract_address,
            key,
        )
        .map_err(internal_server_error)?;

        // If the contract is not deployed, res will be 0. Checking if that's the case so that
        // we'll return an error instead.
        // Contract address 0x1 is a special address, it stores the block
        // hashes. Contracts are not deployed to this address.
        if res == Felt::default() && contract_address != BLOCK_HASH_TABLE_ADDRESS {
            // check if the contract exists
            txn.get_state_reader()
                .map_err(internal_server_error)?
                .get_class_hash_at(state_number, &contract_address)
                .map_err(internal_server_error)?
                .ok_or_else(|| ErrorObjectOwned::from(CONTRACT_NOT_FOUND))?;
        }
        Ok(res)
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_transaction_by_hash(
        &self,
        transaction_hash: TransactionHash,
    ) -> RpcResult<TransactionWithHash> {
        verify_storage_scope(&self.storage_reader)?;

        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        if let Some(transaction_index) =
            txn.get_transaction_idx_by_hash(&transaction_hash).map_err(internal_server_error)?
        {
            let transaction = txn
                .get_transaction(transaction_index)
                .map_err(internal_server_error)?
                .ok_or_else(|| ErrorObjectOwned::from(TRANSACTION_HASH_NOT_FOUND))?;

            Ok(TransactionWithHash { transaction: transaction.try_into()?, transaction_hash })
        } else {
            // The transaction is not in any non-pending block. Search for it in the pending block
            // and if it's not found, return error.
            let client_transaction = read_pending_data(&self.pending_data, &txn)
                .await?
                .block
                .transactions()
                .iter()
                .find(|transaction| transaction.transaction_hash() == transaction_hash)
                .ok_or_else(|| ErrorObjectOwned::from(TRANSACTION_HASH_NOT_FOUND))?
                .clone();

            let starknet_api_transaction: StarknetApiTransaction =
                client_transaction.try_into().map_err(internal_server_error)?;
            return Ok(TransactionWithHash {
                transaction: starknet_api_transaction.try_into()?,
                transaction_hash,
            });
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_transaction_by_block_id_and_index(
        &self,
        block_id: BlockId,
        index: TransactionOffsetInBlock,
    ) -> RpcResult<TransactionWithHash> {
        verify_storage_scope(&self.storage_reader)?;

        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let (starknet_api_transaction, transaction_hash) =
            if let BlockId::Tag(Tag::Pending) = block_id {
                let client_transaction = read_pending_data(&self.pending_data, &txn)
                    .await?
                    .block
                    .transactions()
                    .get(index.0)
                    .ok_or_else(|| ErrorObjectOwned::from(INVALID_TRANSACTION_INDEX))?
                    .clone();
                let transaction_hash = client_transaction.transaction_hash();
                (client_transaction.try_into().map_err(internal_server_error)?, transaction_hash)
            } else {
                let block_number = get_accepted_block_number(&txn, block_id)?;

                let tx_index = TransactionIndex(block_number, index);
                let transaction = txn
                    .get_transaction(tx_index)
                    .map_err(internal_server_error)?
                    .ok_or_else(|| ErrorObjectOwned::from(INVALID_TRANSACTION_INDEX))?;
                let transaction_hash = txn
                    .get_transaction_hash_by_idx(&tx_index)
                    .map_err(internal_server_error)?
                    .ok_or_else(|| ErrorObjectOwned::from(INVALID_TRANSACTION_INDEX))?;
                (transaction, transaction_hash)
            };

        Ok(TransactionWithHash {
            transaction: starknet_api_transaction.try_into()?,
            transaction_hash,
        })
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_block_transaction_count(&self, block_id: BlockId) -> RpcResult<usize> {
        verify_storage_scope(&self.storage_reader)?;
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        if let BlockId::Tag(Tag::Pending) = block_id {
            let transactions_len =
                read_pending_data(&self.pending_data, &txn).await?.block.transactions().len();
            Ok(transactions_len)
        } else {
            let block_number = get_accepted_block_number(&txn, block_id)?;
            Ok(txn
                .get_block_transactions_count(block_number)
                .map_err(internal_server_error)?
                .ok_or_else(|| ErrorObjectOwned::from(BLOCK_NOT_FOUND))?)
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_state_update(&self, block_id: BlockId) -> RpcResult<StateUpdate> {
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        if let BlockId::Tag(Tag::Pending) = block_id {
            let state_update = read_pending_data(&self.pending_data, &txn).await?.state_update;
            return Ok(StateUpdate::PendingStateUpdate(PendingStateUpdate {
                old_root: state_update.old_root,
                state_diff: state_update.state_diff.into(),
            }));
        }

        // Get the block header for the block hash and state root.
        let block_number = get_accepted_block_number(&txn, block_id)?;
        let header: BlockHeader = get_block_header_by_number(&txn, block_number)?.into();

        // Get the old root.
        let old_root = match get_accepted_block_number(
            &txn,
            BlockId::HashOrNumber(BlockHashOrNumber::Hash(header.parent_hash)),
        ) {
            Ok(parent_block_number) => {
                BlockHeader::from(get_block_header_by_number(&txn, parent_block_number)?).new_root
            }
            Err(_) => GlobalRoot(StarkHash::from_hex_unchecked(GENESIS_HASH)),
        };

        // Get the block state diff.
        let mut thin_state_diff = txn
            .get_state_diff(block_number)
            .map_err(internal_server_error)?
            .ok_or_else(|| ErrorObjectOwned::from(BLOCK_NOT_FOUND))?;
        // Remove empty storage diffs. Some blocks contain empty storage diffs that must be kept for
        // the computation of state diff commitment.
        thin_state_diff.storage_diffs.retain(|_k, v| !v.is_empty());

        let state_diff =
            self.convert_thin_state_diff(thin_state_diff, block_id, block_number).await?;
        Ok(StateUpdate::AcceptedStateUpdate(AcceptedStateUpdate {
            block_hash: header.block_hash,
            new_root: header.new_root,
            old_root,
            state_diff,
        }))
    }

    async fn get_transaction_status(
        &self,
        transaction_hash: TransactionHash,
    ) -> RpcResult<TransactionStatus> {
        Ok(self.get_transaction_receipt(transaction_hash).await?.transaction_status())
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_transaction_receipt(
        &self,
        transaction_hash: TransactionHash,
    ) -> RpcResult<GeneralTransactionReceipt> {
        verify_storage_scope(&self.storage_reader)?;

        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        if let Some(transaction_index) =
            txn.get_transaction_idx_by_hash(&transaction_hash).map_err(internal_server_error)?
        {
            let tx = txn
                .get_transaction(transaction_index)
                .map_err(internal_server_error)?
                .unwrap_or_else(|| panic!("Should have tx {transaction_hash}"));

            // TODO(Shahak): Add version function to transaction in SN_API.
            let tx_version = match &tx {
                StarknetApiTransaction::Declare(tx) => tx.version(),
                StarknetApiTransaction::Deploy(tx) => tx.version,
                StarknetApiTransaction::DeployAccount(tx) => tx.version(),
                StarknetApiTransaction::Invoke(tx) => tx.version(),
                StarknetApiTransaction::L1Handler(tx) => tx.version,
            };

            let msg_hash = match tx {
                StarknetApiTransaction::L1Handler(l1_handler_tx) => {
                    Some(l1_handler_tx.calc_msg_hash())
                }
                _ => None,
            };

            get_non_pending_receipt(&txn, transaction_index, transaction_hash, tx_version, msg_hash)
        } else {
            // The transaction is not in any non-pending block. Search for it in the pending block
            // and if it's not found, return error.

            // TODO(shahak): Consider cloning the transactions and the receipts in order to free
            // the lock sooner (Check which is better).
            let pending_data = read_pending_data(&self.pending_data, &txn).await?;

            let client_transaction_receipt = pending_data
                .block
                .transaction_receipts()
                .iter()
                .find(|receipt| receipt.transaction_hash == transaction_hash)
                .ok_or_else(|| ErrorObjectOwned::from(TRANSACTION_HASH_NOT_FOUND))?
                .clone();
            let client_transaction = &pending_data
                .block
                .transactions()
                .iter()
                .find(|tx| tx.transaction_hash() == transaction_hash)
                .ok_or_else(|| ErrorObjectOwned::from(TRANSACTION_HASH_NOT_FOUND))?
                .clone();
            client_receipt_to_rpc_pending_receipt(client_transaction, client_transaction_receipt)
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_class(
        &self,
        block_id: BlockId,
        class_hash: ClassHash,
    ) -> RpcResult<GatewayContractClass> {
        // Check in pending classes.
        let block_id = if let BlockId::Tag(Tag::Pending) = block_id {
            let maybe_class = &self.pending_classes.read().await.get_class(class_hash);
            if let Some(class) = maybe_class {
                return class.clone().try_into().map_err(internal_server_error);
            } else {
                BlockId::Tag(Tag::Latest)
            }
        } else {
            block_id
        };

        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        let block_number = get_accepted_block_number(&txn, block_id)?;
        let state_number = StateNumber::unchecked_right_after_block(block_number);
        let state_reader = txn.get_state_reader().map_err(internal_server_error)?;

        // If class manager supplied, first check with it.
        if let Some(class_manager_client) = &self.class_manager_client {
            let optional_sierra_contract_class = class_manager_client
                .get_sierra(class_hash)
                .await
                .map_err(internal_server_error_with_msg)?;

            if let Some(sierra_contract_class) = optional_sierra_contract_class {
                let optional_class_definition_block_number = state_reader
                    .get_class_definition_block_number(&class_hash)
                    .map_err(internal_server_error)?;

                // Check if this class exists in the Cairo1 classes table.
                if optional_class_definition_block_number.is_some()
                    && optional_class_definition_block_number <= Some(block_number)
                {
                    return Ok(GatewayContractClass::Sierra(sierra_contract_class.into()));
                } else {
                    return Err(ErrorObjectOwned::from(CLASS_HASH_NOT_FOUND));
                }
            }
        }

        // The class might be a deprecated class. Search it first in the declared classes and if not
        // found, search in the deprecated classes.
        if let Some(class) = state_reader
            .get_class_definition_at(state_number, &class_hash)
            .map_err(internal_server_error)?
        {
            Ok(GatewayContractClass::Sierra(class.into()))
        } else {
            let class = state_reader
                .get_deprecated_class_definition_at(state_number, &class_hash)
                .map_err(internal_server_error)?
                .ok_or_else(|| ErrorObjectOwned::from(CLASS_HASH_NOT_FOUND))?;
            Ok(GatewayContractClass::Cairo0(class.try_into().map_err(internal_server_error)?))
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_class_at(
        &self,
        block_id: BlockId,
        contract_address: ContractAddress,
    ) -> RpcResult<GatewayContractClass> {
        let class_hash = self.get_class_hash_at(block_id, contract_address).await?;
        self.get_class(block_id, class_hash).await
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_class_hash_at(
        &self,
        block_id: BlockId,
        contract_address: ContractAddress,
    ) -> RpcResult<ClassHash> {
        self.maybe_get_class_hash_at(block_id, contract_address)
            .await?
            .ok_or_else(|| ErrorObjectOwned::from(CONTRACT_NOT_FOUND))
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_nonce(
        &self,
        block_id: BlockId,
        contract_address: ContractAddress,
    ) -> RpcResult<Nonce> {
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        let maybe_pending_nonces = if let BlockId::Tag(Tag::Pending) = block_id {
            Some(read_pending_data(&self.pending_data, &txn).await?.state_update.state_diff.nonces)
        } else {
            None
        };

        // Check that the block is valid and get the state number.
        let block_number = get_accepted_block_number(&txn, block_id)?;
        let state_number = StateNumber::unchecked_right_after_block(block_number);
        execution_utils::get_nonce_at(
            &txn,
            state_number,
            maybe_pending_nonces.as_ref(),
            contract_address,
        )
        .map_err(internal_server_error)?
        .ok_or_else(|| ErrorObjectOwned::from(CONTRACT_NOT_FOUND))
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    fn chain_id(&self) -> RpcResult<String> {
        Ok(self.chain_id.as_hex())
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn get_events(&self, filter: EventFilter) -> RpcResult<EventsChunk> {
        verify_storage_scope(&self.storage_reader)?;

        // Check the chunk size.
        if filter.chunk_size > self.max_events_chunk_size {
            return Err(ErrorObjectOwned::from(PAGE_SIZE_TOO_BIG));
        }
        // Check the number of keys.
        if filter.keys.len() > self.max_events_keys {
            return Err(ErrorObjectOwned::from(TOO_MANY_KEYS_IN_FILTER));
        }

        // Get the requested block numbers.
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let Some(latest_block_number) = get_latest_block_number(&txn)? else {
            if matches!(filter.to_block, Some(BlockId::Tag(Tag::Pending)) | None) {
                warn!(
                    "Received a request for pending events while there are no accepted blocks. \
                     This is currently unsupported. Returning no events."
                );
            }
            // There are no blocks.
            return Ok(EventsChunk { events: vec![], continuation_token: None });
        };
        let from_block_number = match filter.from_block {
            None => BlockNumber(0),
            Some(BlockId::Tag(Tag::Pending)) => latest_block_number.unchecked_next(),
            Some(block_id) => get_accepted_block_number(&txn, block_id)?,
        };
        let mut to_block_number = match filter.to_block {
            Some(BlockId::Tag(Tag::Pending)) | None => latest_block_number.unchecked_next(),
            Some(block_id) => get_accepted_block_number(&txn, block_id)?,
        };

        if from_block_number > to_block_number {
            return Ok(EventsChunk { events: vec![], continuation_token: None });
        }

        // Get the event index. If there's a continuation token we take the event index from there.
        // Otherwise, we take the first index in the from_block_number.
        let start_event_index = match &filter.continuation_token {
            Some(token) => token.parse()?.0,
            None => EventIndex(
                TransactionIndex(from_block_number, TransactionOffsetInBlock(0)),
                EventIndexInTransactionOutput(0),
            ),
        };

        let include_pending_block = to_block_number > latest_block_number;
        if include_pending_block {
            to_block_number = to_block_number.prev().expect(
                "A block number that's greater than another block number should have a predecessor",
            );
        }

        // Collect the requested events.
        // Once we collected enough events, we continue to check if there are any more events
        // corresponding to the requested filter. If there are, we return a continuation token
        // pointing to the next relevant event. Otherwise, we return a continuation token None.
        let mut filtered_events = vec![];
        if start_event_index.0.0 <= latest_block_number {
            for ((from_address, event_index), content) in txn
                .iter_events(filter.address, start_event_index, to_block_number)
                .map_err(internal_server_error)?
            {
                let block_number = (event_index.0).0;
                if block_number > to_block_number {
                    break;
                }
                if let Some(filter_address) = filter.address {
                    if from_address != filter_address {
                        // The iterator of this loop outputs only events that have the filter's
                        // address, unless there are no more such events and then it outputs other
                        // events, and we can stop the iteration.
                        break;
                    }
                }
                // TODO(Shahak): Consider changing empty sets in the filer keys to None.
                if do_event_keys_match_filter(&content, &filter) {
                    if filtered_events.len() == filter.chunk_size {
                        return Ok(EventsChunk {
                            events: filtered_events,
                            continuation_token: Some(ContinuationToken::new(
                                ContinuationTokenAsStruct(event_index),
                            )?),
                        });
                    }
                    let header: BlockHeader = get_block_header_by_number(&txn, block_number)
                        .map_err(internal_server_error)?
                        .into();
                    let transaction_hash = txn
                        .get_transaction_hash_by_idx(&event_index.0)
                        .map_err(internal_server_error)?
                        .ok_or_else(|| internal_server_error("Unknown internal error."))?;
                    let emitted_event = Event {
                        block_hash: Some(header.block_hash),
                        block_number: Some(block_number),
                        transaction_hash,
                        event: starknet_api::transaction::Event { from_address, content },
                    };
                    filtered_events.push(emitted_event);
                }
            }
        }

        if include_pending_block {
            let pending_block = read_pending_data(&self.pending_data, &txn).await?.block;
            let pending_transaction_receipts = pending_block.transaction_receipts();
            // Extract the first transaction offset and event offset from the starting EventIndex.
            let (transaction_start, event_start) = if start_event_index.0.0 > latest_block_number {
                (start_event_index.0.1.0, start_event_index.1.0)
            } else {
                (0, 0)
            };
            // TODO(shahak): Consider creating the iterator flattened and filtered.
            for (transaction_offset, receipt) in pending_transaction_receipts.iter().enumerate() {
                if transaction_offset < transaction_start {
                    continue;
                }
                for (event_offset, event) in receipt.events.iter().cloned().enumerate() {
                    if transaction_offset == transaction_start && event_offset < event_start {
                        continue;
                    }
                    if filtered_events.len() == filter.chunk_size {
                        return Ok(EventsChunk {
                            events: filtered_events,
                            continuation_token: Some(ContinuationToken::new(
                                ContinuationTokenAsStruct(EventIndex(
                                    TransactionIndex(
                                        latest_block_number.unchecked_next(),
                                        TransactionOffsetInBlock(transaction_offset),
                                    ),
                                    EventIndexInTransactionOutput(event_offset),
                                )),
                            )?),
                        });
                    }
                    if !do_event_keys_match_filter(&event.content, &filter) {
                        continue;
                    }
                    if let Some(filter_address) = filter.address {
                        if event.from_address != filter_address {
                            continue;
                        }
                    }
                    filtered_events.push(Event {
                        block_hash: None,
                        block_number: None,
                        transaction_hash: receipt.transaction_hash,
                        event,
                    })
                }
            }
        }

        Ok(EventsChunk { events: filtered_events, continuation_token: None })
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn syncing(&self) -> RpcResult<SyncingState> {
        let Some(highest_block) = *self.shared_highest_block.read().await else {
            return Ok(SyncingState::Synced);
        };
        let current_block =
            get_last_synced_block(self.storage_reader.clone()).map_err(internal_server_error)?;
        if highest_block.number <= current_block.number {
            return Ok(SyncingState::Synced);
        }
        Ok(SyncingState::SyncStatus(SyncStatus {
            starting_block_hash: self.starting_block.hash,
            starting_block_num: self.starting_block.number,
            current_block_hash: current_block.hash,
            current_block_num: current_block.number,
            highest_block_hash: highest_block.hash,
            highest_block_num: highest_block.number,
        }))
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn call(&self, request: CallRequest, block_id: BlockId) -> RpcResult<Vec<Felt>> {
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let maybe_pending_data = if let BlockId::Tag(Tag::Pending) = block_id {
            Some(client_pending_data_to_execution_pending_data(
                read_pending_data(&self.pending_data, &txn).await?,
                self.pending_classes.read().await.clone(),
            ))
        } else {
            None
        };
        let block_number = get_accepted_block_number(&txn, block_id)?;
        let block_not_reverted_validator = BlockNotRevertedValidator::new(block_number, &txn)?;
        drop(txn);
        let state_number = StateNumber::unchecked_right_after_block(block_number);
        let execution_config = self.execution_config;

        let chain_id = self.chain_id.clone();
        let reader = self.storage_reader.clone();
        let contract_address_copy = request.contract_address;
        let class_manager_client =
            create_class_manager_client(self.class_manager_client.clone()).await;

        let res = tokio::task::spawn_blocking(move || {
            execute_call(
                reader,
                maybe_pending_data,
                &chain_id,
                state_number,
                block_number,
                &contract_address_copy,
                request.entry_point_selector,
                request.calldata,
                &execution_config,
                DONT_IGNORE_L1_DA_MODE,
                class_manager_client,
            )
        })
        .await
        .map_err(internal_server_error)?
        .map_err(execution_error_to_error_object_owned)?;

        if res.failed {
            let contract_err = ContractError { revert_error: format_panic_data(&res.retdata.0) };
            let rpc_err: JsonRpcError<ContractError> = contract_err.into();
            return Err(rpc_err.into());
        }

        block_not_reverted_validator.validate(&self.storage_reader)?;

        Ok(res.retdata.0)
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn add_invoke_transaction(
        &self,
        invoke_transaction: TypedInvokeTransaction,
    ) -> RpcResult<AddInvokeOkResult> {
        let result = self.writer_client.add_invoke_transaction(&invoke_transaction.into()).await;
        match result {
            Ok(res) => Ok(res.into()),
            Err(WriterClientError::ClientError(ClientError::StarknetError(starknet_error))) => {
                Err(ErrorObjectOwned::from(starknet_error_to_invoke_error(starknet_error)))
            }
            Err(err) => Err(internal_server_error(err)),
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn add_deploy_account_transaction(
        &self,
        deploy_account_transaction: TypedDeployAccountTransaction,
    ) -> RpcResult<AddDeployAccountOkResult> {
        let result = self
            .writer_client
            .add_deploy_account_transaction(&deploy_account_transaction.into())
            .await;
        match result {
            Ok(res) => Ok(res.into()),
            Err(WriterClientError::ClientError(ClientError::StarknetError(starknet_error))) => {
                Err(ErrorObjectOwned::from(starknet_error_to_deploy_account_error(starknet_error)))
            }
            Err(err) => Err(internal_server_error(err)),
        }
    }

    #[instrument(skip(self), level = "debug", err, ret)]
    async fn add_declare_transaction(
        &self,
        declare_transaction: BroadcastedDeclareTransaction,
    ) -> RpcResult<AddDeclareOkResult> {
        let result = self
            .writer_client
            .add_declare_transaction(
                &declare_transaction.try_into().map_err(internal_server_error)?,
            )
            .await;
        match result {
            Ok(res) => Ok(res.into()),
            Err(WriterClientError::ClientError(ClientError::StarknetError(starknet_error))) => {
                Err(ErrorObjectOwned::from(starknet_error_to_declare_error(starknet_error)))
            }
            Err(err) => Err(internal_server_error(err)),
        }
    }

    #[instrument(skip(self, transactions), level = "debug", err, ret)]
    async fn estimate_fee(
        &self,
        transactions: Vec<BroadcastedTransaction>,
        simulation_flags: Vec<SimulationFlag>,
        block_id: BlockId,
    ) -> RpcResult<Vec<FeeEstimation>> {
        trace!("Estimating fee of transactions: {:#?}", transactions);
        let validate = !simulation_flags.contains(&SimulationFlag::SkipValidate);

        let storage_txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        let maybe_pending_data = if let BlockId::Tag(Tag::Pending) = block_id {
            Some(client_pending_data_to_execution_pending_data(
                read_pending_data(&self.pending_data, &storage_txn).await?,
                self.pending_classes.read().await.clone(),
            ))
        } else {
            None
        };

        let executable_txns =
            transactions.into_iter().map(|tx| tx.try_into()).collect::<Result<_, _>>()?;

        let block_number = get_accepted_block_number(&storage_txn, block_id)?;
        let block_not_reverted_validator =
            BlockNotRevertedValidator::new(block_number, &storage_txn)?;
        drop(storage_txn);
        let state_number = StateNumber::unchecked_right_after_block(block_number);
        let execution_config = self.execution_config;

        let chain_id = self.chain_id.clone();
        let reader = self.storage_reader.clone();
        let class_manager_client =
            create_class_manager_client(self.class_manager_client.clone()).await;

        let estimate_fee_result = tokio::task::spawn_blocking(move || {
            exec_estimate_fee(
                executable_txns,
                &chain_id,
                reader,
                maybe_pending_data,
                state_number,
                block_number,
                &execution_config,
                validate,
                DONT_IGNORE_L1_DA_MODE,
                class_manager_client,
            )
        })
        .await
        .map_err(internal_server_error)?;

        block_not_reverted_validator.validate(&self.storage_reader)?;

        match estimate_fee_result {
            Ok(Ok(fees)) => Ok(fees),
            Ok(Err(reverted_tx)) => {
                Err(ErrorObjectOwned::from(JsonRpcError::<TransactionExecutionError>::from(
                    TransactionExecutionError {
                        transaction_index: reverted_tx.index,
                        execution_error: reverted_tx.revert_reason,
                    },
                )))
            }
            Err(err) => Err(internal_server_error(err)),
        }
    }

    #[instrument(skip(self, transactions), level = "debug", err, ret)]
    async fn simulate_transactions(
        &self,
        block_id: BlockId,
        transactions: Vec<BroadcastedTransaction>,
        simulation_flags: Vec<SimulationFlag>,
    ) -> RpcResult<Vec<SimulatedTransaction>> {
        trace!("Simulating transactions: {:#?}", transactions);
        let executable_txns =
            transactions.into_iter().map(|tx| tx.try_into()).collect::<Result<_, _>>()?;

        let storage_txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        let maybe_pending_data = if let BlockId::Tag(Tag::Pending) = block_id {
            Some(client_pending_data_to_execution_pending_data(
                read_pending_data(&self.pending_data, &storage_txn).await?,
                self.pending_classes.read().await.clone(),
            ))
        } else {
            None
        };

        let block_number = get_accepted_block_number(&storage_txn, block_id)?;
        let block_not_reverted_validator =
            BlockNotRevertedValidator::new(block_number, &storage_txn)?;
        drop(storage_txn);
        let state_number = StateNumber::unchecked_right_after_block(block_number);
        let execution_config = self.execution_config;

        let chain_id = self.chain_id.clone();
        let reader = self.storage_reader.clone();

        let charge_fee = !simulation_flags.contains(&SimulationFlag::SkipFeeCharge);
        let validate = !simulation_flags.contains(&SimulationFlag::SkipValidate);
        let class_manager_client =
            create_class_manager_client(self.class_manager_client.clone()).await;

        let simulation_results = tokio::task::spawn_blocking(move || {
            exec_simulate_transactions(
                executable_txns,
                None,
                &chain_id,
                reader,
                maybe_pending_data,
                state_number,
                block_number,
                &execution_config,
                charge_fee,
                validate,
                DONT_IGNORE_L1_DA_MODE,
                class_manager_client,
            )
        })
        .await
        .map_err(internal_server_error)?
        .map_err(execution_error_to_error_object_owned)?;

        block_not_reverted_validator.validate(&self.storage_reader)?;

        let mut res = vec![];
        for simulation_output in simulation_results {
            let state_diff = self
                .convert_thin_state_diff(
                    simulation_output.induced_state_diff,
                    block_id,
                    block_number,
                )
                .await?;
            res.push(SimulatedTransaction {
                transaction_trace: (simulation_output.transaction_trace, state_diff).into(),
                fee_estimation: simulation_output.fee_estimation,
            });
        }
        Ok(res)
    }

    #[instrument(skip(self), level = "debug", err)]
    async fn trace_transaction(
        &self,
        transaction_hash: TransactionHash,
    ) -> RpcResult<TransactionTrace> {
        let storage_txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        let pending_block = read_pending_data(&self.pending_data, &storage_txn).await?.block;
        // Search for the transaction inside the pending block.
        let (
            maybe_pending_data,
            executable_transactions,
            transaction_hashes,
            block_number,
            state_number,
        ) = if let Some((pending_transaction_offset, _)) = pending_block
            .transaction_receipts()
            .iter()
            .enumerate()
            .find(|(_, receipt)| receipt.transaction_hash == transaction_hash)
        {
            // If there are no blocks in the network and there is a pending block, as an edge
            // case we treat this as if the pending block is empty.
            let block_number =
                get_latest_block_number(&storage_txn)?.ok_or(INVALID_TRANSACTION_HASH)?;
            let state_number = StateNumber::unchecked_right_after_block(block_number);
            let executable_transactions = pending_block
                .transactions()
                .iter()
                .take(pending_transaction_offset + 1)
                .map(|client_transaction| {
                    let starknet_api_transaction: StarknetApiTransaction =
                        client_transaction.clone().try_into().map_err(internal_server_error)?;
                    stored_txn_to_executable_txn(
                        starknet_api_transaction,
                        &storage_txn,
                        state_number,
                    )
                })
                .collect::<Result<_, _>>()?;
            let transaction_hashes = pending_block
                .transaction_receipts()
                .iter()
                .map(|receipt| receipt.transaction_hash)
                .collect();
            let maybe_pending_data = Some(ExecutionPendingData {
                timestamp: pending_block.timestamp(),
                l1_gas_price: pending_block.l1_gas_price(),
                l1_data_gas_price: pending_block.l1_data_gas_price(),
                l2_gas_price: pending_block.l2_gas_price(),
                l1_da_mode: pending_block.l1_da_mode(),
                sequencer: pending_block.sequencer_address(),
                // The pending state diff should be empty since we look at the state in the
                // start of the pending block.
                // Not using ..Default::default() to avoid missing fields in the future.
                storage_diffs: Default::default(),
                deployed_contracts: Default::default(),
                declared_classes: Default::default(),
                old_declared_contracts: Default::default(),
                nonces: Default::default(),
                replaced_classes: Default::default(),
                classes: Default::default(),
            });
            (
                maybe_pending_data,
                executable_transactions,
                transaction_hashes,
                block_number,
                state_number,
            )
        } else {
            // Transaction is not inside the pending block. Search for it in the storage.
            let TransactionIndex(block_number, tx_offset) = storage_txn
                .get_transaction_idx_by_hash(&transaction_hash)
                .map_err(internal_server_error)?
                .ok_or(TRANSACTION_HASH_NOT_FOUND)?;

            let block_transactions = storage_txn
                .get_block_transactions(block_number)
                .map_err(internal_server_error)?
                .ok_or_else(|| {
                    internal_server_error(StorageError::DBInconsistency {
                        msg: format!("Missing block {block_number} transactions"),
                    })
                })?;

            let transaction_hashes = storage_txn
                .get_block_transaction_hashes(block_number)
                .map_err(internal_server_error)?
                .ok_or_else(|| {
                    internal_server_error(StorageError::DBInconsistency {
                        msg: format!("Missing block {block_number} transactions"),
                    })
                })?;

            let state_number = StateNumber::right_before_block(block_number);
            let executable_transactions = block_transactions
                .into_iter()
                .take(tx_offset.0 + 1)
                .map(|tx| stored_txn_to_executable_txn(tx, &storage_txn, state_number))
                .collect::<Result<_, _>>()?;

            (None, executable_transactions, transaction_hashes, block_number, state_number)
        };

        let block_not_reverted_validator =
            BlockNotRevertedValidator::new(block_number, &storage_txn)?;

        drop(storage_txn);

        let execution_config = self.execution_config;

        let chain_id = self.chain_id.clone();
        let reader = self.storage_reader.clone();
        let class_manager_client =
            create_class_manager_client(self.class_manager_client.clone()).await;

        let is_pending = maybe_pending_data.is_some();
        let mut simulation_results = tokio::task::spawn_blocking(move || {
            exec_simulate_transactions(
                executable_transactions,
                Some(transaction_hashes),
                &chain_id,
                reader,
                maybe_pending_data,
                state_number,
                block_number,
                &execution_config,
                true,
                true,
                DONT_IGNORE_L1_DA_MODE,
                class_manager_client,
            )
        })
        .await
        .map_err(internal_server_error)?
        .map_err(execution_error_to_error_object_owned)?;

        block_not_reverted_validator.validate(&self.storage_reader)?;

        let simulation_result =
            simulation_results.pop().expect("Should have transaction exeuction result");

        let block_id = if is_pending {
            BlockId::Tag(Tag::Pending)
        } else {
            BlockId::HashOrNumber(BlockHashOrNumber::Number(block_number))
        };
        let state_diff = self
            .convert_thin_state_diff(simulation_result.induced_state_diff, block_id, block_number)
            .await?;
        Ok((simulation_result.transaction_trace, state_diff).into())
    }

    #[instrument(skip(self), level = "debug", err)]
    async fn trace_block_transactions(
        &self,
        block_id: BlockId,
    ) -> RpcResult<Vec<TransactionTraceWithHash>> {
        let storage_txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        let maybe_client_pending_data = if let BlockId::Tag(Tag::Pending) = block_id {
            Some(read_pending_data(&self.pending_data, &storage_txn).await?)
        } else {
            None
        };

        let block_number = get_accepted_block_number(&storage_txn, block_id)?;

        let block_not_reverted_validator =
            BlockNotRevertedValidator::new(block_number, &storage_txn)?;

        let (maybe_pending_data, block_transactions, transaction_hashes, state_number) =
            match maybe_client_pending_data {
                Some(client_pending_data) => (
                    Some(ExecutionPendingData {
                        timestamp: client_pending_data.block.timestamp(),
                        l1_gas_price: client_pending_data.block.l1_gas_price(),
                        l1_data_gas_price: client_pending_data.block.l1_data_gas_price(),
                        l2_gas_price: client_pending_data.block.l2_gas_price(),
                        l1_da_mode: client_pending_data.block.l1_da_mode(),
                        sequencer: client_pending_data.block.sequencer_address(),
                        // The pending state diff should be empty since we look at the state in the
                        // start of the pending block.
                        // Not using ..Default::default() to avoid missing fields in the future.
                        storage_diffs: Default::default(),
                        deployed_contracts: Default::default(),
                        declared_classes: Default::default(),
                        old_declared_contracts: Default::default(),
                        nonces: Default::default(),
                        replaced_classes: Default::default(),
                        classes: Default::default(),
                    }),
                    client_pending_data
                        .block
                        .transactions()
                        .iter()
                        .map(|client_transaction| {
                            client_transaction.clone().try_into().map_err(internal_server_error)
                        })
                        .collect::<Result<Vec<_>, ErrorObjectOwned>>()?,
                    client_pending_data
                        .block
                        .transaction_receipts()
                        .iter()
                        .map(|receipt| receipt.transaction_hash)
                        .collect(),
                    StateNumber::unchecked_right_after_block(block_number),
                ),
                None => (
                    None,
                    storage_txn
                        .get_block_transactions(block_number)
                        .map_err(internal_server_error)?
                        .ok_or_else(|| {
                            internal_server_error(StorageError::DBInconsistency {
                                msg: format!("Missing block {block_number} transactions"),
                            })
                        })?,
                    storage_txn
                        .get_block_transaction_hashes(block_number)
                        .map_err(internal_server_error)?
                        .ok_or_else(|| {
                            internal_server_error(StorageError::DBInconsistency {
                                msg: format!("Missing block {block_number} transactions"),
                            })
                        })?,
                    StateNumber::right_before_block(block_number),
                ),
            };

        let executable_txns = block_transactions
            .into_iter()
            .map(|tx| stored_txn_to_executable_txn(tx, &storage_txn, state_number))
            .collect::<Result<_, _>>()?;

        drop(storage_txn);

        let execution_config = self.execution_config;

        let chain_id = self.chain_id.clone();
        let reader = self.storage_reader.clone();
        let transaction_hashes_clone = transaction_hashes.clone();
        let class_manager_client =
            create_class_manager_client(self.class_manager_client.clone()).await;

        let simulation_results = tokio::task::spawn_blocking(move || {
            exec_simulate_transactions(
                executable_txns,
                Some(transaction_hashes_clone),
                &chain_id,
                reader,
                maybe_pending_data,
                state_number,
                block_number,
                &execution_config,
                true,
                true,
                DONT_IGNORE_L1_DA_MODE,
                class_manager_client,
            )
        })
        .await
        .map_err(internal_server_error)?
        .map_err(execution_error_to_error_object_owned)?;

        block_not_reverted_validator.validate(&self.storage_reader)?;

        let mut res = vec![];
        for (simulation_output, transaction_hash) in
            simulation_results.into_iter().zip(transaction_hashes)
        {
            let state_diff = self
                .convert_thin_state_diff(
                    simulation_output.induced_state_diff,
                    block_id,
                    block_number,
                )
                .await?;
            res.push(TransactionTraceWithHash {
                transaction_hash,
                trace_root: (simulation_output.transaction_trace, state_diff).into(),
            });
        }
        Ok(res)
    }

    #[instrument(skip(self, message), level = "debug", err)]
    async fn estimate_message_fee(
        &self,
        message: MessageFromL1,
        block_id: BlockId,
    ) -> RpcResult<FeeEstimation> {
        trace!("Estimating fee of message: {:#?}", message);
        let storage_txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let maybe_pending_data = if let BlockId::Tag(Tag::Pending) = block_id {
            Some(client_pending_data_to_execution_pending_data(
                read_pending_data(&self.pending_data, &storage_txn).await?,
                self.pending_classes.read().await.clone(),
            ))
        } else {
            None
        };
        // Convert the message to an L1 handler transaction, and estimate the fee of the
        // transaction.
        // The fee input is used to bound the amount of fee used. Because we want to estimate the
        // fee, we pass u128::MAX so the execution won't fail.
        let executable_txns =
            vec![ExecutableTransactionInput::L1Handler(message.into(), Fee(u128::MAX), false)];

        let block_number = get_accepted_block_number(&storage_txn, block_id)?;
        let block_not_reverted_validator =
            BlockNotRevertedValidator::new(block_number, &storage_txn)?;
        drop(storage_txn);
        let state_number = StateNumber::unchecked_right_after_block(block_number);
        let execution_config = self.execution_config;

        let chain_id = self.chain_id.clone();
        let reader = self.storage_reader.clone();
        let class_manager_client =
            create_class_manager_client(self.class_manager_client.clone()).await;

        let estimate_fee_result = tokio::task::spawn_blocking(move || {
            exec_estimate_fee(
                executable_txns,
                &chain_id,
                reader,
                maybe_pending_data,
                state_number,
                block_number,
                &execution_config,
                false,
                DONT_IGNORE_L1_DA_MODE,
                class_manager_client,
            )
        })
        .await
        .map_err(internal_server_error)?;

        block_not_reverted_validator.validate(&self.storage_reader)?;

        match estimate_fee_result {
            Ok(Ok(mut fee_as_vec)) => {
                if fee_as_vec.len() != 1 {
                    return Err(internal_server_error(format!(
                        "Expected a single fee, got {}",
                        fee_as_vec.len()
                    )));
                }
                let Some(fee_estimation) = fee_as_vec.pop() else {
                    return Err(internal_server_error(
                        "Expected a single fee, got an empty vector",
                    ));
                };
                Ok(fee_estimation)
            }
            // Error in the execution of the contract.
            Ok(Err(reverted_tx)) => Err(JsonRpcError::<ContractError>::from(ContractError {
                revert_error: reverted_tx.revert_reason,
            })
            .into()),
            // Internal error during the execution.
            Err(err) => Err(internal_server_error(err)),
        }
    }

    #[instrument(skip(self), level = "debug", err)]
    fn get_compiled_class(
        &self,
        block_id: BlockId,
        class_hash: ClassHash,
    ) -> RpcResult<(CompiledContractClass, SierraVersion)> {
        let storage_txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        let state_reader = storage_txn.get_state_reader().map_err(internal_server_error)?;
        let block_number = get_accepted_block_number(&storage_txn, block_id)?;

        // Check if this class exists in the Cairo1 classes table.
        if let Some(class_definition_block_number) = state_reader
            .get_class_definition_block_number(&class_hash)
            .map_err(internal_server_error)?
        {
            if class_definition_block_number > block_number {
                return Err(ErrorObjectOwned::from(CLASS_HASH_NOT_FOUND));
            }
            let (option_casm, option_sierra) = storage_txn
                .get_casm_and_sierra(&class_hash)
                .map_err(internal_server_error_with_msg)?;

            // Check if both options are `Some`.
            let (casm, sierra) = option_casm
                .zip(option_sierra)
                .ok_or_else(|| ErrorObjectOwned::from(CLASS_HASH_NOT_FOUND))?;
            let sierra_version = SierraVersion::extract_from_program(&sierra.sierra_program)
                .map_err(internal_server_error_with_msg)?;
            return Ok((CompiledContractClass::V1(casm), sierra_version));
        }

        // Check if this class exists in the Cairo0 classes table.
        let state_number = StateNumber::right_after_block(block_number)
            .ok_or_else(|| internal_server_error("Could not compute state number"))?;
        let deprecated_compiled_contract_class = state_reader
            .get_deprecated_class_definition_at(state_number, &class_hash)
            .map_err(internal_server_error)?
            .ok_or_else(|| ErrorObjectOwned::from(CLASS_HASH_NOT_FOUND))?;
        Ok((
            CompiledContractClass::V0(deprecated_compiled_contract_class),
            SierraVersion::DEPRECATED,
        ))
    }
}

async fn read_pending_data<Mode: TransactionKind>(
    pending_data: &Arc<RwLock<PendingData>>,
    txn: &StorageTxn<'_, Mode>,
) -> RpcResult<PendingData> {
    let latest_header = match get_latest_block_number(txn)? {
        Some(latest_block_number) => get_block_header_by_number(txn, latest_block_number)?,
        None => starknet_api::block::BlockHeader {
            block_header_without_hash: BlockHeaderWithoutHash {
                parent_hash: BlockHash(StarkHash::from_hex_unchecked(GENESIS_HASH)),
                ..Default::default()
            },
            ..Default::default()
        },
    };
    let pending_data = &pending_data.read().await;
    if pending_data.block.parent_block_hash() == latest_header.block_hash {
        Ok((*pending_data).clone())
    } else {
        Ok(PendingData {
            block: PendingBlockOrDeprecated::Deprecated(DeprecatedPendingBlock {
                parent_block_hash: latest_header.block_hash,
                eth_l1_gas_price: latest_header.block_header_without_hash.l1_gas_price.price_in_wei,
                strk_l1_gas_price: latest_header
                    .block_header_without_hash
                    .l1_gas_price
                    .price_in_fri,
                timestamp: latest_header.block_header_without_hash.timestamp,
                sequencer_address: latest_header.block_header_without_hash.sequencer,
                starknet_version: latest_header
                    .block_header_without_hash
                    .starknet_version
                    .to_string(),
                ..Default::default()
            }),
            state_update: ClientPendingStateUpdate {
                old_root: latest_header.block_header_without_hash.state_root,
                state_diff: Default::default(),
            },
        })
    }
}

impl JsonRpcServerImpl {
    // Get the block with the given ID and the given custom logic for getting the transactions.
    async fn get_block(
        &self,
        block_id: BlockId,
        get_pending_transactions: impl FnOnce(PendingData) -> RpcResult<Transactions>,
        get_transactions: impl FnOnce(&StorageTxn<'_, RO>, BlockNumber) -> RpcResult<Transactions>,
    ) -> RpcResult<Block> {
        verify_storage_scope(&self.storage_reader)?;
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;
        if let BlockId::Tag(Tag::Pending) = block_id {
            let pending_data = read_pending_data(&self.pending_data, &txn).await?;
            let block = &pending_data.block;
            let pending_block_header = PendingBlockHeader {
                parent_hash: block.parent_block_hash(),
                sequencer_address: block.sequencer_address(),
                timestamp: block.timestamp(),
                l1_gas_price: GasPricePerToken {
                    price_in_wei: block.l1_gas_price().price_in_wei,
                    price_in_fri: block.l1_gas_price().price_in_fri,
                },
                l1_data_gas_price: GasPricePerToken {
                    price_in_wei: block.l1_data_gas_price().price_in_wei,
                    price_in_fri: block.l1_data_gas_price().price_in_fri,
                },
                l2_gas_price: GasPricePerToken {
                    price_in_wei: block.l2_gas_price().price_in_wei,
                    price_in_fri: block.l2_gas_price().price_in_fri,
                },
                l1_da_mode: block.l1_da_mode(),
                starknet_version: block.starknet_version(),
            };
            let header = GeneralBlockHeader::PendingBlockHeader(pending_block_header);

            return Ok(Block {
                status: None,
                header,
                transactions: get_pending_transactions(pending_data)?,
            });
        }

        let block_number = get_accepted_block_number(&txn, block_id)?;
        let status = get_block_status(&txn, block_number)?;
        let header =
            GeneralBlockHeader::BlockHeader(get_block_header_by_number(&txn, block_number)?.into());
        Ok(Block {
            status: Some(status),
            header,
            transactions: get_transactions(&txn, block_number)?,
        })
    }

    async fn maybe_get_class_hash_at(
        &self,
        block_id: BlockId,
        contract_address: ContractAddress,
    ) -> RpcResult<Option<ClassHash>> {
        let txn = self.storage_reader.begin_ro_txn().map_err(internal_server_error)?;

        let maybe_pending_deployed_contracts_and_replaced_classes =
            if let BlockId::Tag(Tag::Pending) = block_id {
                let pending_state_diff =
                    read_pending_data(&self.pending_data, &txn).await?.state_update.state_diff;
                Some((pending_state_diff.deployed_contracts, pending_state_diff.replaced_classes))
            } else {
                None
            };

        let block_number = get_accepted_block_number(&txn, block_id)?;
        let state_number = StateNumber::unchecked_right_after_block(block_number);
        execution_utils::get_class_hash_at(
            &txn,
            state_number,
            // This map converts &(T, S) to (&T, &S).
            maybe_pending_deployed_contracts_and_replaced_classes.as_ref().map(|t| (&t.0, &t.1)),
            contract_address,
        )
        .map_err(internal_server_error)
    }

    async fn is_deployed(
        &self,
        block_number: BlockNumber,
        contract_address: ContractAddress,
    ) -> RpcResult<bool> {
        let block_id = BlockId::HashOrNumber(BlockHashOrNumber::Number(block_number));
        Ok(self.maybe_get_class_hash_at(block_id, contract_address).await?.is_some())
    }

    async fn convert_thin_state_diff(
        &self,
        mut thin_state_diff: StarknetApiThinStateDiff,
        // TODO(AlonH): Remove the `block_id` parameter once we don't have pending blocks.
        block_id: BlockId,
        block_number: BlockNumber,
    ) -> RpcResult<ThinStateDiff> {
        let prev_block_number = match block_id {
            BlockId::Tag(Tag::Pending) => Some(block_number),
            _ => block_number.prev(),
        };
        let mut replaced_classes = vec![];
        for (&address, &class_hash) in thin_state_diff.deployed_contracts.iter() {
            // Check if the class was replaced.
            if let Some(prev_block_number) = prev_block_number {
                if self.is_deployed(prev_block_number, address).await? {
                    replaced_classes.push((address, class_hash));
                }
            }
        }
        replaced_classes.iter().for_each(|(address, _)| {
            thin_state_diff.deployed_contracts.swap_remove(address);
        });
        Ok(ThinStateDiff::from(thin_state_diff, replaced_classes))
    }
}

fn get_non_pending_receipt<Mode: TransactionKind>(
    txn: &StorageTxn<'_, Mode>,
    transaction_index: TransactionIndex,
    transaction_hash: TransactionHash,
    tx_version: TransactionVersion,
    msg_hash: Option<L1L2MsgHash>,
) -> RpcResult<GeneralTransactionReceipt> {
    let block_number = transaction_index.0;
    let status = get_block_status(txn, block_number)?;

    // rejected blocks should not be a part of the API so we early return here.
    // this assumption also holds for the conversion from block status to transaction
    // finality status where we set rejected blocks to unreachable.
    if status == BlockStatus::Rejected {
        return Err(ErrorObjectOwned::from(BLOCK_NOT_FOUND));
    }

    let block_hash =
        get_block_header_by_number(txn, block_number).map_err(internal_server_error)?.block_hash;

    let output = txn
        .get_transaction_output(transaction_index)
        .map_err(internal_server_error)?
        .ok_or_else(|| ErrorObjectOwned::from(TRANSACTION_HASH_NOT_FOUND))?;

    let output = TransactionOutput::from((output, tx_version, msg_hash));

    Ok(GeneralTransactionReceipt::TransactionReceipt(TransactionReceipt {
        finality_status: status.into(),
        transaction_hash,
        block_hash,
        block_number,
        output,
    }))
}

fn client_receipt_to_rpc_pending_receipt(
    client_transaction: &ClientTransaction,
    client_transaction_receipt: ClientTransactionReceipt,
) -> RpcResult<GeneralTransactionReceipt> {
    let transaction_hash = client_transaction.transaction_hash();
    let starknet_api_output =
        client_transaction_receipt.into_starknet_api_transaction_output(client_transaction);
    let msg_hash = match client_transaction {
        ClientTransaction::L1Handler(tx) => Some(tx.calc_msg_hash()),
        _ => None,
    };
    let output = PendingTransactionOutput::try_from(TransactionOutput::from((
        starknet_api_output,
        client_transaction.transaction_version(),
        msg_hash,
    )))?;
    Ok(GeneralTransactionReceipt::PendingTransactionReceipt(PendingTransactionReceipt {
        // ACCEPTED_ON_L2 is the only finality status of a pending transaction.
        finality_status: PendingTransactionFinalityStatus::AcceptedOnL2,
        transaction_hash,
        output,
    }))
}

fn do_event_keys_match_filter(event_content: &EventContent, filter: &EventFilter) -> bool {
    filter.keys.iter().enumerate().all(|(i, keys)| {
        event_content.keys.len() > i && (keys.is_empty() || keys.contains(&event_content.keys[i]))
    })
}

impl JsonRpcServerTrait for JsonRpcServerImpl {
    fn new(
        chain_id: ChainId,
        execution_config: ExecutionConfig,
        storage_reader: StorageReader,
        max_events_chunk_size: usize,
        max_events_keys: usize,
        starting_block: BlockHashAndNumber,
        shared_highest_block: Arc<RwLock<Option<BlockHashAndNumber>>>,
        pending_data: Arc<RwLock<PendingData>>,
        pending_classes: Arc<RwLock<PendingClasses>>,
        writer_client: Arc<dyn StarknetWriter>,
        class_manager_client: Option<SharedClassManagerClient>,
    ) -> Self {
        Self {
            chain_id,
            execution_config,
            storage_reader,
            max_events_chunk_size,
            max_events_keys,
            starting_block,
            shared_highest_block,
            pending_data,
            pending_classes,
            writer_client,
            class_manager_client,
        }
    }

    fn into_rpc_module(self) -> RpcModule<Self> {
        self.into_rpc()
    }
}
