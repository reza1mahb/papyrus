use std::collections::HashSet;
use std::io::Read;

use flate2::bufread::GzDecoder;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::ErrorObjectOwned;
use papyrus_common::BlockHashAndNumber;
use papyrus_execution::objects::TransactionTrace;
use papyrus_execution::{ExecutableTransactionInput, ExecutionError};
use papyrus_proc_macros::versioned_rpc;
use papyrus_storage::db::RO;
use papyrus_storage::state::StateStorageReader;
use papyrus_storage::StorageTxn;
use serde::{Deserialize, Serialize};
use starknet_api::block::{BlockNumber, GasPrice};
use starknet_api::core::{ClassHash, ContractAddress, EntryPointSelector, Nonce};
use starknet_api::deprecated_contract_class::Program;
use starknet_api::hash::StarkFelt;
use starknet_api::state::{StateNumber, StorageKey};
use starknet_api::transaction::{
    Calldata,
    EventKey,
    Fee,
    TransactionHash,
    TransactionOffsetInBlock,
};

use super::block::Block;
use super::broadcasted_transaction::{
    BroadcastedDeclareTransaction,
    BroadcastedDeclareV1Transaction,
    BroadcastedTransaction,
};
use super::deprecated_contract_class::ContractClass as DeprecatedContractClass;
use super::error::{JsonRpcError, BLOCK_NOT_FOUND, CONTRACT_ERROR, CONTRACT_NOT_FOUND};
use super::state::{ContractClass, StateUpdate};
use super::transaction::{
    DeployAccountTransaction,
    Event,
    InvokeTransaction,
    InvokeTransactionV0,
    InvokeTransactionV1,
    TransactionReceipt,
    TransactionWithHash,
};
use super::write_api_result::{AddDeclareOkResult, AddDeployAccountOkResult, AddInvokeOkResult};
use crate::api::BlockId;
use crate::syncing_state::SyncingState;
use crate::v0_4_0::error::INVALID_CONTINUATION_TOKEN;
use crate::{internal_server_error, ContinuationTokenAsStruct};

pub mod api_impl;
#[cfg(test)]
mod test;

#[versioned_rpc("V0_4")]
#[async_trait]
pub trait JsonRpc {
    /// Gets the most recent accepted block number.
    #[method(name = "blockNumber")]
    fn block_number(&self) -> RpcResult<BlockNumber>;

    /// Gets the most recent accepted block hash and number.
    #[method(name = "blockHashAndNumber")]
    fn block_hash_and_number(&self) -> RpcResult<BlockHashAndNumber>;

    /// Gets block information with transaction hashes given a block identifier.
    #[method(name = "getBlockWithTxHashes")]
    fn get_block_w_transaction_hashes(&self, block_id: BlockId) -> RpcResult<Block>;

    /// Gets block information with full transactions given a block identifier.
    #[method(name = "getBlockWithTxs")]
    fn get_block_w_full_transactions(&self, block_id: BlockId) -> RpcResult<Block>;

    /// Gets the value of the storage at the given address, key, and block.
    #[method(name = "getStorageAt")]
    fn get_storage_at(
        &self,
        contract_address: ContractAddress,
        key: StorageKey,
        block_id: BlockId,
    ) -> RpcResult<StarkFelt>;

    /// Gets the details of a submitted transaction.
    #[method(name = "getTransactionByHash")]
    fn get_transaction_by_hash(
        &self,
        transaction_hash: TransactionHash,
    ) -> RpcResult<TransactionWithHash>;

    /// Gets the details of a transaction by a given block id and index.
    #[method(name = "getTransactionByBlockIdAndIndex")]
    fn get_transaction_by_block_id_and_index(
        &self,
        block_id: BlockId,
        index: TransactionOffsetInBlock,
    ) -> RpcResult<TransactionWithHash>;

    /// Gets the number of transactions in a block given a block id.
    #[method(name = "getBlockTransactionCount")]
    fn get_block_transaction_count(&self, block_id: BlockId) -> RpcResult<usize>;

    /// Gets the information about the result of executing the requested block.
    #[method(name = "getStateUpdate")]
    fn get_state_update(&self, block_id: BlockId) -> RpcResult<StateUpdate>;

    /// Gets the transaction receipt by the transaction hash.
    #[method(name = "getTransactionReceipt")]
    fn get_transaction_receipt(
        &self,
        transaction_hash: TransactionHash,
    ) -> RpcResult<TransactionReceipt>;

    /// Gets the contract class definition associated with the given hash.
    #[method(name = "getClass")]
    fn get_class(
        &self,
        block_id: BlockId,
        class_hash: ClassHash,
    ) -> RpcResult<GatewayContractClass>;

    /// Gets the contract class definition in the given block at the given address.
    #[method(name = "getClassAt")]
    fn get_class_at(
        &self,
        block_id: BlockId,
        contract_address: ContractAddress,
    ) -> RpcResult<GatewayContractClass>;

    /// Gets the contract class hash in the given block for the contract deployed at the given
    /// address.
    #[method(name = "getClassHashAt")]
    fn get_class_hash_at(
        &self,
        block_id: BlockId,
        contract_address: ContractAddress,
    ) -> RpcResult<ClassHash>;

    /// Gets the nonce associated with the given address in the given block.
    #[method(name = "getNonce")]
    fn get_nonce(&self, block_id: BlockId, contract_address: ContractAddress) -> RpcResult<Nonce>;

    /// Returns the currently configured StarkNet chain id.
    #[method(name = "chainId")]
    fn chain_id(&self) -> RpcResult<String>;

    /// Returns all events matching the given filter.
    #[method(name = "getEvents")]
    fn get_events(&self, filter: EventFilter) -> RpcResult<EventsChunk>;

    /// Returns the synching status of the node, or false if the node is not synching.
    #[method(name = "syncing")]
    async fn syncing(&self) -> RpcResult<SyncingState>;

    /// Executes the entry point of the contract at the given address with the given calldata,
    /// returns the result (Retdata).
    #[method(name = "call")]
    fn call(
        &self,
        contract_address: ContractAddress,
        entry_point_selector: EntryPointSelector,
        calldata: Calldata,
        block_id: BlockId,
    ) -> RpcResult<Vec<StarkFelt>>;

    /// Submits a new invoke transaction to be added to the chain.
    #[method(name = "addInvokeTransaction")]
    async fn add_invoke_transaction(
        &self,
        invoke_transaction: InvokeTransactionV1,
    ) -> RpcResult<AddInvokeOkResult>;

    /// Submits a new deploy account transaction to be added to the chain.
    #[method(name = "addDeployAccountTransaction")]
    async fn add_deploy_account_transaction(
        &self,
        deploy_account_transaction: DeployAccountTransaction,
    ) -> RpcResult<AddDeployAccountOkResult>;

    /// Submits a new declare transaction to be added to the chain.
    #[method(name = "addDeclareTransaction")]
    async fn add_declare_transaction(
        &self,
        declare_transaction: BroadcastedDeclareTransaction,
    ) -> RpcResult<AddDeclareOkResult>;

    /// Estimates the fee of a series of transactions.
    #[method(name = "estimateFee")]
    fn estimate_fee(
        &self,
        transactions: Vec<BroadcastedTransaction>,
        block_id: BlockId,
    ) -> RpcResult<Vec<FeeEstimate>>;

    /// Simulates execution of a series of transactions.
    #[method(name = "simulateTransactions")]
    fn simulate_transactions(
        &self,
        block_id: BlockId,
        transactions: Vec<BroadcastedTransaction>,
        simulation_flags: Vec<SimulationFlag>,
    ) -> RpcResult<Vec<SimulatedTransaction>>;

    /// Calculates the transaction trace of a transaction that is already included in a block.
    #[method(name = "traceTransaction")]
    fn trace_transaction(&self, transaction_hash: TransactionHash) -> RpcResult<TransactionTrace>;
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum GatewayContractClass {
    Cairo0(DeprecatedContractClass),
    Sierra(ContractClass),
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct EventsChunk {
    pub events: Vec<Event>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation_token: Option<ContinuationToken>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct EventFilter {
    pub from_block: Option<BlockId>,
    pub to_block: Option<BlockId>,
    pub continuation_token: Option<ContinuationToken>,
    pub chunk_size: usize,
    pub address: Option<ContractAddress>,
    #[serde(default)]
    pub keys: Vec<HashSet<EventKey>>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq, Deserialize, Serialize)]
pub struct ContinuationToken(pub String);

impl ContinuationToken {
    fn parse(&self) -> Result<ContinuationTokenAsStruct, ErrorObjectOwned> {
        let ct = serde_json::from_str(&self.0)
            .map_err(|_| ErrorObjectOwned::from(INVALID_CONTINUATION_TOKEN))?;

        Ok(ContinuationTokenAsStruct(ct))
    }

    fn new(ct: ContinuationTokenAsStruct) -> Result<Self, ErrorObjectOwned> {
        Ok(Self(serde_json::to_string(&ct.0).map_err(internal_server_error)?))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct FeeEstimate {
    pub gas_consumed: StarkFelt,
    pub gas_price: GasPrice,
    pub overall_fee: Fee,
}

impl FeeEstimate {
    pub fn from(gas_price: GasPrice, overall_fee: Fee) -> Self {
        match gas_price {
            GasPrice(0) => Self::default(),
            _ => {
                Self { gas_consumed: (overall_fee.0 / gas_price.0).into(), gas_price, overall_fee }
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SimulatedTransaction {
    pub transaction_trace: TransactionTrace,
    pub fee_estimation: FeeEstimate,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SimulationFlag {
    SkipValidate,
    SkipFeeCharge,
}

impl TryFrom<BroadcastedTransaction> for ExecutableTransactionInput {
    type Error = ErrorObjectOwned;
    fn try_from(value: BroadcastedTransaction) -> Result<Self, Self::Error> {
        Ok(match value {
            BroadcastedTransaction::Declare(tx) => tx.try_into()?,
            BroadcastedTransaction::DeployAccount(tx) => Self::Deploy(tx),
            BroadcastedTransaction::Invoke(tx) => Self::Invoke(tx.into()),
        })
    }
}

pub(crate) fn stored_txn_to_executable_txn(
    stored_txn: starknet_api::transaction::Transaction,
    storage_txn: &StorageTxn<'_, RO>,
    state_number: StateNumber,
) -> Result<ExecutableTransactionInput, ErrorObjectOwned> {
    match stored_txn {
        starknet_api::transaction::Transaction::Declare(
            starknet_api::transaction::DeclareTransaction::V0(value),
        ) => {
            // Copy the class hash before the value moves.
            let class_hash = value.class_hash;
            Ok(ExecutableTransactionInput::DeclareV0(
                value,
                storage_txn
                    .get_state_reader()
                    .map_err(internal_server_error)?
                    .get_deprecated_class_definition_at(state_number, &class_hash)
                    .map_err(internal_server_error)?
                    .ok_or(internal_server_error(format!(
                        "Missing deprecated class definition of {class_hash}."
                    )))?,
            ))
        }
        starknet_api::transaction::Transaction::Declare(
            starknet_api::transaction::DeclareTransaction::V1(value),
        ) => {
            // Copy the class hash before the value moves.
            let class_hash = value.class_hash;
            Ok(ExecutableTransactionInput::DeclareV1(
                value,
                storage_txn
                    .get_state_reader()
                    .map_err(internal_server_error)?
                    .get_deprecated_class_definition_at(state_number, &class_hash)
                    .map_err(internal_server_error)?
                    .ok_or(internal_server_error(format!(
                        "Missing deprecated class definition of {class_hash}."
                    )))?,
            ))
        }
        starknet_api::transaction::Transaction::Declare(
            starknet_api::transaction::DeclareTransaction::V2(_),
        ) => Err(internal_server_error("Declare v2 txns not supported yet in execution")),
        starknet_api::transaction::Transaction::Deploy(_) => {
            Err(internal_server_error("Deploy txns not supported in execution"))
        }
        starknet_api::transaction::Transaction::DeployAccount(value) => {
            Ok(ExecutableTransactionInput::Deploy(value))
        }
        starknet_api::transaction::Transaction::Invoke(value) => {
            Ok(ExecutableTransactionInput::Invoke(value))
        }
        starknet_api::transaction::Transaction::L1Handler(_) => {
            Err(internal_server_error("L1 handler txns not supported in execution"))
        }
    }
}

impl TryFrom<BroadcastedDeclareTransaction> for ExecutableTransactionInput {
    type Error = ErrorObjectOwned;
    fn try_from(value: BroadcastedDeclareTransaction) -> Result<Self, Self::Error> {
        match value {
            BroadcastedDeclareTransaction::V1(BroadcastedDeclareV1Transaction {
                r#type: _,
                contract_class,
                sender_address,
                nonce,
                max_fee,
                signature,
            }) => Ok(Self::DeclareV1(
                starknet_api::transaction::DeclareTransactionV0V1 {
                    max_fee,
                    signature,
                    nonce,
                    // The blockifier doesn't need the class hash, but it uses the SN_API
                    // DeclareTransactionV0V1 which requires it.
                    class_hash: ClassHash::default(),
                    sender_address,
                },
                user_deprecated_contract_class_to_sn_api(contract_class)?,
            )),
            BroadcastedDeclareTransaction::V2(_) => {
                // TODO(yair): We need a way to get the casm of a declare V2 transaction.
                Err(internal_server_error("Declare V2 is not supported yet in execution."))
            }
        }
    }
}

fn user_deprecated_contract_class_to_sn_api(
    value: starknet_client::writer::objects::transaction::DeprecatedContractClass,
) -> Result<starknet_api::deprecated_contract_class::ContractClass, ErrorObjectOwned> {
    Ok(starknet_api::deprecated_contract_class::ContractClass {
        abi: value.abi,
        program: decompress_program(&value.compressed_program)?,
        entry_points_by_type: value.entry_points_by_type,
    })
}

impl From<InvokeTransaction> for starknet_api::transaction::InvokeTransaction {
    fn from(value: InvokeTransaction) -> Self {
        match value {
            InvokeTransaction::Version0(InvokeTransactionV0 {
                max_fee,
                version: _,
                signature,
                contract_address,
                entry_point_selector,
                calldata,
            }) => Self::V0(starknet_api::transaction::InvokeTransactionV0 {
                max_fee,
                signature,
                contract_address,
                entry_point_selector,
                calldata,
            }),
            InvokeTransaction::Version1(InvokeTransactionV1 {
                max_fee,
                version: _,
                signature,
                nonce,
                sender_address,
                calldata,
            }) => Self::V1(starknet_api::transaction::InvokeTransactionV1 {
                max_fee,
                signature,
                nonce,
                sender_address,
                calldata,
            }),
        }
    }
}

impl TryFrom<ExecutionError> for JsonRpcError {
    type Error = ErrorObjectOwned;
    fn try_from(value: ExecutionError) -> Result<Self, Self::Error> {
        match value {
            ExecutionError::NotSynced { .. } => Ok(BLOCK_NOT_FOUND),
            ExecutionError::ContractNotFound { .. } => Ok(CONTRACT_NOT_FOUND),
            // All other execution errors are considered contract errors.
            _ => Ok(CONTRACT_ERROR),
        }
    }
}

pub(crate) fn decompress_program(
    base64_compressed_program: &String,
) -> Result<Program, ErrorObjectOwned> {
    base64::decode(base64_compressed_program).unwrap();
    let compressed_data =
        base64::decode(base64_compressed_program).map_err(internal_server_error)?;
    let mut decoder = GzDecoder::new(compressed_data.as_slice());
    let mut decompressed = Vec::new();
    decoder.read_to_end(&mut decompressed).map_err(internal_server_error)?;
    serde_json::from_reader(decompressed.as_slice()).map_err(internal_server_error)
}