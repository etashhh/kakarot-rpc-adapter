use core::iter::once;

use async_trait::async_trait;
use num_bigint::BigUint;
use reth_primitives::{Address, Bytes, H256, U256};
use reth_rpc_types::Log;
use starknet::core::types::Event;

use super::felt::Felt252Wrapper;
use crate::client::client_api::KakarotProvider;
use crate::client::errors::EthApiError;
use crate::models::convertible::ConvertibleStarknetEvent;

pub struct StarknetEvent(Event);

impl StarknetEvent {
    pub fn new(sn_event: Event) -> Self {
        Self(sn_event)
    }
}

#[async_trait]
impl ConvertibleStarknetEvent for StarknetEvent {
    async fn to_eth_log(
        &self,
        client: &dyn KakarotProvider,
        block_hash: Option<H256>,
        block_number: Option<U256>,
        transaction_hash: Option<H256>,
        log_index: Option<U256>,
        transaction_index: Option<U256>,
    ) -> Result<Log, EthApiError> {
        // If event `from_address` does not equal kakarot address, return early
        if self.0.from_address != client.kakarot_address() {
            return Err(EthApiError::OtherError(anyhow::anyhow!("Kakarot Filter: Event is not part of Kakarot")));
        }

        // Derive the evm address from the last item in the `event.keys` vector and remove it
        let (evm_contract_address, keys) = self.0.keys.split_last().ok_or_else(|| {
            EthApiError::OtherError(anyhow::anyhow!("Kakarot Filter: Event is not an Kakarot evm event"))
        })?;

        let address: Address = {
            let felt_wrapper: Felt252Wrapper = (*evm_contract_address).into();
            felt_wrapper.into()
        };

        let topics: Vec<H256> = keys
            .chunks(2)
            .map(|chunk| {
                let low = BigUint::from_bytes_be(&chunk[0].to_bytes_be());
                let high = match chunk.get(1) {
                    Some(h) => BigUint::from_bytes_be(&h.to_bytes_be()),
                    None => {
                        return Err(anyhow::anyhow!("Not a convertible event: High value doesn't exist",));
                    }
                };
                let result = low + (BigUint::from(2u128).pow(128u32) * high);
                // Converts the result to bytes.
                let bytes = result.to_bytes_be();
                // If the length of bytes is less than 32, prepends it with zeros to make it 32 bytes long.
                let bytes = once(0u8).cycle().take(32 - bytes.len()).chain(bytes.into_iter()).collect::<Vec<_>>();
                Ok(H256::from_slice(&bytes))
            })
            .collect::<Result<_, _>>()?;

        let data: Bytes = self.0.data.iter()
            .flat_map(|felt| felt.to_bytes_be())
            .collect::<Vec<u8>>() // Collect into Vec<u8> first
            .into(); // Then convert into Bytes

        Ok(Log {
            address,
            topics,
            data,
            block_hash,
            block_number,
            transaction_hash,
            log_index,
            transaction_index,
            removed: false,
        })
    }
}
