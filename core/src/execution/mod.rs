use std::collections::{HashMap, HashSet};

use alloy::network::ReceiptResponse;
use alloy::primitives::{keccak256, Address, B256, U256};
use alloy::rlp::encode;
use alloy::rpc::types::{Filter, Log};
use eyre::Result;
use futures::future::join_all;
use revm::primitives::KECCAK_EMPTY;
use triehash_ethereum::ordered_trie_root;

use crate::network_spec::NetworkSpec;
use crate::types::{Block, BlockTag, Transactions};

use self::constants::MAX_SUPPORTED_LOGS_NUMBER;
use self::errors::ExecutionError;
use self::proof::{encode_account, verify_proof};
use self::rpc::ExecutionRpc;
use self::state::State;
use self::types::Account;

pub mod constants;
pub mod errors;
pub mod evm;
pub mod rpc;
pub mod state;
pub mod types;

mod proof;

#[derive(Clone)]
pub struct ExecutionClient<N: NetworkSpec, R: ExecutionRpc<N>> {
    pub rpc: R,
    state: State<N, R>,
}

impl<N: NetworkSpec, R: ExecutionRpc<N>> ExecutionClient<N, R> {
    pub fn new(rpc: &str, state: State<N, R>) -> Result<Self> {
        let rpc: R = ExecutionRpc::new(rpc)?;
        Ok(ExecutionClient::<N, R> { rpc, state })
    }

    pub async fn check_rpc(&self, chain_id: u64) -> Result<()> {
        if self.rpc.chain_id().await? != chain_id {
            Err(ExecutionError::IncorrectRpcNetwork().into())
        } else {
            Ok(())
        }
    }

    pub async fn get_account(
        &self,
        address: Address,
        slots: Option<&[B256]>,
        tag: BlockTag,
    ) -> Result<Account> {
        let slots = slots.unwrap_or(&[]);
        let block = self
            .state
            .get_block(tag)
            .await
            .ok_or(ExecutionError::BlockNotFound(tag))?;

        let proof = self
            .rpc
            .get_proof(address, slots, block.number.to())
            .await?;

        let account_path = keccak256(address).to_vec();
        let account_encoded = encode_account(&proof);

        let is_valid = verify_proof(
            &proof.account_proof,
            block.state_root.as_slice(),
            &account_path,
            &account_encoded,
        );

        if !is_valid {
            return Err(ExecutionError::InvalidAccountProof(address).into());
        }

        let mut slot_map = HashMap::new();

        for storage_proof in proof.storage_proof {
            let key = storage_proof.key.0;
            let key_hash = keccak256(key);
            let value = encode(storage_proof.value);

            let is_valid = verify_proof(
                &storage_proof.proof,
                proof.storage_hash.as_slice(),
                key_hash.as_slice(),
                &value,
            );

            if !is_valid {
                return Err(ExecutionError::InvalidStorageProof(address, key).into());
            }

            slot_map.insert(key, storage_proof.value);
        }

        let code = if proof.code_hash == KECCAK_EMPTY || proof.code_hash == B256::ZERO {
            Vec::new()
        } else {
            let code = self.rpc.get_code(address, block.number.to()).await?;
            let code_hash = keccak256(&code);

            if proof.code_hash != code_hash {
                return Err(
                    ExecutionError::CodeHashMismatch(address, code_hash, proof.code_hash).into(),
                );
            }

            code
        };

        Ok(Account {
            balance: proof.balance,
            nonce: proof.nonce,
            code,
            code_hash: proof.code_hash,
            storage_hash: proof.storage_hash,
            slots: slot_map,
        })
    }

    pub async fn send_raw_transaction(&self, bytes: &[u8]) -> Result<B256> {
        self.rpc.send_raw_transaction(bytes).await
    }

    pub async fn get_block(
        &self,
        tag: BlockTag,
        full_tx: bool,
    ) -> Result<Block<N::TransactionResponse>> {
        let mut block = self
            .state
            .get_block(tag)
            .await
            .ok_or(ExecutionError::BlockNotFound(tag))?;

        if !full_tx {
            block.transactions = Transactions::Hashes(block.transactions.hashes());
        }

        Ok(block)
    }

    pub async fn get_block_by_hash(
        &self,
        hash: B256,
        full_tx: bool,
    ) -> Result<Block<N::TransactionResponse>> {
        let mut block = self
            .state
            .get_block_by_hash(hash)
            .await
            .ok_or(eyre::eyre!("block not found"))?;

        if !full_tx {
            block.transactions = Transactions::Hashes(block.transactions.hashes());
        }

        Ok(block)
    }

    pub async fn get_transaction_by_block_hash_and_index(
        &self,
        block_hash: B256,
        index: u64,
    ) -> Option<N::TransactionResponse> {
        self.state
            .get_transaction_by_block_and_index(block_hash, index)
            .await
    }

    pub async fn get_transaction_receipt(
        &self,
        tx_hash: B256,
    ) -> Result<Option<N::ReceiptResponse>> {
        let receipt = self.rpc.get_transaction_receipt(tx_hash).await?;
        if receipt.is_none() {
            return Ok(None);
        }
        let receipt = receipt.unwrap();

        let block_number = receipt.block_number().unwrap();
        let tag = BlockTag::Number(block_number);

        let block = self.state.get_block(tag).await;
        let block = if let Some(block) = block {
            block
        } else {
            return Ok(None);
        };

        // Fetch all receipts in block, check root and inclusion
        let receipts = self
            .rpc
            .get_block_receipts(tag)
            .await?
            .ok_or(eyre::eyre!("missing block receipt"))?;

        let receipts_encoded: Vec<Vec<u8>> = receipts.iter().map(N::encode_receipt).collect();
        let expected_receipt_root = ordered_trie_root(receipts_encoded.clone());
        let expected_receipt_root = B256::from_slice(&expected_receipt_root.to_fixed_bytes());

        if expected_receipt_root != block.receipts_root
            // Note: Some RPC providers return different response in `eth_getTransactionReceipt` vs `eth_getBlockReceipts`
            // Primarily due to https://github.com/ethereum/execution-apis/issues/295 not finalized
            // Which means that the basic equality check in N::receipt_contains can be flaky
            // So as a fallback do equality check on encoded receipts as well
            || !(
                N::receipt_contains(&receipts, &receipt)
                || receipts_encoded.contains(&N::encode_receipt(&receipt))
            )
        {
            return Err(ExecutionError::ReceiptRootMismatch(tx_hash).into());
        }

        Ok(Some(receipt))
    }

    pub async fn get_block_receipts(
        &self,
        tag: BlockTag,
    ) -> Result<Option<Vec<N::ReceiptResponse>>> {
        let block = self.state.get_block(tag).await;
        let block = if let Some(block) = block {
            block
        } else {
            return Ok(None);
        };

        let tag = BlockTag::Number(block.number.to());

        let receipts = self
            .rpc
            .get_block_receipts(tag)
            .await?
            .ok_or(eyre::eyre!("block receipts not found"))?;

        let receipts_encoded: Vec<Vec<u8>> = receipts.iter().map(N::encode_receipt).collect();

        let expected_receipt_root = ordered_trie_root(receipts_encoded);
        let expected_receipt_root = B256::from_slice(&expected_receipt_root.to_fixed_bytes());

        if expected_receipt_root != block.receipts_root {
            return Err(ExecutionError::BlockReceiptsRootMismatch(tag).into());
        }

        Ok(Some(receipts))
    }

    pub async fn get_transaction(&self, hash: B256) -> Option<N::TransactionResponse> {
        self.state.get_transaction(hash).await
    }

    pub async fn get_logs(&self, filter: &Filter) -> Result<Vec<Log>> {
        let filter = filter.clone();

        // avoid fetching logs for a block helios hasn't seen yet
        let filter = if filter.get_to_block().is_none() && filter.get_block_hash().is_none() {
            let block = self.state.latest_block_number().await.unwrap();
            let filter = filter.to_block(block);
            if filter.get_from_block().is_none() {
                filter.from_block(block)
            } else {
                filter
            }
        } else {
            filter
        };

        let logs = self.rpc.get_logs(&filter).await?;
        if logs.len() > MAX_SUPPORTED_LOGS_NUMBER {
            return Err(
                ExecutionError::TooManyLogsToProve(logs.len(), MAX_SUPPORTED_LOGS_NUMBER).into(),
            );
        }

        self.verify_logs(&logs).await?;
        Ok(logs)
    }

    pub async fn get_filter_changes(&self, filter_id: U256) -> Result<Vec<Log>> {
        let logs = self.rpc.get_filter_changes(filter_id).await?;
        if logs.len() > MAX_SUPPORTED_LOGS_NUMBER {
            return Err(
                ExecutionError::TooManyLogsToProve(logs.len(), MAX_SUPPORTED_LOGS_NUMBER).into(),
            );
        }
        self.verify_logs(&logs).await?;
        Ok(logs)
    }

    pub async fn uninstall_filter(&self, filter_id: U256) -> Result<bool> {
        self.rpc.uninstall_filter(filter_id).await
    }

    pub async fn get_new_filter(&self, filter: &Filter) -> Result<U256> {
        let filter = filter.clone();

        // avoid submitting a filter for logs for a block helios hasn't seen yet
        let filter = if filter.get_to_block().is_none() && filter.get_block_hash().is_none() {
            let block = self.state.latest_block_number().await.unwrap();
            let filter = filter.to_block(block);
            if filter.get_from_block().is_none() {
                filter.from_block(block)
            } else {
                filter
            }
        } else {
            filter
        };
        self.rpc.get_new_filter(&filter).await
    }

    pub async fn get_new_block_filter(&self) -> Result<U256> {
        self.rpc.get_new_block_filter().await
    }

    pub async fn get_new_pending_transaction_filter(&self) -> Result<U256> {
        self.rpc.get_new_pending_transaction_filter().await
    }

    async fn verify_logs(&self, logs: &[Log]) -> Result<()> {
        // Collect all (unique) tx hashes
        let txs_hash = logs
            .iter()
            .map(|log| {
                log.transaction_hash
                    .ok_or(eyre::eyre!("tx hash not found in log"))
            })
            .collect::<Result<HashSet<_>, _>>()?;

        // Collect all (proven) tx receipts as a map of tx hash to receipt
        // TODO: use get_block_receipts instead to reduce the number of RPC calls?
        let receipts_fut = txs_hash.iter().map(|&tx_hash| async move {
            let receipt = self.get_transaction_receipt(tx_hash).await;
            receipt?.map(|r| (tx_hash, r)).ok_or(eyre::eyre!(
                ExecutionError::NoReceiptForTransaction(tx_hash)
            ))
        });
        let receipts = join_all(receipts_fut).await;
        let receipts: HashMap<_, _> = receipts.into_iter().collect::<Result<_, _>>()?;

        // Map tx hashes to encoded logs
        let receipts_logs_encoded: HashMap<_, _> = receipts
            .iter()
            .map(|(tx_hash, receipt)| {
                let encoded_logs = N::receipt_logs(&receipt)
                    .iter()
                    .map(|l| encode(&l.inner))
                    .collect::<Vec<_>>();
                (tx_hash, encoded_logs)
            })
            .collect();

        for log in logs.iter() {
            // Check if the receipt contains the desired log
            // Encoding logs for comparison
            let tx_hash = log.transaction_hash.unwrap();
            let log_encoded = encode(&log.inner);
            let receipt_logs_encoded = receipts_logs_encoded.get(&tx_hash).unwrap();

            if !receipt_logs_encoded.contains(&log_encoded) {
                return Err(ExecutionError::MissingLog(
                    tx_hash,
                    U256::from(log.log_index.unwrap()),
                )
                .into());
            }
        }
        Ok(())
    }
}
