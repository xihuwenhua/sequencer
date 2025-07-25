//! Execution objects.
use std::collections::HashMap;

use blockifier::context::BlockContext;
use blockifier::execution::call_info::{
    CallInfo,
    OrderedEvent as BlockifierOrderedEvent,
    OrderedL2ToL1Message as BlockifierOrderedL2ToL1Message,
    Retdata as BlockifierRetdata,
};
use blockifier::execution::entry_point::CallType as BlockifierCallType;
use blockifier::transaction::objects::TransactionExecutionInfo;
use blockifier::utils::u64_from_usize;
use cairo_vm::types::builtin_name::BuiltinName;
use cairo_vm::vm::runners::cairo_runner::ExecutionResources as VmExecutionResources;
use indexmap::IndexMap;
use itertools::Itertools;
use papyrus_common::pending_classes::PendingClasses;
use papyrus_common::state::{
    DeclaredClassHashEntry,
    DeployedContract,
    ReplacedClass,
    StorageEntry,
};
use serde::{Deserialize, Serialize};
use starknet_api::block::{BlockTimestamp, FeeType, GasPrice, GasPricePerToken};
use starknet_api::contract_class::EntryPointType;
use starknet_api::core::{
    ClassHash,
    ContractAddress,
    EntryPointSelector,
    EthAddress,
    Nonce,
    SequencerContractAddress,
};
use starknet_api::data_availability::L1DataAvailabilityMode;
use starknet_api::execution_resources::{
    Builtin,
    ExecutionResources,
    GasVector,
    GasVector as StarknetApiGasVector,
};
use starknet_api::state::ThinStateDiff;
use starknet_api::transaction::fields::{Calldata, Fee};
use starknet_api::transaction::{EventContent, MessageToL1};
use starknet_types_core::felt::Felt;

use crate::{ExecutionError, ExecutionResult, TransactionExecutionOutput};

// TODO(yair): Move types to starknet_api.

/// The output of simulating a transaction.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct TransactionSimulationOutput {
    /// The execution trace of the transaction.
    pub transaction_trace: TransactionTrace,
    /// The state diff induced by the transaction.
    pub induced_state_diff: ThinStateDiff,
    /// The details of the fees charged by the transaction.
    pub fee_estimation: FeeEstimation,
}

/// The execution trace of a transaction.
#[allow(missing_docs)]
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(tag = "type")]
pub enum TransactionTrace {
    #[serde(rename = "L1_HANDLER")]
    L1Handler(L1HandlerTransactionTrace),
    #[serde(rename = "INVOKE")]
    Invoke(InvokeTransactionTrace),
    #[serde(rename = "DECLARE")]
    Declare(DeclareTransactionTrace),
    #[serde(rename = "DEPLOY_ACCOUNT")]
    DeployAccount(DeployAccountTransactionTrace),
}

/// The execution trace of an Invoke transaction.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct InvokeTransactionTrace {
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The trace of the __validate__ call.
    pub validate_invocation: Option<FunctionInvocation>,
    /// The trace of the __execute__ call or the reason in case of reverted transaction.
    pub execute_invocation: FunctionInvocationResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// The trace of the __fee_transfer__ call.
    pub fee_transfer_invocation: Option<FunctionInvocation>,
}

/// Output for successful fee estimation.
// TODO(shahak): We assume that this struct has the same deserialization as the RPC specs v0.7.
// Consider duplicating this struct inside the RPC crate.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct FeeEstimation {
    /// Gas consumed by this transaction. This includes gas for DA in calldata mode.
    pub gas_consumed: Felt,
    /// The gas price for execution and calldata DA.
    pub l1_gas_price: GasPrice,
    /// Gas consumed by DA in blob mode.
    pub data_gas_consumed: Felt,
    /// The gas price for DA blob.
    pub l1_data_gas_price: GasPrice,
    // TODO(Tzahi): Add l2_gas_consumed. Verify overall_fee estimation of l1_gas_price only is
    // close enough (as there are roundings) to the fee of both l1_gas_price and l2_gas_price.
    /// The L2 gas price for execution.
    pub l2_gas_price: GasPrice,
    /// The total amount of fee. This is equal to:
    /// gas_consumed * gas_price + data_gas_consumed * data_gas_price.
    pub overall_fee: Fee,
    /// The unit in which the fee was paid (Wei/Fri).
    pub unit: PriceUnit,
}

/// The reason for a reverted transaction.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum RevertReason {
    #[serde(rename = "revert_reason")]
    RevertReason(String),
}

impl TryFrom<TransactionExecutionInfo> for InvokeTransactionTrace {
    type Error = ExecutionError;
    fn try_from(transaction_execution_info: TransactionExecutionInfo) -> ExecutionResult<Self> {
        let execute_invocation = match transaction_execution_info.revert_error {
            Some(revert_error) => {
                FunctionInvocationResult::Err(RevertReason::RevertReason(revert_error.to_string()))
            }
            None => FunctionInvocationResult::Ok(
                (
                    transaction_execution_info
                        .execute_call_info
                        .expect("Invoke transaction execution should contain execute_call_info."),
                    transaction_execution_info.receipt.da_gas,
                )
                    .try_into()?,
            ),
        };

        Ok(Self {
            validate_invocation: match transaction_execution_info.validate_call_info {
                None => None,
                Some(call_info) => {
                    Some((call_info, transaction_execution_info.receipt.da_gas).try_into()?)
                }
            },
            execute_invocation,
            fee_transfer_invocation: match transaction_execution_info.fee_transfer_call_info {
                None => None,
                Some(call_info) => {
                    Some((call_info, transaction_execution_info.receipt.da_gas).try_into()?)
                }
            },
        })
    }
}

// TODO(Dan, Yair): consider box large elements (because of BadDeclareTransaction) or use ID
// instead.
pub(crate) fn tx_execution_output_to_fee_estimation(
    tx_execution_output: &TransactionExecutionOutput,
    block_context: &BlockContext,
) -> ExecutionResult<FeeEstimation> {
    let gas_prices = &block_context.block_info().gas_prices;
    let (l1_gas_price, l1_data_gas_price, l2_gas_price) = (
        gas_prices.l1_gas_price(&tx_execution_output.price_unit.into()).get(),
        gas_prices.l1_data_gas_price(&tx_execution_output.price_unit.into()).get(),
        gas_prices.l2_gas_price(&tx_execution_output.price_unit.into()).get(),
    );

    let gas_vector = tx_execution_output.execution_info.receipt.gas;

    Ok(FeeEstimation {
        gas_consumed: gas_vector.l1_gas.0.into(),
        l1_gas_price,
        data_gas_consumed: gas_vector.l1_data_gas.0.into(),
        l1_data_gas_price,
        l2_gas_price,
        overall_fee: tx_execution_output.execution_info.receipt.fee,
        unit: tx_execution_output.price_unit,
    })
}

/// The execution trace of a Declare transaction.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct DeclareTransactionTrace {
    /// The trace of the __validate__ call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validate_invocation: Option<FunctionInvocation>,
    /// The trace of the __fee_transfer__ call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_transfer_invocation: Option<FunctionInvocation>,
}

impl TryFrom<TransactionExecutionInfo> for DeclareTransactionTrace {
    type Error = ExecutionError;
    fn try_from(transaction_execution_info: TransactionExecutionInfo) -> ExecutionResult<Self> {
        Ok(Self {
            validate_invocation: match transaction_execution_info.validate_call_info {
                None => None,
                Some(call_info) => {
                    Some((call_info, transaction_execution_info.receipt.da_gas).try_into()?)
                }
            },
            fee_transfer_invocation: match transaction_execution_info.fee_transfer_call_info {
                None => None,
                Some(call_info) => {
                    Some((call_info, transaction_execution_info.receipt.da_gas).try_into()?)
                }
            },
        })
    }
}

/// The execution trace of a DeployAccount transaction.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct DeployAccountTransactionTrace {
    /// The trace of the __validate__ call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub validate_invocation: Option<FunctionInvocation>,
    /// The trace of the __constructor__ call.
    pub constructor_invocation: FunctionInvocation,
    /// The trace of the __fee_transfer__ call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_transfer_invocation: Option<FunctionInvocation>,
}

impl TryFrom<TransactionExecutionInfo> for DeployAccountTransactionTrace {
    type Error = ExecutionError;
    fn try_from(transaction_execution_info: TransactionExecutionInfo) -> ExecutionResult<Self> {
        Ok(Self {
            validate_invocation: match transaction_execution_info.validate_call_info {
                None => None,
                Some(call_info) => {
                    Some((call_info, transaction_execution_info.receipt.da_gas).try_into()?)
                }
            },
            constructor_invocation: (
                transaction_execution_info.execute_call_info.expect(
                    "Deploy account execution should contain execute_call_info (the constructor \
                     call info).",
                ),
                transaction_execution_info.receipt.da_gas,
            )
                .try_into()?,
            fee_transfer_invocation: match transaction_execution_info.fee_transfer_call_info {
                None => None,
                Some(call_info) => {
                    Some((call_info, transaction_execution_info.receipt.da_gas).try_into()?)
                }
            },
        })
    }
}

/// The execution trace of an L1Handler transaction.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct L1HandlerTransactionTrace {
    /// The trace of the funcion call.
    pub function_invocation: FunctionInvocation,
}

impl TryFrom<TransactionExecutionInfo> for L1HandlerTransactionTrace {
    type Error = ExecutionError;
    fn try_from(transaction_execution_info: TransactionExecutionInfo) -> ExecutionResult<Self> {
        Ok(Self {
            function_invocation: (
                transaction_execution_info
                    .execute_call_info
                    .expect("L1Handler execution should contain execute_call_info."),
                transaction_execution_info.receipt.da_gas,
            )
                .try_into()?,
        })
    }
}

/// Wether the function invocation succeeded or reverted.
// Not using `Result` because it is not being serialized according to the spec.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[allow(missing_docs)]
#[serde(untagged)]
pub enum FunctionInvocationResult {
    Ok(FunctionInvocation),
    Err(RevertReason),
}

/// The execution trace of a function call.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct FunctionInvocation {
    #[serde(flatten)]
    /// The details of the function call.
    pub function_call: FunctionCall,
    /// The address of the invoking contract. 0 for the root invocation.
    pub caller_address: ContractAddress,
    /// The hash of the class being called.
    pub class_hash: ClassHash,
    /// The type of the entry point being called.
    pub entry_point_type: EntryPointType,
    /// library call or regular call.
    pub call_type: CallType,
    /// The value returned from the function invocation.
    pub result: Retdata,
    /// The calls made by this invocation.
    pub calls: Vec<Self>,
    /// The events emitted in this invocation.
    pub events: Vec<OrderedEvent>,
    /// The messages sent by this invocation to L1.
    pub messages: Vec<OrderedL2ToL1Message>,
    /// The VM execution resources used by this invocation.
    pub execution_resources: ExecutionResources,
}

impl TryFrom<(CallInfo, GasVector)> for FunctionInvocation {
    type Error = ExecutionError;
    fn try_from((call_info, gas_vector): (CallInfo, GasVector)) -> ExecutionResult<Self> {
        Ok(Self {
            function_call: FunctionCall {
                contract_address: call_info.call.storage_address,
                entry_point_selector: call_info.call.entry_point_selector,
                calldata: call_info.call.calldata,
            },
            caller_address: call_info.call.caller_address,
            class_hash: call_info.call.class_hash.ok_or(ExecutionError::MissingClassHash)?, /* TODO(DanB): fix this. */
            entry_point_type: call_info.call.entry_point_type,
            call_type: call_info.call.call_type.into(),
            result: call_info.execution.retdata.into(),
            calls: call_info
                .inner_calls
                .into_iter()
                .map(|call_info| (call_info, gas_vector))
                .map(Self::try_from)
                .collect::<Result<_, _>>()?,
            events: call_info
                .execution
                .events
                .into_iter()
                .sorted_by_key(|ordered_event| ordered_event.order)
                .map(OrderedEvent::from)
                .collect(),
            messages: call_info
                .execution
                .l2_to_l1_messages
                .into_iter()
                .sorted_by_key(|ordered_message| ordered_message.order)
                .map(|ordered_message| {
                    // TODO(yair): write a test that verifies that the from_address is correct.
                    OrderedL2ToL1Message::from(ordered_message, call_info.call.storage_address)
                })
                .collect(),
            execution_resources: vm_resources_to_execution_resources(
                call_info.resources,
                gas_vector,
            )?,
        })
    }
}

// Can't implement `TryFrom` because both types are from external crates.
// TODO(Dan, Yair): consider box large elements (because of BadDeclareTransaction) or use ID
// instead.
fn vm_resources_to_execution_resources(
    vm_resources: VmExecutionResources,
    GasVector { l1_gas, l1_data_gas, l2_gas }: GasVector,
) -> ExecutionResult<ExecutionResources> {
    let mut builtin_instance_counter = HashMap::new();
    for (builtin_name, count) in vm_resources.builtin_instance_counter {
        if count == 0 {
            continue;
        }
        let count = u64_from_usize(count);
        match builtin_name {
            BuiltinName::output => continue,
            BuiltinName::pedersen => builtin_instance_counter.insert(Builtin::Pedersen, count),
            BuiltinName::range_check => builtin_instance_counter.insert(Builtin::RangeCheck, count),
            BuiltinName::ecdsa => builtin_instance_counter.insert(Builtin::Ecdsa, count),
            BuiltinName::bitwise => builtin_instance_counter.insert(Builtin::Bitwise, count),
            BuiltinName::ec_op => builtin_instance_counter.insert(Builtin::EcOp, count),
            BuiltinName::keccak => builtin_instance_counter.insert(Builtin::Keccak, count),
            BuiltinName::poseidon => builtin_instance_counter.insert(Builtin::Poseidon, count),
            BuiltinName::segment_arena => {
                builtin_instance_counter.insert(Builtin::SegmentArena, count)
            }
            // TODO(DanB): what about the following?
            // BuiltinName::range_check96 => todo!(),
            // BuiltinName::add_mod => todo!(),
            // BuiltinName::mul_mod => todo!(),
            _ => {
                return Err(ExecutionError::UnknownBuiltin { builtin_name });
            }
        };
    }
    Ok(ExecutionResources {
        steps: u64_from_usize(vm_resources.n_steps),
        builtin_instance_counter,
        memory_holes: u64_from_usize(vm_resources.n_memory_holes),
        da_gas_consumed: StarknetApiGasVector { l1_gas, l2_gas, l1_data_gas },
        gas_consumed: StarknetApiGasVector::default(),
    })
}

/// library call or regular call.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[allow(missing_docs)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CallType {
    Call,
    LibraryCall,
}

impl From<BlockifierCallType> for CallType {
    fn from(call_type: BlockifierCallType) -> Self {
        match call_type {
            BlockifierCallType::Call => CallType::Call,
            BlockifierCallType::Delegate => CallType::LibraryCall,
        }
    }
}

/// The return data of a function call.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Retdata(pub Vec<Felt>);

impl From<BlockifierRetdata> for Retdata {
    fn from(retdata: BlockifierRetdata) -> Self {
        Self(retdata.0)
    }
}

/// An event emitted by a contract.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OrderedEvent {
    /// The order of the event in the transaction.
    pub order: usize,
    #[serde(flatten)]
    /// The event.
    pub event: EventContent,
}

impl From<BlockifierOrderedEvent> for OrderedEvent {
    fn from(ordered_event: BlockifierOrderedEvent) -> Self {
        Self { order: ordered_event.order, event: ordered_event.event }
    }
}

/// A message sent from L2 to L1.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OrderedL2ToL1Message {
    /// The order of the message in the transaction.
    pub order: usize,
    #[serde(flatten)]
    /// The message.
    pub message: MessageToL1,
}

impl OrderedL2ToL1Message {
    /// Constructs a new `OrderedL2ToL1Message`.
    pub fn from(
        blockifier_message: BlockifierOrderedL2ToL1Message,
        from_address: ContractAddress,
    ) -> Self {
        Self {
            order: blockifier_message.order,
            message: MessageToL1 {
                from_address,
                to_address: EthAddress::try_from(blockifier_message.message.to_address)
                    .expect("Failed to convert L1Address to EthAddress"),
                payload: blockifier_message.message.payload,
            },
        }
    }
}

/// The details of a function call.
#[derive(Clone, Debug, Serialize, Deserialize, Eq, PartialEq)]
pub struct FunctionCall {
    /// The address of the contract being called.
    pub contract_address: ContractAddress,
    /// The selector of the entry point being called.
    pub entry_point_selector: EntryPointSelector,
    /// The calldata of the function call.
    pub calldata: Calldata,
}

/// A state diff for the pending block.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct PendingData {
    // TODO(shahak): Consider indexing by address and key.
    /// All the contract storages that were changed in the pending block.
    pub storage_diffs: IndexMap<ContractAddress, Vec<StorageEntry>>,
    /// All the contracts that were deployed in the pending block.
    pub deployed_contracts: Vec<DeployedContract>,
    /// All the classes that were declared in the pending block.
    pub declared_classes: Vec<DeclaredClassHashEntry>,
    /// All the deprecated classes that were declared in the pending block.
    pub old_declared_contracts: Vec<ClassHash>,
    /// All the nonces that were changed in the pending block.
    pub nonces: IndexMap<ContractAddress, Nonce>,
    /// All the classes that were declared in the pending block.
    pub replaced_classes: Vec<ReplacedClass>,
    /// The timestamp of the pending block.
    pub timestamp: BlockTimestamp,
    /// The gas price of the pending block.
    pub l1_gas_price: GasPricePerToken,
    /// The data price of the pending block.
    pub l1_data_gas_price: GasPricePerToken,
    /// The L2 gas price of the pending block.
    pub l2_gas_price: GasPricePerToken,
    /// The data availability mode of the pending block.
    pub l1_da_mode: L1DataAvailabilityMode,
    /// The sequencer address of the pending block.
    pub sequencer: SequencerContractAddress,
    /// The classes and casms that were declared in the pending block.
    pub classes: PendingClasses,
}

/// The unit of the fee.
#[derive(
    Debug, Default, Clone, Copy, Eq, Hash, PartialEq, Deserialize, Serialize, PartialOrd, Ord,
)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PriceUnit {
    /// Wei.
    #[default]
    Wei,
    /// Fri.
    Fri,
}

impl From<PriceUnit> for FeeType {
    fn from(value: PriceUnit) -> Self {
        match value {
            PriceUnit::Wei => FeeType::Eth,
            PriceUnit::Fri => FeeType::Strk,
        }
    }
}
