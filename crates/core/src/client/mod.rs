pub mod client_api;
pub mod config;
pub mod constants;
pub mod errors;
pub mod helpers;

use std::str::FromStr;

use async_trait::async_trait;
use constants::selectors::BYTECODE;
use eyre::Result;
use futures::future::join_all;
use helpers::{
    decode_eth_call_return, ethers_block_id_to_starknet_block_id, raw_starknet_calldata,
    starknet_address_to_ethereum_address, vec_felt_to_bytes, FeltOrFeltArray,
};
// TODO: all reth_primitives::rpc types should be replaced when native reth Log is implemented
// https://github.com/paradigmxyz/reth/issues/1396#issuecomment-1440890689
use reth_primitives::{
    keccak256, Address, BlockId, BlockNumberOrTag, Bloom, Bytes, Bytes as RpcBytes, TransactionSigned, H160, H256,
    U128, U256, U64, U8,
};
use reth_rlp::Decodable;
use reth_rpc_types::{
    BlockTransactions, CallRequest, FeeHistory, Index, Log, RichBlock, SyncInfo, SyncStatus,
    Transaction as EtherTransaction, TransactionReceipt,
};
use starknet::core::types::{
    BlockId as StarknetBlockId, BlockTag, BroadcastedInvokeTransaction, BroadcastedInvokeTransactionV1, FieldElement,
    FunctionCall, InvokeTransactionReceipt, MaybePendingBlockWithTxs, MaybePendingTransactionReceipt, SyncStatusType,
    Transaction as TransactionType, TransactionReceipt as StarknetTransactionReceipt,
    TransactionStatus as StarknetTransactionStatus,
};
use starknet::providers::jsonrpc::{HttpTransport, JsonRpcClient};
use starknet::providers::Provider;
use url::Url;

use self::client_api::KakarotProvider;
use self::config::StarknetConfig;
use self::constants::gas::{BASE_FEE_PER_GAS, MAX_PRIORITY_FEE_PER_GAS};
use self::constants::selectors::{BALANCE_OF, COMPUTE_STARKNET_ADDRESS, GET_EVM_ADDRESS};
use self::constants::{MAX_FEE, STARKNET_NATIVE_TOKEN};
use self::errors::EthApiError;
use crate::client::constants::selectors::ETH_CALL;
use crate::models::balance::{TokenBalance, TokenBalances};
use crate::models::block::{BlockWithTxHashes, BlockWithTxs};
use crate::models::convertible::{ConvertibleStarknetBlock, ConvertibleStarknetTransaction};
use crate::models::felt::Felt252Wrapper;
use crate::models::transaction::{StarknetTransaction, StarknetTransactions};

pub struct KakarotClient<StarknetClient>
where
    StarknetClient: Provider,
{
    starknet_provider: StarknetClient,
    kakarot_address: FieldElement,
    proxy_account_class_hash: FieldElement,
}

impl KakarotClient<JsonRpcClient<HttpTransport>> {
    /// Create a new `KakarotClient`.
    ///
    /// # Arguments
    ///
    /// * `starknet_rpc(&str)` - `StarkNet` RPC
    ///
    /// # Errors
    ///
    /// `Err(EthApiError)` if the operation failed.
    pub fn new(starknet_config: StarknetConfig) -> Result<Self> {
        let StarknetConfig { starknet_rpc, kakarot_address, proxy_account_class_hash } = starknet_config;
        let url = Url::parse(&starknet_rpc)?;
        Ok(Self {
            starknet_provider: JsonRpcClient::new(HttpTransport::new(url)),
            kakarot_address,
            proxy_account_class_hash,
        })
    }

    /// Get the Ethereum address of a Starknet Kakarot smart-contract by calling `get_evm_address`
    /// on it. If the contract's `get_evm_address` errors, returns the Starknet address sliced
    /// to 20 bytes to conform with EVM addresses formats.
    ///
    /// ## Arguments
    ///
    /// * `starknet_address` - The Starknet address of the contract.
    /// * `starknet_block_id` - The block id to query the contract at.
    ///
    /// ## Returns
    ///
    /// * `eth_address` - The Ethereum address of the contract.
    pub async fn safe_get_evm_address(
        &self,
        starknet_address: &FieldElement,
        starknet_block_id: &StarknetBlockId,
    ) -> Address {
        self.get_evm_address(starknet_address, starknet_block_id)
            .await
            .unwrap_or_else(|_| starknet_address_to_ethereum_address(starknet_address))
    }
}

#[async_trait]
impl KakarotProvider for KakarotClient<JsonRpcClient<HttpTransport>> {
    fn kakarot_address(&self) -> FieldElement {
        self.kakarot_address
    }

    fn proxy_account_class_hash(&self) -> FieldElement {
        self.proxy_account_class_hash
    }

    fn starknet_provider(&self) -> &JsonRpcClient<HttpTransport> {
        &self.starknet_provider
    }

    /// Get the number of transactions in a block given a block id.
    /// The number of transactions in a block.
    ///
    /// ## Arguments
    ///
    ///
    ///
    /// ## Returns
    ///
    ///  * `block_number(u64)` - The block number.
    ///
    /// `Ok(ContractClass)` if the operation was successful.
    /// `Err(EthApiError)` if the operation failed.
    async fn block_number(&self) -> Result<U64, EthApiError> {
        let block_number = self.starknet_provider.block_number().await?;
        Ok(block_number.into())
    }

    /// Get the block given a block id.
    /// The block.
    /// ## Arguments
    /// * `block_id(StarknetBlockId)` - The block id.
    /// * `hydrated_tx(bool)` - Whether to hydrate the transactions.
    /// ## Returns
    /// `Ok(RichBlock)` if the operation was successful.
    /// `Err(EthApiError)` if the operation failed.
    async fn get_eth_block_from_starknet_block(
        &self,
        block_id: StarknetBlockId,
        hydrated_tx: bool,
    ) -> Result<RichBlock, EthApiError> {
        if hydrated_tx {
            let block = self.starknet_provider.get_block_with_txs(block_id).await?;
            let starknet_block = BlockWithTxs::new(block);
            starknet_block.to_eth_block(self).await
        } else {
            let block = self.starknet_provider.get_block_with_tx_hashes(block_id).await?;
            let starknet_block = BlockWithTxHashes::new(block);
            starknet_block.to_eth_block(self).await
        }
    }

    /// Get the number of transactions in a block given a block id.
    /// The number of transactions in a block.
    ///
    /// ## Arguments
    ///
    ///
    ///
    /// ## Returns
    ///
    ///  * `block_number(u64)` - The block number.
    ///
    /// `Ok(Bytes)` if the operation was successful.
    /// `Err(EthApiError)` if the operation failed.
    async fn get_code(
        &self,
        ethereum_address: Address,
        starknet_block_id: StarknetBlockId,
    ) -> Result<Bytes, EthApiError> {
        // Convert the hex-encoded string to a FieldElement
        let ethereum_address: Felt252Wrapper = ethereum_address.into();
        let ethereum_address = ethereum_address.into();

        // Prepare the calldata for the get_starknet_contract_address function call
        let tx_calldata_vec = vec![ethereum_address];
        let request = FunctionCall {
            contract_address: self.kakarot_address,
            entry_point_selector: COMPUTE_STARKNET_ADDRESS,
            calldata: tx_calldata_vec,
        };
        // Make the function call to get the Starknet contract address
        let starknet_contract_address = self.starknet_provider.call(request, starknet_block_id).await?;

        // shadow the variable to FielElement from a Vec<FieldElement>, for use in subsequent code
        let starknet_contract_address = match starknet_contract_address.get(0) {
            Some(x) if starknet_contract_address.len() == 1 => *x,
            _ => {
                return Err(EthApiError::OtherError(anyhow::anyhow!(
                    "Kakarot get_code: starknet_contract_address is empty"
                )));
            }
        };

        // Prepare the calldata for the bytecode function call
        let request = FunctionCall {
            contract_address: starknet_contract_address,
            entry_point_selector: BYTECODE,
            calldata: vec![],
        };
        // Make the function call to get the contract bytecode
        let contract_bytecode = self.starknet_provider.call(request, starknet_block_id).await?;
        // Convert the result of the function call to a vector of bytes
        let contract_bytecode_in_u8: Vec<u8> = contract_bytecode.into_iter().flat_map(|x| x.to_bytes_be()).collect();
        let bytes_result = Bytes::from(contract_bytecode_in_u8);
        Ok(bytes_result)
    }

    // Return the bytecode as a Result<Bytes>
    async fn call_view(
        &self,
        ethereum_address: Address,
        calldata: Bytes,
        starknet_block_id: StarknetBlockId,
    ) -> Result<Bytes, EthApiError> {
        let ethereum_address: Felt252Wrapper = ethereum_address.into();
        let ethereum_address = ethereum_address.into();

        let mut calldata_vec = calldata.clone().into_iter().map(FieldElement::from).collect::<Vec<FieldElement>>();

        let mut call_parameters =
            vec![ethereum_address, FieldElement::MAX, FieldElement::ZERO, FieldElement::ZERO, calldata.len().into()];

        call_parameters.append(&mut calldata_vec);

        let request = FunctionCall {
            contract_address: self.kakarot_address,
            entry_point_selector: ETH_CALL,
            calldata: call_parameters,
        };

        let call_result: Vec<FieldElement> = self.starknet_provider.call(request, starknet_block_id).await?;

        // Parse and decode Kakarot's call return data (temporary solution and not scalable - will
        // fail is Kakarot API changes)
        // Declare Vec of Result
        // TODO: Change to decode based on ABI or use starknet-rs future feature to decode return
        // params
        let segmented_result = decode_eth_call_return(&call_result)?;

        // Convert the result of the function call to a vector of bytes
        let return_data = segmented_result.first().ok_or_else(|| {
            EthApiError::OtherError(anyhow::anyhow!("Cannot parse and decode last argument of Kakarot call",))
        })?;
        if let FeltOrFeltArray::FeltArray(felt_array) = return_data {
            let result: Vec<u8> = felt_array.iter().filter_map(|f| u8::try_from(*f).ok()).collect();
            let bytes_result = Bytes::from(result);
            return Ok(bytes_result);
        }
        Err(EthApiError::OtherError(anyhow::anyhow!("Cannot parse and decode the return data of Kakarot call")))
    }

    /// Get the syncing status of the light client
    /// # Arguments
    /// # Returns
    ///  `Ok(SyncStatus)` if the operation was successful.
    ///  `Err(EthApiError)` if the operation failed.
    async fn syncing(&self) -> Result<SyncStatus, EthApiError> {
        let status = self.starknet_provider.syncing().await?;

        match status {
            SyncStatusType::NotSyncing => Ok(SyncStatus::None),

            SyncStatusType::Syncing(data) => {
                let starting_block: U256 = U256::from(data.starting_block_num);
                let current_block: U256 = U256::from(data.current_block_num);
                let highest_block: U256 = U256::from(data.highest_block_num);
                let warp_chunks_amount: Option<U256> = None;
                let warp_chunks_processed: Option<U256> = None;

                let status_info = SyncInfo {
                    starting_block,
                    current_block,
                    highest_block,
                    warp_chunks_amount,
                    warp_chunks_processed,
                };

                Ok(SyncStatus::Info(status_info))
            }
        }
    }

    /// Get the number of transactions in a block given a block number.
    /// The number of transactions in a block.
    ///
    /// # Arguments
    ///
    /// * `number(u64)` - The block number.
    ///
    /// # Returns
    ///
    ///  * `transaction_count(U64)` - The number of transactions.
    ///
    /// `Ok(U64)` if the operation was successful.
    /// `Err(EthApiError)` if the operation failed.
    async fn block_transaction_count_by_number(&self, number: BlockNumberOrTag) -> Result<U64, EthApiError> {
        let starknet_block_id = ethers_block_id_to_starknet_block_id(BlockId::Number(number))?;
        self.get_transaction_count_by_block(starknet_block_id).await
    }

    /// Get the number of transactions in a block given a block hash.
    /// The number of transactions in a block.
    /// # Arguments
    /// * `hash(H256)` - The block hash.
    /// # Returns
    ///
    ///  * `transaction_count(U64)` - The number of transactions.
    ///
    /// `Ok(U64)` if the operation was successful.
    /// `Err(EthApiError)` if the operation failed.
    async fn block_transaction_count_by_hash(&self, hash: H256) -> Result<U64, EthApiError> {
        let starknet_block_id = ethers_block_id_to_starknet_block_id(BlockId::Hash(hash.into()))?;
        self.get_transaction_count_by_block(starknet_block_id).await
    }

    async fn get_transaction_count_by_block(&self, starknet_block_id: StarknetBlockId) -> Result<U64, EthApiError> {
        let starknet_block = self.starknet_provider.get_block_with_txs(starknet_block_id).await?;

        let block_transactions = match starknet_block {
            MaybePendingBlockWithTxs::PendingBlock(pending_block_with_txs) => {
                self.filter_starknet_into_eth_txs(pending_block_with_txs.transactions.into(), None, None).await?
            }
            MaybePendingBlockWithTxs::Block(block_with_txs) => {
                let block_hash: Felt252Wrapper = block_with_txs.block_hash.into();
                let block_hash = Some(block_hash.into());
                let block_number: Felt252Wrapper = block_with_txs.block_number.into();
                let block_number = Some(block_number.into());
                self.filter_starknet_into_eth_txs(block_with_txs.transactions.into(), block_hash, block_number).await?
            }
        };
        let len = match block_transactions {
            BlockTransactions::Full(transactions) => transactions.len(),
            BlockTransactions::Hashes(_) => 0,
            BlockTransactions::Uncle => 0,
        };
        Ok(U64::from(len))
    }

    async fn transaction_by_block_id_and_index(
        &self,
        block_id: StarknetBlockId,
        tx_index: Index,
    ) -> Result<EtherTransaction, EthApiError> {
        let index: u64 = usize::from(tx_index) as u64;

        let starknet_tx: StarknetTransaction =
            self.starknet_provider.get_transaction_by_block_id_and_index(block_id, index).await?.into();

        let tx_hash: FieldElement = starknet_tx.transaction_hash()?.into();

        let tx_receipt = self.starknet_provider.get_transaction_receipt(tx_hash).await?;
        let (block_hash, block_num) = match tx_receipt {
            MaybePendingTransactionReceipt::Receipt(StarknetTransactionReceipt::Invoke(tr)) => {
                let block_hash: Felt252Wrapper = tr.block_hash.into();
                (Some(block_hash.into()), Some(U256::from(tr.block_number)))
            }
            _ => (None, None), // skip all transactions other than Invoke, covers the pending case
        };

        let eth_tx = starknet_tx.to_eth_transaction(self, block_hash, block_num, Some(U256::from(index))).await?;
        Ok(eth_tx)
    }

    /// Returns the Starknet address associated with a given Ethereum address.
    ///
    /// ## Arguments
    /// * `ethereum_address` - The Ethereum address to convert to a Starknet address.
    /// * `starknet_block_id` - The block ID to use for the Starknet contract call.
    async fn compute_starknet_address(
        &self,
        ethereum_address: Address,
        starknet_block_id: &StarknetBlockId,
    ) -> Result<FieldElement, EthApiError> {
        let ethereum_address: Felt252Wrapper = ethereum_address.into();
        let ethereum_address = ethereum_address.into();

        let request = FunctionCall {
            contract_address: self.kakarot_address,
            entry_point_selector: COMPUTE_STARKNET_ADDRESS,
            calldata: vec![ethereum_address],
        };

        let starknet_contract_address = self.starknet_provider.call(request, starknet_block_id).await?;

        let result = starknet_contract_address.first().ok_or_else(|| {
            EthApiError::OtherError(anyhow::anyhow!("Kakarot Core: Failed to get Starknet address from Kakarot"))
        })?;

        Ok(*result)
    }

    async fn transaction_by_hash(&self, hash: H256) -> Result<EtherTransaction, EthApiError> {
        let hash: Felt252Wrapper = hash.try_into()?;

        let transaction: StarknetTransaction =
            self.starknet_provider.get_transaction_by_hash::<FieldElement>(hash.clone().into()).await?.into();
        let hash: FieldElement = hash.into();
        let tx_receipt = self.starknet_provider.get_transaction_receipt(hash).await?;
        let (block_hash, block_num) = match tx_receipt {
            MaybePendingTransactionReceipt::Receipt(StarknetTransactionReceipt::Invoke(tr)) => {
                let block_hash: Felt252Wrapper = tr.block_hash.into();
                (Some(block_hash.into()), Some(U256::from(tr.block_number)))
            }
            _ => (None, None), // skip all transactions other than Invoke, covers the pending case
        };
        let eth_transaction = transaction.to_eth_transaction(self, block_hash, block_num, None).await?;
        Ok(eth_transaction)
    }

    async fn submit_starknet_transaction(&self, request: BroadcastedInvokeTransactionV1) -> Result<H256, EthApiError> {
        let transaction_result =
            self.starknet_provider.add_invoke_transaction(&BroadcastedInvokeTransaction::V1(request)).await?;

        Ok(H256::from(transaction_result.transaction_hash.to_bytes_be()))
    }

    /// Returns the receipt of a transaction by transaction hash.
    ///
    /// # Arguments
    ///
    /// * `hash(H256)` - The transaction hash.
    ///
    /// # Returns
    ///
    ///  * `transaction_receipt(TransactionReceipt)` - The transaction receipt.
    ///
    /// `Ok(Option<TransactionReceipt>)` if the operation was successful.
    /// `Err(EthApiError)` if the operation failed.
    async fn transaction_receipt(&self, hash: H256) -> Result<Option<TransactionReceipt>, EthApiError> {
        // TODO: Error when trying to transform 32 bytes hash to FieldElement
        let transaction_hash: Felt252Wrapper = hash.try_into()?;
        let starknet_tx_receipt =
            self.starknet_provider.get_transaction_receipt::<FieldElement>(transaction_hash.into()).await?;

        let starknet_block_id = StarknetBlockId::Tag(BlockTag::Latest);

        let res_receipt = match starknet_tx_receipt {
            MaybePendingTransactionReceipt::Receipt(receipt) => match receipt {
                StarknetTransactionReceipt::Invoke(InvokeTransactionReceipt {
                    transaction_hash,
                    status,
                    block_hash,
                    block_number,
                    events,
                    ..
                }) => {
                    let starknet_tx: StarknetTransaction =
                        self.starknet_provider.get_transaction_by_hash(transaction_hash).await?.into();

                    let starknet_sender_address: FieldElement = starknet_tx.sender_address()?.into();
                    let contract_address =
                        Some(self.safe_get_evm_address(&starknet_sender_address, &starknet_block_id).await);

                    let transaction_hash: Felt252Wrapper = transaction_hash.into();
                    let transaction_hash: Option<H256> = Some(transaction_hash.into());

                    let block_hash: Felt252Wrapper = block_hash.into();
                    let block_hash: Option<H256> = Some(block_hash.into());

                    let block_number: Felt252Wrapper = block_number.into();
                    let block_number: Option<U256> = Some(block_number.into());

                    let eth_tx = starknet_tx.to_eth_transaction(self, None, None, None).await?;
                    let from = eth_tx.from;
                    let to = eth_tx.to;

                    let status_code = match status {
                        StarknetTransactionStatus::Rejected | StarknetTransactionStatus::Pending => Some(U64::from(0)),
                        StarknetTransactionStatus::AcceptedOnL1 | StarknetTransactionStatus::AcceptedOnL2 => {
                            Some(U64::from(1))
                        }
                    };

                    // Handle events -- Will error if the event is not a Kakarot event
                    let mut logs = Vec::new();

                    // Cannot use `map` because of the `await` call.
                    for event in events {
                        let contract_address = self.safe_get_evm_address(&event.from_address, &starknet_block_id).await;

                        // event "keys" in cairo are event "topics" in solidity
                        // they're returned as list where consecutive values are
                        // low, high, low, high, etc. of the Uint256 Cairo representation
                        // of the bytes32 topics. This recomputes the original topic
                        let topics = (0..event.keys.len())
                            .step_by(2)
                            .map(|i| {
                                let next_key = *event.keys.get(i + 1).unwrap_or(&FieldElement::ZERO);

                                // Can unwrap here as we know 2^128 is a valid FieldElement
                                let two_pow_16: FieldElement =
                                    FieldElement::from_hex_be("0x100000000000000000000000000000000").unwrap();

                                // TODO: May wrap around prime field - Investigate edge cases
                                let felt_shifted_next_key = next_key * two_pow_16;
                                event.keys[i] + felt_shifted_next_key
                            })
                            .map(|topic| H256::from(&topic.to_bytes_be()))
                            .collect::<Vec<_>>();

                        let data = vec_felt_to_bytes(event.data);

                        let log = Log {
                            // TODO: fetch correct address from Kakarot.
                            // Contract Address is the account contract's address (EOA or KakarotAA)
                            address: H160::from_slice(&contract_address.0),
                            topics,
                            data: RpcBytes::from(data.0),
                            block_hash: None,
                            block_number: None,
                            transaction_hash: None,
                            transaction_index: None,
                            log_index: None,
                            removed: false,
                        };

                        logs.push(log);
                    }

                    TransactionReceipt {
                        transaction_hash,
                        transaction_index: None,
                        block_hash,
                        block_number,
                        from,
                        to,
                        cumulative_gas_used: U256::from(1_000_000), // TODO: Fetch real data
                        gas_used: Some(U256::from(500_000)),
                        contract_address,
                        logs,
                        state_root: None,             // TODO: Fetch real data
                        logs_bloom: Bloom::default(), // TODO: Fetch real data
                        status_code,
                        effective_gas_price: U128::from(1_000_000), // TODO: Fetch real data
                        transaction_type: U8::from(0),              // TODO: Fetch real data
                    }
                }
                // L1Handler, Declare, Deploy and DeployAccount transactions unsupported for now in
                // Kakarot
                _ => return Ok(None),
            },
            MaybePendingTransactionReceipt::PendingReceipt(_) => {
                return Ok(None);
            }
        };

        Ok(Some(res_receipt))
    }

    async fn get_evm_address(
        &self,
        starknet_address: &FieldElement,
        starknet_block_id: &StarknetBlockId,
    ) -> Result<Address, EthApiError> {
        let request = FunctionCall {
            contract_address: *starknet_address,
            entry_point_selector: GET_EVM_ADDRESS,
            calldata: vec![],
        };

        let evm_address_felt = self.starknet_provider.call(request, starknet_block_id).await?;
        let evm_address = evm_address_felt
            .first()
            .ok_or_else(|| {
                EthApiError::OtherError(anyhow::anyhow!(
                    "Kakarot Core: Failed to get EVM address from smart contract on Kakarot"
                ))
            })?
            .to_bytes_be();

        // Workaround as .get(12..32) does not dynamically size the slice
        let slice: &[u8] = evm_address.get(12..32).ok_or_else(|| {
            EthApiError::OtherError(anyhow::anyhow!(
                "Kakarot Core: Failed to cast EVM address from 32 bytes to 20 bytes EVM format"
            ))
        })?;
        let mut tmp_slice = [0u8; 20];
        tmp_slice.copy_from_slice(slice);
        let evm_address_sliced = &tmp_slice;

        Ok(Address::from(evm_address_sliced))
    }

    /// Get the nonce for a given ethereum address
    /// Convert Ethereum address to Starknet address
    /// Return the nonce, by calling `get_nonce` for the addresss via starknet rpc
    /// * `ethereum_address` - The EVM address to get the nonce of
    /// * `block_id` - The block to get the nonce at
    ///
    /// ### Returns
    /// * `Result<U256, EthApiError>` - The nonce of the EVM address
    async fn nonce(&self, ethereum_address: Address, block_id: StarknetBlockId) -> Result<U256, EthApiError> {
        let starknet_address = self.compute_starknet_address(ethereum_address, &block_id).await?;

        let nonce: Felt252Wrapper = self.starknet_provider.get_nonce(block_id, starknet_address).await?.into();

        Ok(nonce.into())
    }

    /// Get the balance in Starknet's native token of a specific EVM address.
    /// Reproduces the principle of Kakarot native coin by using Starknet's native ERC20 token
    /// (gas-utility token) ### Arguments
    /// * `ethereum_address` - The EVM address to get the balance of
    /// * `block_id` - The block to get the balance at
    ///
    /// ### Returns
    /// * `Result<U256, EthApiError>` - The balance of the EVM address in Starknet's native token
    async fn balance(&self, ethereum_address: Address, block_id: StarknetBlockId) -> Result<U256, EthApiError> {
        let starknet_address = self.compute_starknet_address(ethereum_address, &block_id).await?;

        let request = FunctionCall {
            // This FieldElement::from_hex_be cannot fail as the value is a constant
            contract_address: FieldElement::from_hex_be(STARKNET_NATIVE_TOKEN).unwrap(),
            entry_point_selector: BALANCE_OF,
            calldata: vec![starknet_address],
        };

        let balance_felt = self.starknet_provider.call(request, block_id).await?;

        let balance = balance_felt
            .first()
            .ok_or_else(|| {
                EthApiError::OtherError(anyhow::anyhow!("Kakarot Core: Failed to get native token balance"))
            })?
            .to_bytes_be();

        let balance = U256::from_be_bytes(balance);

        Ok(balance)
    }

    /// Returns token balances for a specific address given a list of contracts.
    ///
    /// # Arguments
    ///
    /// * `address(Address)` - specific address
    /// * `contract_addresses(Vec<Address>)` - List of contract addresses
    ///
    /// # Returns
    ///
    ///  * `token_balances(TokenBalances)` - Token balances
    ///
    /// `Ok(<TokenBalances>)` if the operation was successful.
    /// `Err(EthApiError)` if the operation failed.
    async fn token_balances(
        &self,
        address: Address,
        contract_addresses: Vec<Address>,
    ) -> Result<TokenBalances, EthApiError> {
        let entrypoint: Felt252Wrapper = keccak256("balanceOf(address)").try_into()?;
        let entrypoint: FieldElement = entrypoint.into();
        let felt_address = FieldElement::from_str(&address.to_string()).map_err(|e| {
            EthApiError::OtherError(anyhow::anyhow!("Failed to convert address to FieldElement: {}", e))
        })?;
        let handles = contract_addresses.into_iter().map(|token_address| {
            let calldata = vec![entrypoint, felt_address];

            self.call_view(
                token_address,
                Bytes::from(vec_felt_to_bytes(calldata).0),
                StarknetBlockId::Tag(BlockTag::Latest),
            )
        });
        let token_balances = join_all(handles)
            .await
            .into_iter()
            .map(|token_address| match token_address {
                Ok(call) => {
                    let hex_balance = U256::from_str_radix(&call.to_string(), 16)
                        .map_err(|e| {
                            EthApiError::OtherError(anyhow::anyhow!("Failed to convert token balance to U256: {}", e))
                        })
                        .unwrap();
                    TokenBalance { contract_address: address, token_balance: Some(hex_balance), error: None }
                }
                Err(e) => TokenBalance {
                    contract_address: address,
                    token_balance: None,
                    error: Some(format!("kakarot_getTokenBalances Error: {e}")),
                },
            })
            .collect();

        Ok(TokenBalances { address, token_balances })
    }

    async fn filter_starknet_into_eth_txs(
        &self,
        initial_transactions: StarknetTransactions,
        block_hash: Option<H256>,
        block_number: Option<U256>,
    ) -> Result<BlockTransactions, EthApiError> {
        let handles = Into::<Vec<TransactionType>>::into(initial_transactions).into_iter().map(|tx| async move {
            let tx = Into::<StarknetTransaction>::into(tx);
            tx.to_eth_transaction(self, block_hash, block_number, None).await
        });
        let transactions_vec = join_all(handles).await.into_iter().filter_map(|transaction| transaction.ok()).collect();
        Ok(BlockTransactions::Full(transactions_vec))
    }

    async fn send_transaction(&self, bytes: Bytes) -> Result<H256, EthApiError> {
        let mut data = bytes.as_ref();

        if data.is_empty() {
            return Err(EthApiError::OtherError(anyhow::anyhow!(
                "Kakarot send_transaction: Transaction bytes are empty"
            )));
        };

        let transaction = TransactionSigned::decode(&mut data).map_err(|_| {
            EthApiError::OtherError(anyhow::anyhow!("Kakarot send_transaction: transaction bytes failed to be decoded"))
        })?;

        let evm_address = transaction.recover_signer().ok_or_else(|| {
            EthApiError::OtherError(anyhow::anyhow!("Kakarot send_transaction: signature ecrecover failed"))
        })?;

        let starknet_block_id = StarknetBlockId::Tag(BlockTag::Latest);

        let starknet_address = self.compute_starknet_address(evm_address, &starknet_block_id).await?;

        let nonce = FieldElement::from(transaction.nonce());

        let calldata = raw_starknet_calldata(self.kakarot_address, bytes);

        // Get estimated_fee from Starknet
        let max_fee = *MAX_FEE;

        let signature = vec![];

        let request =
            BroadcastedInvokeTransactionV1 { max_fee, signature, nonce, sender_address: starknet_address, calldata };

        let starknet_transaction_hash = self.submit_starknet_transaction(request).await?;

        Ok(starknet_transaction_hash)
    }

    /// Returns the fixed base_fee_per_gas of Kakarot
    /// Since Starknet works on a FCFS basis (FIFO queue), it is not possible to tip miners to
    /// incentivize faster transaction inclusion
    /// As a result, in Kakarot, gas_price := base_fee_per_gas
    ///
    /// Computes the Starknet gas_price using starknet_estimateFee RPC method
    fn base_fee_per_gas(&self) -> U256 {
        U256::from(BASE_FEE_PER_GAS)
    }

    fn max_priority_fee_per_gas(&self) -> U128 {
        MAX_PRIORITY_FEE_PER_GAS
    }

    async fn fee_history(
        &self,
        _block_count: U256,
        _newest_block: BlockNumberOrTag,
        _reward_percentiles: Option<Vec<f64>>,
    ) -> Result<FeeHistory, EthApiError> {
        let block_count_usize = usize::from_str_radix(&_block_count.to_string(), 16).unwrap_or(1);

        let base_fee = self.base_fee_per_gas();

        let base_fee_per_gas: Vec<U256> = vec![base_fee; block_count_usize + 1];
        let newest_block = match _newest_block {
            BlockNumberOrTag::Number(n) => n,
            // TODO: Add Genesis block number
            BlockNumberOrTag::Earliest => 1_u64,
            _ => self.block_number().await?.as_u64(),
        };

        let gas_used_ratio: Vec<f64> = vec![0.9; block_count_usize];
        let oldest_block: U256 = U256::from(newest_block) - _block_count;

        Ok(FeeHistory { base_fee_per_gas, gas_used_ratio, oldest_block, reward: None })
    }

    async fn estimate_gas(
        &self,
        _call_request: CallRequest,
        _block_number: Option<BlockId>,
    ) -> Result<U256, EthApiError> {
        todo!();
    }
}
