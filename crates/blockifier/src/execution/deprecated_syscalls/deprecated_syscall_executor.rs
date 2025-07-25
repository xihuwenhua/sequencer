use std::collections::HashMap;

use cairo_vm::hint_processor::builtin_hint_processor::hint_utils::get_ptr_from_var_name;
use cairo_vm::hint_processor::hint_processor_definition::HintReference;
use cairo_vm::serde::deserialize_program::ApTracking;
use cairo_vm::types::errors::math_errors::MathError;
use cairo_vm::types::relocatable::Relocatable;
use cairo_vm::vm::errors::hint_errors::HintError;
use cairo_vm::vm::errors::memory_errors::MemoryError;
use cairo_vm::vm::errors::vm_errors::VirtualMachineError;
use cairo_vm::vm::vm_core::VirtualMachine;
use num_bigint::{BigUint, TryFromBigIntError};
use starknet_api::StarknetApiError;
use starknet_types_core::felt::Felt;

use crate::execution::deprecated_syscalls::{
    CallContractRequest,
    CallContractResponse,
    DelegateCallRequest,
    DelegateCallResponse,
    DeployRequest,
    DeployResponse,
    DeprecatedSyscallSelector,
    EmitEventRequest,
    EmitEventResponse,
    GetBlockNumberRequest,
    GetBlockNumberResponse,
    GetBlockTimestampRequest,
    GetBlockTimestampResponse,
    GetCallerAddressRequest,
    GetCallerAddressResponse,
    GetContractAddressRequest,
    GetContractAddressResponse,
    GetSequencerAddressRequest,
    GetSequencerAddressResponse,
    GetTxInfoRequest,
    GetTxInfoResponse,
    GetTxSignatureRequest,
    GetTxSignatureResponse,
    LibraryCallRequest,
    LibraryCallResponse,
    ReplaceClassRequest,
    ReplaceClassResponse,
    SendMessageToL1Request,
    SendMessageToL1Response,
    StorageReadRequest,
    StorageReadResponse,
    StorageWriteRequest,
    StorageWriteResponse,
    SyscallRequest,
    SyscallResponse,
};
use crate::execution::execution_utils::felt_from_ptr;

pub trait DeprecatedSyscallExecutor {
    type Error: From<DeprecatedSyscallExecutorBaseError>;

    fn read_next_syscall_selector(
        &mut self,
        vm: &mut VirtualMachine,
    ) -> DeprecatedSyscallExecutorBaseResult<Felt> {
        Ok(felt_from_ptr(vm, self.get_mut_syscall_ptr())?)
    }

    fn increment_syscall_count(&mut self, selector: &DeprecatedSyscallSelector);

    fn verify_syscall_ptr(&self, actual_ptr: Relocatable) -> Result<(), Self::Error>;

    fn get_mut_syscall_ptr(&mut self) -> &mut Relocatable;

    fn call_contract(
        request: CallContractRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<CallContractResponse, Self::Error>;

    fn delegate_call(
        request: DelegateCallRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<DelegateCallResponse, Self::Error>;

    fn delegate_l1_handler(
        request: DelegateCallRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<DelegateCallResponse, Self::Error>;

    fn deploy(
        request: DeployRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<DeployResponse, Self::Error>;

    fn emit_event(
        request: EmitEventRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<EmitEventResponse, Self::Error>;

    fn get_block_number(
        request: GetBlockNumberRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<GetBlockNumberResponse, Self::Error>;

    fn get_block_timestamp(
        request: GetBlockTimestampRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<GetBlockTimestampResponse, Self::Error>;

    fn get_caller_address(
        request: GetCallerAddressRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<GetCallerAddressResponse, Self::Error>;

    fn get_contract_address(
        request: GetContractAddressRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<GetContractAddressResponse, Self::Error>;

    fn get_sequencer_address(
        request: GetSequencerAddressRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<GetSequencerAddressResponse, Self::Error>;

    fn get_tx_info(
        request: GetTxInfoRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<GetTxInfoResponse, Self::Error>;

    fn get_tx_signature(
        request: GetTxSignatureRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<GetTxSignatureResponse, Self::Error>;

    fn library_call(
        request: LibraryCallRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<LibraryCallResponse, Self::Error>;

    fn library_call_l1_handler(
        request: LibraryCallRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<LibraryCallResponse, Self::Error>;

    fn replace_class(
        request: ReplaceClassRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<ReplaceClassResponse, Self::Error>;

    fn send_message_to_l1(
        request: SendMessageToL1Request,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<SendMessageToL1Response, Self::Error>;

    fn storage_read(
        request: StorageReadRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<StorageReadResponse, Self::Error>;

    fn storage_write(
        request: StorageWriteRequest,
        vm: &mut VirtualMachine,
        syscall_handler: &mut Self,
    ) -> Result<StorageWriteResponse, Self::Error>;
}

pub fn execute_deprecated_syscall_from_selector<T: DeprecatedSyscallExecutor>(
    deprecated_syscall_executor: &mut T,
    vm: &mut VirtualMachine,
    selector: DeprecatedSyscallSelector,
) -> Result<(), T::Error> {
    match selector {
        DeprecatedSyscallSelector::CallContract => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::call_contract)
        }
        DeprecatedSyscallSelector::DelegateCall => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::delegate_call)
        }
        DeprecatedSyscallSelector::DelegateL1Handler => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::delegate_l1_handler)
        }
        DeprecatedSyscallSelector::Deploy => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::deploy)
        }
        DeprecatedSyscallSelector::EmitEvent => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::emit_event)
        }
        DeprecatedSyscallSelector::GetBlockNumber => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::get_block_number)
        }
        DeprecatedSyscallSelector::GetBlockTimestamp => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::get_block_timestamp)
        }
        DeprecatedSyscallSelector::GetCallerAddress => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::get_caller_address)
        }
        DeprecatedSyscallSelector::GetContractAddress => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::get_contract_address)
        }
        DeprecatedSyscallSelector::GetSequencerAddress => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::get_sequencer_address)
        }
        DeprecatedSyscallSelector::GetTxInfo => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::get_tx_info)
        }
        DeprecatedSyscallSelector::GetTxSignature => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::get_tx_signature)
        }
        DeprecatedSyscallSelector::LibraryCall => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::library_call)
        }
        DeprecatedSyscallSelector::LibraryCallL1Handler => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::library_call_l1_handler)
        }
        DeprecatedSyscallSelector::ReplaceClass => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::replace_class)
        }
        DeprecatedSyscallSelector::SendMessageToL1 => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::send_message_to_l1)
        }
        DeprecatedSyscallSelector::StorageRead => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::storage_read)
        }
        DeprecatedSyscallSelector::StorageWrite => {
            execute_deprecated_syscall(deprecated_syscall_executor, vm, T::storage_write)
        }
        // Explicitly list unsupported syscalls, so compiler can catch if a syscall is missing.
        DeprecatedSyscallSelector::GetBlockHash
        | DeprecatedSyscallSelector::GetClassHashAt
        | DeprecatedSyscallSelector::GetExecutionInfo
        | DeprecatedSyscallSelector::Keccak
        | DeprecatedSyscallSelector::KeccakRound
        | DeprecatedSyscallSelector::Sha256ProcessBlock
        | DeprecatedSyscallSelector::MetaTxV0
        | DeprecatedSyscallSelector::Secp256k1Add
        | DeprecatedSyscallSelector::Secp256k1GetPointFromX
        | DeprecatedSyscallSelector::Secp256k1GetXy
        | DeprecatedSyscallSelector::Secp256k1Mul
        | DeprecatedSyscallSelector::Secp256k1New
        | DeprecatedSyscallSelector::Secp256r1Add
        | DeprecatedSyscallSelector::Secp256r1GetPointFromX
        | DeprecatedSyscallSelector::Secp256r1GetXy
        | DeprecatedSyscallSelector::Secp256r1Mul
        | DeprecatedSyscallSelector::Secp256r1New => {
            Err(DeprecatedSyscallExecutorBaseError::from(HintError::UnknownHint(
                format!("Unsupported syscall selector {selector:?}.").into(),
            )))?
        }
    }
}

fn execute_deprecated_syscall<Request, Response, ExecuteCallback, Executor>(
    deprecated_syscall_executor: &mut Executor,
    vm: &mut VirtualMachine,
    execute_callback: ExecuteCallback,
) -> Result<(), Executor::Error>
where
    Executor: DeprecatedSyscallExecutor,
    Request: SyscallRequest,
    Response: SyscallResponse,
    ExecuteCallback:
        FnOnce(Request, &mut VirtualMachine, &mut Executor) -> Result<Response, Executor::Error>,
{
    let request = Request::read(vm, deprecated_syscall_executor.get_mut_syscall_ptr())?;

    let response = execute_callback(request, vm, deprecated_syscall_executor)?;
    response.write(vm, deprecated_syscall_executor.get_mut_syscall_ptr())?;

    Ok(())
}

/// Infers and executes the next syscall.
/// Must comply with the API of a hint function, as defined by the `HintProcessor`.
pub fn execute_next_deprecated_syscall<T: DeprecatedSyscallExecutor>(
    deprecated_syscall_executor: &mut T,
    vm: &mut VirtualMachine,
    ids_data: &HashMap<String, HintReference>,
    ap_tracking: &ApTracking,
) -> Result<(), T::Error> {
    let initial_syscall_ptr = get_ptr_from_var_name("syscall_ptr", vm, ids_data, ap_tracking)
        .map_err(DeprecatedSyscallExecutorBaseError::from)?;
    deprecated_syscall_executor.verify_syscall_ptr(initial_syscall_ptr)?;

    let selector = DeprecatedSyscallSelector::try_from(
        deprecated_syscall_executor.read_next_syscall_selector(vm)?,
    )?;
    deprecated_syscall_executor.increment_syscall_count(&selector);

    execute_deprecated_syscall_from_selector(deprecated_syscall_executor, vm, selector)
}

#[derive(Debug, thiserror::Error)]
pub enum DeprecatedSyscallExecutorBaseError {
    #[error("Bad syscall_ptr; expected: {expected_ptr:?}, got: {actual_ptr:?}.")]
    BadSyscallPointer { expected_ptr: Relocatable, actual_ptr: Relocatable },
    #[error(transparent)]
    Hint(#[from] HintError),
    #[error("Invalid syscall selector: {0:?}.")]
    InvalidDeprecatedSyscallSelector(Felt),
    #[error("Invalid syscall input: {input:?}; {info}")]
    InvalidSyscallInput { input: Felt, info: String },
    #[error(transparent)]
    Math(#[from] MathError),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    StarknetApi(#[from] StarknetApiError),
    #[error(transparent)]
    FromBigUint(#[from] TryFromBigIntError<BigUint>),
    #[error(transparent)]
    VirtualMachine(#[from] VirtualMachineError),
}

pub type DeprecatedSyscallExecutorBaseResult<T> = Result<T, DeprecatedSyscallExecutorBaseError>;

// Needed for custom hint implementations (in our case, syscall hints) which must comply with the
// cairo-rs API.
impl From<DeprecatedSyscallExecutorBaseError> for HintError {
    fn from(error: DeprecatedSyscallExecutorBaseError) -> Self {
        Self::Internal(VirtualMachineError::Other(error.into()))
    }
}
