use std::collections::{BTreeMap, HashMap};
use std::convert::TryFrom;
use std::hash::Hash;
use std::ops::Deref;
use std::sync::Arc;

use byteorder::BigEndian;
use cairo_lang_casm::hints::Hint;
use cairo_lang_starknet_classes::casm_contract_class::{
    CasmContractClass,
    CasmContractEntryPoint,
    CasmContractEntryPoints,
};
use cairo_lang_starknet_classes::NestedIntList;
use cairo_lang_utils::bigint::BigUintAsHex;
use indexmap::IndexMap;
use integer_encoding::*;
use num_bigint::BigUint;
use parity_scale_codec::{Decode, Encode};
use primitive_types::H160;
use starknet_api::block::{
    BlockHash,
    BlockNumber,
    BlockSignature,
    BlockStatus,
    BlockTimestamp,
    GasPrice,
    GasPricePerToken,
    StarknetVersion,
};
use starknet_api::contract_class::EntryPointType;
use starknet_api::core::{
    ClassHash,
    CompiledClassHash,
    ContractAddress,
    EntryPointSelector,
    EthAddress,
    EventCommitment,
    GlobalRoot,
    Nonce,
    PatriciaKey,
    ReceiptCommitment,
    SequencerContractAddress,
    StateDiffCommitment,
    TransactionCommitment,
};
use starknet_api::crypto::utils::Signature;
use starknet_api::data_availability::{DataAvailabilityMode, L1DataAvailabilityMode};
use starknet_api::deprecated_contract_class::{
    ConstructorType,
    ContractClass as DeprecatedContractClass,
    ContractClassAbiEntry,
    EntryPointOffset,
    EntryPointV0 as DeprecatedEntryPoint,
    EventAbiEntry,
    EventType,
    FunctionAbiEntry,
    FunctionStateMutability,
    FunctionType,
    L1HandlerType,
    Program,
    StructAbiEntry,
    StructMember,
    StructType,
    TypedParameter,
};
use starknet_api::execution_resources::{Builtin, ExecutionResources, GasAmount, GasVector};
use starknet_api::hash::{PoseidonHash, StarkHash};
use starknet_api::rpc_transaction::EntryPointByType;
use starknet_api::state::{
    EntryPoint,
    FunctionIndex,
    SierraContractClass,
    StorageKey,
    ThinStateDiff,
};
use starknet_api::transaction::fields::{
    AccountDeploymentData,
    AllResourceBounds,
    Calldata,
    ContractAddressSalt,
    Fee,
    PaymasterData,
    Resource,
    ResourceBounds,
    Tip,
    TransactionSignature,
    ValidResourceBounds,
};
use starknet_api::transaction::{
    DeclareTransaction,
    DeclareTransactionOutput,
    DeclareTransactionV0V1,
    DeclareTransactionV2,
    DeclareTransactionV3,
    DeployAccountTransaction,
    DeployAccountTransactionOutput,
    DeployAccountTransactionV1,
    DeployAccountTransactionV3,
    DeployTransaction,
    DeployTransactionOutput,
    Event,
    EventContent,
    EventData,
    EventIndexInTransactionOutput,
    EventKey,
    InvokeTransaction,
    InvokeTransactionOutput,
    InvokeTransactionV0,
    InvokeTransactionV1,
    InvokeTransactionV3,
    L1HandlerTransaction,
    L1HandlerTransactionOutput,
    L1ToL2Payload,
    L2ToL1Payload,
    MessageToL1,
    MessageToL2,
    RevertedTransactionExecutionStatus,
    Transaction,
    TransactionExecutionStatus,
    TransactionHash,
    TransactionOffsetInBlock,
    TransactionOutput,
    TransactionVersion,
};
use starknet_types_core::felt::Felt;
use tracing::warn;

use crate::body::events::EventIndex;
use crate::body::TransactionIndex;
use crate::compression_utils::{
    compress,
    decompress,
    decompress_from_reader,
    serialize_and_compress,
    IsCompressed,
};
use crate::db::serialization::{StorageSerde, StorageSerdeError};
use crate::db::table_types::NoValue;
use crate::header::StorageBlockHeader;
use crate::mmap_file::LocationInFile;
#[cfg(test)]
use crate::serialization::serializers_test::{create_storage_serde_test, StorageSerdeTest};
use crate::state::data::IndexedDeprecatedContractClass;
use crate::version::Version;
use crate::{MarkerKind, OffsetKind, TransactionMetadata};

// The threshold for compressing transactions.
const COMPRESSION_THRESHOLD_BYTES: usize = 384;

auto_storage_serde! {
    pub struct AccountDeploymentData(pub Vec<Felt>);
    pub struct AllResourceBounds {
        pub l1_gas: ResourceBounds,
        pub l2_gas: ResourceBounds,
        pub l1_data_gas: ResourceBounds,
    }
    pub struct BlockHash(pub StarkHash);
    pub struct StorageBlockHeader {
        pub block_hash: BlockHash,
        pub parent_hash: BlockHash,
        pub block_number: BlockNumber,
        pub l1_gas_price: GasPricePerToken,
        pub l1_data_gas_price: GasPricePerToken,
        pub l2_gas_price: GasPricePerToken,
        pub l2_gas_consumed: GasAmount,
        pub next_l2_gas_price: GasPrice,
        pub state_root: GlobalRoot,
        pub sequencer: SequencerContractAddress,
        pub timestamp: BlockTimestamp,
        pub l1_da_mode: L1DataAvailabilityMode,
        pub state_diff_commitment: Option<StateDiffCommitment>,
        pub transaction_commitment: Option<TransactionCommitment>,
        pub event_commitment: Option<EventCommitment>,
        pub receipt_commitment: Option<ReceiptCommitment>,
        pub state_diff_length: Option<usize>,
        pub n_transactions: usize,
        pub n_events: usize,
    }
    pub struct BlockSignature(pub Signature);
    pub enum BlockStatus {
        Pending = 0,
        AcceptedOnL2 = 1,
        AcceptedOnL1 = 2,
        Rejected = 3,
    }
    pub struct BlockTimestamp(pub u64);
    pub struct Calldata(pub Arc<Vec<Felt>>);
    pub struct CompiledClassHash(pub StarkHash);
    pub struct ClassHash(pub StarkHash);
    pub struct ContractAddressSalt(pub StarkHash);
    pub enum ContractClassAbiEntry {
        Event(EventAbiEntry) = 0,
        Function(FunctionAbiEntry<FunctionType>) = 1,
        Constructor(FunctionAbiEntry<ConstructorType>) = 2,
        L1Handler(FunctionAbiEntry<L1HandlerType>) = 3,
        Struct(StructAbiEntry) = 4,
    }
    pub enum DataAvailabilityMode {
        L1 = 0,
        L2 = 1,
    }
    pub enum DeclareTransaction {
        V0(DeclareTransactionV0V1) = 0,
        V1(DeclareTransactionV0V1) = 1,
        V2(DeclareTransactionV2) = 2,
        V3(DeclareTransactionV3) = 3,
    }
    pub struct DeclareTransactionV0V1 {
        pub max_fee: Fee,
        pub signature: TransactionSignature,
        pub nonce: Nonce,
        pub class_hash: ClassHash,
        pub sender_address: ContractAddress,
    }
    pub struct DeclareTransactionV2 {
        pub max_fee: Fee,
        pub signature: TransactionSignature,
        pub nonce: Nonce,
        pub class_hash: ClassHash,
        pub compiled_class_hash: CompiledClassHash,
        pub sender_address: ContractAddress,
    }
    pub struct DeclareTransactionV3 {
        pub resource_bounds: ValidResourceBounds,
        pub tip: Tip,
        pub signature: TransactionSignature,
        pub nonce: Nonce,
        pub class_hash: ClassHash,
        pub compiled_class_hash: CompiledClassHash,
        pub sender_address: ContractAddress,
        pub nonce_data_availability_mode: DataAvailabilityMode,
        pub fee_data_availability_mode: DataAvailabilityMode,
        pub paymaster_data: PaymasterData,
        pub account_deployment_data: AccountDeploymentData,
    }
    pub enum DeployAccountTransaction {
        V1(DeployAccountTransactionV1) = 0,
        V3(DeployAccountTransactionV3) = 1,
    }
    pub struct DeprecatedEntryPoint {
        pub selector: EntryPointSelector,
        pub offset: EntryPointOffset,
    }
    pub struct EntryPoint {
        pub function_idx: FunctionIndex,
        pub selector: EntryPointSelector,
    }
    pub struct EntryPointByType {
        pub constructor: Vec<EntryPoint>,
        pub external: Vec<EntryPoint>,
        pub l1handler: Vec<EntryPoint>,
    }
    pub struct FunctionIndex(pub usize);
    pub struct EntryPointOffset(pub usize);
    pub struct EntryPointSelector(pub StarkHash);
    pub enum EntryPointType {
        Constructor = 0,
        External = 1,
        L1Handler = 2,
    }
    // TODO(dan): consider implementing directly with no H160 dependency.
    pub struct EthAddress(pub H160);
    pub struct EventAbiEntry {
        pub data: Vec<TypedParameter>,
        pub keys: Vec<TypedParameter>,
        pub name: String,
        pub r#type: EventType,
    }
    pub struct Event {
        pub from_address: ContractAddress,
        pub content: EventContent,
    }
    pub struct EventContent {
        pub keys: Vec<EventKey>,
        pub data: EventData,
    }
    pub struct EventCommitment(pub StarkHash);
    pub struct EventData(pub Vec<Felt>);
    struct EventIndex(pub TransactionIndex, pub EventIndexInTransactionOutput);
    pub struct EventIndexInTransactionOutput(pub usize);
    pub struct EventKey(pub Felt);
    pub enum EventType {
        Event = 0,
    }
    pub struct Fee(pub u128);
    pub enum FunctionStateMutability {
        View = 0,
    }
    pub struct GasPrice(pub u128);
    pub struct GasAmount(pub u64);
    pub struct GasPricePerToken {
        pub price_in_fri: GasPrice,
        pub price_in_wei: GasPrice,
    }
    pub struct GasVector {
        pub l1_gas: GasAmount,
        pub l1_data_gas: GasAmount,
        pub l2_gas: GasAmount,
    }
    pub struct GlobalRoot(pub StarkHash);
    pub struct H160(pub [u8; 20]);
    pub struct IndexedDeprecatedContractClass {
        pub block_number: BlockNumber,
        pub location_in_file: LocationInFile,
    }
    pub enum InvokeTransaction {
        V0(InvokeTransactionV0) = 0,
        V1(InvokeTransactionV1) = 1,
        V3(InvokeTransactionV3) = 2,
    }
    pub enum IsCompressed {
        No = 0,
        Yes = 1,
    }
    pub enum L1DataAvailabilityMode {
        Calldata = 0,
        Blob = 1,
    }
    pub struct L1ToL2Payload(pub Vec<Felt>);
    pub struct L2ToL1Payload(pub Vec<Felt>);
    enum MarkerKind {
        Header = 0,
        Body = 1,
        Event = 2,
        State = 3,
        Class = 4,
        CompiledClass = 5,
        BaseLayerBlock = 6,
        ClassManagerBlock = 7,
        CompilerBackwardCompatibility = 8,
    }
    pub struct MessageToL1 {
        pub to_address: EthAddress,
        pub payload: L2ToL1Payload,
        pub from_address: ContractAddress,
    }
    pub struct MessageToL2 {
        pub from_address: EthAddress,
        pub payload: L1ToL2Payload,
    }
    pub enum NestedIntList {
        Leaf(usize) = 0,
        Node(Vec<NestedIntList>) = 1,
    }
    pub struct Nonce(pub Felt);
    pub enum OffsetKind {
        ThinStateDiff = 0,
        ContractClass = 1,
        Casm = 2,
        DeprecatedContractClass = 3,
        TransactionOutput = 4,
        Transaction = 5,
    }
    pub struct PaymasterData(pub Vec<Felt>);
    pub struct PoseidonHash(pub Felt);
    pub struct Program {
        pub attributes: serde_json::Value,
        pub builtins: serde_json::Value,
        pub compiler_version: serde_json::Value,
        pub data: serde_json::Value,
        pub debug_info: serde_json::Value,
        pub hints: serde_json::Value,
        pub identifiers: serde_json::Value,
        pub main_scope: serde_json::Value,
        pub prime: serde_json::Value,
        pub reference_manager: serde_json::Value,
    }
    pub struct ReceiptCommitment(pub StarkHash);
    pub enum Resource {
        L1Gas = 0,
        L2Gas = 1,
        L1DataGas = 2,
    }
    pub struct ResourceBounds {
        pub max_amount: GasAmount,
        pub max_price_per_unit: GasPrice,
    }
    pub struct SequencerContractAddress(pub ContractAddress);
    pub struct Signature {
        pub r: Felt,
        pub s: Felt,
    }
    pub struct StructAbiEntry {
        pub members: Vec<StructMember>,
        pub name: String,
        pub size: usize,
        pub r#type: StructType,
    }
    pub struct StructMember {
        pub name: String,
        pub offset: usize,
        pub r#type: String,
    }
    pub enum StructType {
        Struct = 0,
    }
    pub enum StarknetVersion {
        V0_9_1 = 0,
        V0_10_0 = 1,
        V0_10_1 = 2,
        V0_10_2 = 3,
        V0_10_3 = 4,
        V0_11_0 = 5,
        V0_11_0_2 = 6,
        V0_11_1 = 7,
        V0_11_2 = 8,
        V0_12_0 = 9,
        V0_12_1 = 10,
        V0_12_2 = 11,
        V0_12_3 = 12,
        V0_13_0 = 13,
        V0_13_1 = 14,
        V0_13_1_1 = 15,
        V0_13_2 = 16,
        V0_13_2_1 = 17,
        V0_13_3 = 18,
        V0_13_4 = 19,
        V0_13_5 = 20,
        V0_13_6 = 21,
        V0_14_0 = 22,
        V0_15_0 = 23,
    }
    pub struct StateDiffCommitment(pub PoseidonHash);
    pub struct Tip(pub u64);
    pub struct TransactionCommitment(pub StarkHash);
    pub struct TypedParameter {
        pub name: String,
        pub r#type: String,
    }
    pub struct TransactionMetadata {
        pub tx_hash: TransactionHash,
        pub tx_location: LocationInFile,
        pub tx_output_location: LocationInFile,
    }
    pub enum Transaction {
        Declare(DeclareTransaction) = 0,
        Deploy(DeployTransaction) = 1,
        DeployAccount(DeployAccountTransaction) = 2,
        Invoke(InvokeTransaction) = 3,
        L1Handler(L1HandlerTransaction) = 4,
    }
    pub enum TransactionExecutionStatus {
        Succeeded = 0,
        Reverted(RevertedTransactionExecutionStatus) = 1,
    }
    pub struct RevertedTransactionExecutionStatus {
        pub revert_reason: String,
    }
    pub struct TransactionHash(pub StarkHash);
    struct TransactionIndex(pub BlockNumber, pub TransactionOffsetInBlock);
    pub enum TransactionOutput {
        Declare(DeclareTransactionOutput) = 0,
        Deploy(DeployTransactionOutput) = 1,
        DeployAccount(DeployAccountTransactionOutput) = 2,
        Invoke(InvokeTransactionOutput) = 3,
        L1Handler(L1HandlerTransactionOutput) = 4,
    }
    pub struct TransactionSignature(pub Arc<Vec<Felt>>);
    pub struct TransactionVersion(pub Felt);
    pub enum ValidResourceBounds {
        L1Gas(ResourceBounds) = 0,
        AllResources(AllResourceBounds) = 1,
    }
    pub struct Version{
        pub major: u32,
        pub minor: u32,
    }

    pub struct CasmContractEntryPoints {
        pub external: Vec<CasmContractEntryPoint>,
        pub l1_handler: Vec<CasmContractEntryPoint>,
        pub constructor: Vec<CasmContractEntryPoint>,
    }

    pub struct CasmContractEntryPoint {
        pub selector: BigUint,
        pub offset: usize,
        pub builtins: Vec<String>,
    }

    pub struct BigUintAsHex {
        pub value: BigUint,
    }

    pub struct ExecutionResources {
        pub steps: u64,
        pub builtin_instance_counter: HashMap<Builtin, u64>,
        pub memory_holes: u64,
        pub da_gas_consumed: GasVector,
        pub gas_consumed: GasVector,
    }

    pub enum Builtin {
        RangeCheck = 0,
        Pedersen = 1,
        Poseidon = 2,
        EcOp = 3,
        Ecdsa = 4,
        Bitwise = 5,
        Keccak = 6,
        SegmentArena = 7,
        AddMod = 8,
        MulMod = 9,
        RangeCheck96 = 10,
    }

    binary(u32, read_u32, write_u32);
    binary(u64, read_u64, write_u64);
    binary(u128, read_u128, write_u128);


    (BlockNumber, TransactionOffsetInBlock);
    (BlockHash, ClassHash);
    (ClassHash, BlockNumber);
    (ContractAddress, BlockHash);
    (ContractAddress, BlockNumber);
    (ContractAddress, Nonce);
    (ContractAddress, StorageKey);
    (ContractAddress, TransactionIndex);
    ((ContractAddress, StorageKey), BlockNumber);
    (usize, Vec<Hint>);
    (usize, Vec<String>);
}

////////////////////////////////////////////////////////////////////////
//  impl StorageSerde macro.
////////////////////////////////////////////////////////////////////////
#[allow(unused_macro_rules)]
macro_rules! auto_storage_serde {
    () => {};
    // Tuple structs (no names associated with fields) - one field.
    ($(pub)? struct $name:ident($(pub)? $ty:ty); $($rest:tt)*) => {
        impl StorageSerde for $name {
            fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
                self.0.serialize_into(res)
            }
            fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
                Some(Self (<$ty>::deserialize_from(bytes)?))
            }
        }
        #[cfg(test)]
        create_storage_serde_test!($name);
        auto_storage_serde!($($rest)*);
    };
    // Tuple structs (no names associated with fields) - two fields.
    ($(pub)? struct $name:ident($(pub)? $ty0:ty, $(pub)? $ty1:ty) ; $($rest:tt)*) => {
        impl StorageSerde for $name {
            fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
                self.0.serialize_into(res)?;
                self.1.serialize_into(res)
            }
            fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
                Some($name(<$ty0>::deserialize_from(bytes)?, <$ty1>::deserialize_from(bytes)?))
            }
        }
        #[cfg(test)]
        create_storage_serde_test!($name);
        auto_storage_serde!($($rest)*);
    };
    // Structs with public fields.
    ($(pub)? struct $name:ident { $(pub $field:ident : $ty:ty ,)* } $($rest:tt)*) => {
        impl StorageSerde for $name {
            fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
                $(
                    self.$field.serialize_into(res)?;
                )*
                Ok(())
            }
            fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
                Some(Self {
                    $(
                        $field: <$ty>::deserialize_from(bytes)?,
                    )*
                })
            }
        }
        #[cfg(test)]
        create_storage_serde_test!($name);
        auto_storage_serde!($($rest)*);
    };
    // Tuples - two elements.
    (($ty0:ty, $ty1:ty) ; $($rest:tt)*) => {
        impl StorageSerde for ($ty0, $ty1) {
            fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
                self.0.serialize_into(res)?;
                self.1.serialize_into(res)
            }
            fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
                Some((
                    <$ty0>::deserialize_from(bytes)?,
                    <$ty1>::deserialize_from(bytes)?,
                ))
            }
        }
        auto_storage_serde!($($rest)*);
    };
    // Tuples - three elements.
    (($ty0:ty, $ty1:ty, $ty2:ty) ; $($rest:tt)*) => {
        impl StorageSerde for ($ty0, $ty1, $ty2) {
            fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
                self.0.serialize_into(res)?;
                self.1.serialize_into(res)?;
                self.2.serialize_into(res)
            }
            fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
                Some((
                    <$ty0>::deserialize_from(bytes)?,
                    <$ty1>::deserialize_from(bytes)?,
                    <$ty2>::deserialize_from(bytes)?,
                ))
            }
        }
        auto_storage_serde!($($rest)*);
    };
    // enums.
    ($(pub)? enum $name:ident { $($variant:ident $( ($ty:ty) )? = $num:expr ,)* } $($rest:tt)*) => {
        impl StorageSerde for $name {
            fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
                #[allow(clippy::as_conversions)]
                match self {
                    $(
                        variant!( value, $variant $( ($ty) )?) => {
                            res.write_all(&[$num as u8])?;
                            $(
                                (value as &$ty).serialize_into(res)?;
                            )?
                            Ok(())
                        }
                    )*
                }
            }
            fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
                let mut kind = [0u8; 1];
                bytes.read_exact(&mut kind).ok()?;
                match kind[0] {
                    $(
                        $num => {
                            Some(Self::$variant $( (<$ty>::deserialize_from(bytes)?) )? )
                        },
                    )*
                    _ => None,}
            }
        }
        #[cfg(test)]
        create_storage_serde_test!($name);
        auto_storage_serde!($($rest)*);
    };
    // Binary.
    (binary($name:ident, $read:ident, $write:ident); $($rest:tt)*) => {
        impl StorageSerde for $name {
            fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
                Ok(byteorder::WriteBytesExt::$write::<BigEndian>(res, *self)?)
            }

            fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
                byteorder::ReadBytesExt::$read::<BigEndian>(bytes).ok()
            }
        }
        #[cfg(test)]
        create_storage_serde_test!($name);
        auto_storage_serde!($($rest)*);
    }
}
pub(crate) use auto_storage_serde;

// Helper macro.
macro_rules! variant {
    ($value:ident, $variant:ident) => {
        Self::$variant
    };
    ($value:ident, $variant:ident($ty:ty)) => {
        Self::$variant($value)
    };
}
pub(crate) use variant;

////////////////////////////////////////////////////////////////////////
// Starknet API structs.
////////////////////////////////////////////////////////////////////////
impl StorageSerde for ContractAddress {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        self.0.serialize_into(res)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        ContractAddress::try_from(StarkHash::deserialize(bytes)?).ok()
    }
}

impl StorageSerde for PatriciaKey {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        self.key().serialize_into(res)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        Self::try_from(StarkHash::deserialize(bytes)?).ok()
    }
}

impl StorageSerde for StarkHash {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        Ok(self.serialize(res)?)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        Self::deserialize(bytes)
    }
}

impl StorageSerde for StorageKey {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        self.0.serialize_into(res)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        StorageKey::try_from(StarkHash::deserialize(bytes)?).ok()
    }
}

////////////////////////////////////////////////////////////////////////
//  Primitive types.
////////////////////////////////////////////////////////////////////////
impl StorageSerde for bool {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        u8::from(*self).serialize_into(res)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        Some((u8::deserialize_from(bytes)?) != 0)
    }
}

// TODO(spapini): Perhaps compress this textual data.
impl StorageSerde for serde_json::Value {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        let bytes = serde_json::to_vec(self)?;
        bytes.serialize_into(res)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let buf = Vec::deserialize_from(bytes)?;
        serde_json::from_slice(buf.as_slice()).ok()
    }
}

impl StorageSerde for String {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        (self.as_bytes().to_vec()).serialize_into(res)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        Self::from_utf8(Vec::deserialize_from(bytes)?).ok()
    }
}

impl<T: StorageSerde> StorageSerde for Option<T> {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        match self {
            Some(value) => {
                res.write_all(&[1])?;
                value.serialize_into(res)
            }
            None => Ok(res.write_all(&[0])?),
        }
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let mut exists = [0u8; 1];
        bytes.read_exact(&mut exists).ok()?;
        match exists[0] {
            0 => Some(None),
            1 => Some(Some(T::deserialize_from(bytes)?)),
            _ => None,
        }
    }
}

impl StorageSerde for u8 {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        Ok(byteorder::WriteBytesExt::write_u8(res, *self)?)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        byteorder::ReadBytesExt::read_u8(bytes).ok()
    }
}

// TODO(dan): get rid of usize.
impl StorageSerde for usize {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        (u64::try_from(*self).expect("usize should fit in u64")).serialize_into(res)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        usize::try_from(u64::deserialize_from(bytes)?).ok()
    }
}

impl<T: StorageSerde> StorageSerde for Vec<T> {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        res.write_varint(self.len())?;
        for x in self {
            x.serialize_into(res)?
        }
        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let n: usize = bytes.read_varint().ok()?;
        let mut res = Vec::with_capacity(n);
        for _i in 0..n {
            res.push(T::deserialize_from(bytes)?);
        }
        Some(res)
    }
}
impl<K: StorageSerde + Eq + Hash, V: StorageSerde> StorageSerde for HashMap<K, V> {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        res.write_varint(self.len())?;
        for (k, v) in self.iter() {
            k.serialize_into(res)?;
            v.serialize_into(res)?;
        }
        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let n: usize = bytes.read_varint().ok()?;
        let mut res = HashMap::with_capacity(n);
        for _i in 0..n {
            let k = K::deserialize_from(bytes)?;
            let v = V::deserialize_from(bytes)?;
            if res.insert(k, v).is_some() {
                return None;
            }
        }
        Some(res)
    }
}
// TODO(anatg): Find a way to share code with StorageSerde for HashMap.
impl<K: StorageSerde + Eq + Hash, V: StorageSerde> StorageSerde for IndexMap<K, V> {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        res.write_varint(self.len())?;
        for (k, v) in self.iter() {
            k.serialize_into(res)?;
            v.serialize_into(res)?;
        }
        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let n: usize = bytes.read_varint().ok()?;
        let mut res = IndexMap::with_capacity(n);
        for _i in 0..n {
            let k = K::deserialize_from(bytes)?;
            let v = V::deserialize_from(bytes)?;
            if res.insert(k, v).is_some() {
                return None;
            }
        }
        Some(res)
    }
}
impl<K: StorageSerde + Eq + Ord, V: StorageSerde> StorageSerde for BTreeMap<K, V> {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        res.write_varint(self.len())?;
        for (k, v) in self.iter() {
            k.serialize_into(res)?;
            v.serialize_into(res)?;
        }
        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let n: usize = bytes.read_varint().ok()?;
        let mut res = BTreeMap::new();
        for _i in 0..n {
            let k = K::deserialize_from(bytes)?;
            let v = V::deserialize_from(bytes)?;
            if res.insert(k, v).is_some() {
                return None;
            }
        }
        Some(res)
    }
}
impl<T: StorageSerde + Default + Copy, const N: usize> StorageSerde for [T; N] {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        for x in self {
            x.serialize_into(res)?;
        }
        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let mut res = [T::default(); N];
        for elm in res.iter_mut() {
            *elm = T::deserialize_from(bytes)?;
        }
        Some(res)
    }
}
impl<T: StorageSerde> StorageSerde for Arc<T> {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        self.deref().serialize_into(res)?;
        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let res = T::deserialize_from(bytes)?;
        Some(Arc::new(res))
    }
}

impl StorageSerde for Hint {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        self.encode().serialize_into(res)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        Self::decode(&mut Vec::<u8>::deserialize_from(bytes)?.as_slice()).ok()
    }
}

impl StorageSerde for BigUint {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        self.to_bytes_be().serialize_into(res)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let bytes_be = Vec::<u8>::deserialize_from(bytes)?;
        Some(BigUint::from_bytes_be(bytes_be.as_slice()))
    }
}

impl StorageSerde for NoValue {
    fn serialize_into(&self, _res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        Ok(())
    }

    fn deserialize_from(_bytes: &mut impl std::io::Read) -> Option<Self> {
        Some(Self)
    }
}

////////////////////////////////////////////////////////////////////////
//  Custom serialization for storage reduction.
////////////////////////////////////////////////////////////////////////
// TODO(dvir): remove this when BlockNumber will be u32.
impl StorageSerde for BlockNumber {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        u32::try_from(self.0).expect("BlockNumber should fit into 32 bits.").serialize_into(res)
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        Some(BlockNumber(u32::deserialize_from(bytes)?.into()))
    }
}

// This serialization write the offset as 3 bytes, which means that the maximum offset is ~16
// million (1<<24 bytes).
impl StorageSerde for TransactionOffsetInBlock {
    fn serialize_into(
        &self,
        res: &mut impl std::io::Write,
    ) -> Result<(), crate::db::serialization::StorageSerdeError> {
        let bytes = &self.0.to_be_bytes();
        res.write_all(&bytes[5..])?;
        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let mut arr = [0u8; 8];
        bytes.read_exact(&mut arr[5..]).ok()?;
        let index = usize::from_be_bytes(arr);
        Some(Self(index))
    }
}

////////////////////////////////////////////////////////////////////////
//  Custom serialization with compression.
////////////////////////////////////////////////////////////////////////
impl StorageSerde for SierraContractClass {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        serialize_and_compress(&self.sierra_program)?.serialize_into(res)?;
        self.contract_class_version.serialize_into(res)?;
        self.entry_points_by_type.serialize_into(res)?;
        serialize_and_compress(&self.abi)?.serialize_into(res)?;
        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        Some(Self {
            sierra_program: Vec::<Felt>::deserialize_from(
                &mut decompress_from_reader(bytes)?.as_slice(),
            )?,
            contract_class_version: String::deserialize_from(bytes)?,
            entry_points_by_type: EntryPointByType::deserialize_from(bytes)?,
            abi: String::deserialize_from(&mut decompress_from_reader(bytes)?.as_slice())?,
        })
    }
}
#[cfg(test)]
create_storage_serde_test!(SierraContractClass);

impl StorageSerde for DeprecatedContractClass {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        // Compress together the program and abi for better compression results.
        let mut to_compress: Vec<u8> = Vec::new();
        self.abi.serialize_into(&mut to_compress)?;
        self.program.serialize_into(&mut to_compress)?;
        if to_compress.len() > crate::compression_utils::MAX_DECOMPRESSED_SIZE {
            warn!(
                "DeprecatedContractClass serialization size is too large and will lead to \
                 deserialization error: {}",
                to_compress.len()
            );
        }
        let compressed = compress(to_compress.as_slice())?;
        compressed.serialize_into(res)?;
        self.entry_points_by_type.serialize_into(res)?;
        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let compressed_data = Vec::<u8>::deserialize_from(bytes)?;
        let data = decompress(compressed_data.as_slice())
            .expect("destination buffer should be large enough");
        let data = &mut data.as_slice();
        Some(Self {
            abi: Option::<Vec<ContractClassAbiEntry>>::deserialize_from(data)?,
            program: Program::deserialize_from(data)?,
            entry_points_by_type:
                HashMap::<EntryPointType, Vec<DeprecatedEntryPoint>>::deserialize_from(bytes)?,
        })
    }
}
#[cfg(test)]
create_storage_serde_test!(DeprecatedContractClass);

impl<TYPE: Default> StorageSerde for FunctionAbiEntry<TYPE> {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        self.name.serialize_into(res)?;
        self.inputs.serialize_into(res)?;
        self.outputs.serialize_into(res)?;
        self.state_mutability.serialize_into(res)?;
        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        Some(Self {
            name: String::deserialize_from(bytes)?,
            inputs: Vec::<TypedParameter>::deserialize_from(bytes)?,
            outputs: Vec::<TypedParameter>::deserialize_from(bytes)?,
            state_mutability: Option::<FunctionStateMutability>::deserialize_from(bytes)?,
            r#type: TYPE::default(),
        })
    }
}

impl StorageSerde for CasmContractClass {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        let mut to_compress: Vec<u8> = Vec::new();
        self.prime.serialize_into(&mut to_compress)?;
        self.compiler_version.serialize_into(&mut to_compress)?;
        self.bytecode.serialize_into(&mut to_compress)?;
        self.bytecode_segment_lengths.serialize_into(&mut to_compress)?;
        self.hints.serialize_into(&mut to_compress)?;
        self.pythonic_hints.serialize_into(&mut to_compress)?;
        self.entry_points_by_type.serialize_into(&mut to_compress)?;
        if to_compress.len() > crate::compression_utils::MAX_DECOMPRESSED_SIZE {
            warn!(
                "CasmContractClass serialization size is too large and will lead to \
                 deserialization error: {}",
                to_compress.len()
            );
        }
        let compressed = compress(to_compress.as_slice())?;
        compressed.serialize_into(res)?;

        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let compressed_data = Vec::<u8>::deserialize_from(bytes)?;
        let data = decompress(compressed_data.as_slice())
            .expect("destination buffer should be large enough");
        let data = &mut data.as_slice();
        Some(Self {
            prime: BigUint::deserialize_from(data)?,
            compiler_version: String::deserialize_from(data)?,
            bytecode: Vec::<BigUintAsHex>::deserialize_from(data)?,
            bytecode_segment_lengths: Option::<NestedIntList>::deserialize_from(data)?,
            hints: Vec::<(usize, Vec<Hint>)>::deserialize_from(data)?,
            pythonic_hints: Option::<Vec<(usize, Vec<String>)>>::deserialize_from(data)?,
            entry_points_by_type: CasmContractEntryPoints::deserialize_from(data)?,
        })
    }
}

#[cfg(test)]
create_storage_serde_test!(CasmContractClass);

impl StorageSerde for ThinStateDiff {
    fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
        let mut to_compress: Vec<u8> = Vec::new();
        self.deployed_contracts.serialize_into(&mut to_compress)?;
        self.storage_diffs.serialize_into(&mut to_compress)?;
        self.declared_classes.serialize_into(&mut to_compress)?;
        self.deprecated_declared_classes.serialize_into(&mut to_compress)?;
        self.nonces.serialize_into(&mut to_compress)?;
        if to_compress.len() > crate::compression_utils::MAX_DECOMPRESSED_SIZE {
            warn!(
                "ThinStateDiff serialization size is too large and will lead to deserialization \
                 error: {}",
                to_compress.len()
            );
        }
        let compressed = compress(to_compress.as_slice())?;
        compressed.serialize_into(res)?;
        Ok(())
    }

    fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
        let compressed_data = Vec::<u8>::deserialize_from(bytes)?;
        let data = decompress(compressed_data.as_slice())
            .expect("destination buffer should be large enough");
        let data = &mut data.as_slice();
        Some(Self {
            deployed_contracts: IndexMap::deserialize_from(data)?,
            storage_diffs: IndexMap::deserialize_from(data)?,
            declared_classes: IndexMap::deserialize_from(data)?,
            deprecated_declared_classes: Vec::deserialize_from(data)?,
            nonces: IndexMap::deserialize_from(data)?,
        })
    }
}

#[cfg(test)]
create_storage_serde_test!(ThinStateDiff);

// The following structs are conditionally compressed based on their serialized size.
macro_rules! auto_storage_serde_conditionally_compressed {
    () => {};
    ($(pub)? struct $name:ident { $(pub $field:ident : $ty:ty ,)* } $($rest:tt)*) => {
        impl StorageSerde for $name {
            fn serialize_into(&self, res: &mut impl std::io::Write) -> Result<(), StorageSerdeError> {
                let mut to_compress: Vec<u8> = Vec::new();
                $(
                    self.$field.serialize_into(&mut to_compress)?;
                )*
                if to_compress.len() > COMPRESSION_THRESHOLD_BYTES {
                    IsCompressed::Yes.serialize_into(res)?;
                    if to_compress.len() > crate::compression_utils::MAX_DECOMPRESSED_SIZE {
                        warn!(
                            "{} serialization size is too large and will lead to deserialization \
                            error: {}",
                            stringify!($name),
                            to_compress.len()
                        );
                    }
                    let compressed = compress(to_compress.as_slice())?;
                    compressed.serialize_into(res)?;
                } else {
                    IsCompressed::No.serialize_into(res)?;
                    to_compress.serialize_into(res)?;
                }
                Ok(())
            }
            fn deserialize_from(bytes: &mut impl std::io::Read) -> Option<Self> {
                let is_compressed = IsCompressed::deserialize_from(bytes)?;
                let maybe_compressed_data = Vec::<u8>::deserialize_from(bytes)?;
                let data = match is_compressed {
                    IsCompressed::No => maybe_compressed_data,
                    IsCompressed::Yes => decompress(
                        maybe_compressed_data.as_slice())
                            .expect("destination buffer should be large enough"),
                };
                let data = &mut data.as_slice();
                Some(Self {
                    $(
                        $field: <$ty>::deserialize_from(data)?,
                    )*
                })
            }
        }
        #[cfg(test)]
        create_storage_serde_test!($name);
        auto_storage_serde_conditionally_compressed!($($rest)*);
    };
}

// The following transactions have variable length Calldata and are conditionally compressed.
auto_storage_serde_conditionally_compressed! {
    pub struct DeployAccountTransactionV1 {
        pub max_fee: Fee,
        pub signature: TransactionSignature,
        pub nonce: Nonce,
        pub class_hash: ClassHash,
        pub contract_address_salt: ContractAddressSalt,
        pub constructor_calldata: Calldata,
    }

    pub struct DeployAccountTransactionV3 {
        pub resource_bounds: ValidResourceBounds,
        pub tip: Tip,
        pub signature: TransactionSignature,
        pub nonce: Nonce,
        pub class_hash: ClassHash,
        pub contract_address_salt: ContractAddressSalt,
        pub constructor_calldata: Calldata,
        pub nonce_data_availability_mode: DataAvailabilityMode,
        pub fee_data_availability_mode: DataAvailabilityMode,
        pub paymaster_data: PaymasterData,
    }

    pub struct DeployTransaction {
        pub version: TransactionVersion,
        pub class_hash: ClassHash,
        pub contract_address_salt: ContractAddressSalt,
        pub constructor_calldata: Calldata,
    }

    pub struct InvokeTransactionV0 {
        pub max_fee: Fee,
        pub signature: TransactionSignature,
        pub contract_address: ContractAddress,
        pub entry_point_selector: EntryPointSelector,
        pub calldata: Calldata,
    }

    pub struct InvokeTransactionV1 {
        pub max_fee: Fee,
        pub signature: TransactionSignature,
        pub nonce: Nonce,
        pub sender_address: ContractAddress,
        pub calldata: Calldata,
    }

    pub struct InvokeTransactionV3 {
        pub resource_bounds: ValidResourceBounds,
        pub tip: Tip,
        pub signature: TransactionSignature,
        pub nonce: Nonce,
        pub sender_address: ContractAddress,
        pub calldata: Calldata,
        pub nonce_data_availability_mode: DataAvailabilityMode,
        pub fee_data_availability_mode: DataAvailabilityMode,
        pub paymaster_data: PaymasterData,
        pub account_deployment_data: AccountDeploymentData,
    }

    pub struct L1HandlerTransaction {
        pub version: TransactionVersion,
        pub nonce: Nonce,
        pub contract_address: ContractAddress,
        pub entry_point_selector: EntryPointSelector,
        pub calldata: Calldata,
    }

    pub struct DeclareTransactionOutput {
        pub actual_fee: Fee,
        pub messages_sent: Vec<MessageToL1>,
        pub events: Vec<Event>,
        pub execution_status: TransactionExecutionStatus,
        pub execution_resources: ExecutionResources,
    }

    pub struct DeployAccountTransactionOutput {
        pub actual_fee: Fee,
        pub messages_sent: Vec<MessageToL1>,
        pub events: Vec<Event>,
        pub contract_address: ContractAddress,
        pub execution_status: TransactionExecutionStatus,
        pub execution_resources: ExecutionResources,
    }

    pub struct DeployTransactionOutput {
        pub actual_fee: Fee,
        pub messages_sent: Vec<MessageToL1>,
        pub events: Vec<Event>,
        pub contract_address: ContractAddress,
        pub execution_status: TransactionExecutionStatus,
        pub execution_resources: ExecutionResources,
    }

    pub struct InvokeTransactionOutput {
        pub actual_fee: Fee,
        pub messages_sent: Vec<MessageToL1>,
        pub events: Vec<Event>,
        pub execution_status: TransactionExecutionStatus,
        pub execution_resources: ExecutionResources,
    }

    pub struct L1HandlerTransactionOutput {
        pub actual_fee: Fee,
        pub messages_sent: Vec<MessageToL1>,
        pub events: Vec<Event>,
        pub execution_status: TransactionExecutionStatus,
        pub execution_resources: ExecutionResources,
    }
}
