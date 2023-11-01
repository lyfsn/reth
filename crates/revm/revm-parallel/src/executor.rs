//! Implementation of parallel executor.

use crate::{
    queue::{BlockQueue, BlockQueueStore, TransactionBatch},
    shared::{LockedSharedState, SharedState},
};
use futures::{stream::FuturesOrdered, Future, FutureExt, StreamExt};
use reth_interfaces::{
    executor::{BlockExecutionError, BlockValidationError},
    RethError, RethResult,
};
use reth_primitives::{
    revm::{
        compat::into_reth_log,
        env::{fill_cfg_and_block_env, fill_tx_env},
    },
    Address, Block, BlockNumber, ChainSpec, Hardfork, PruneModes, Receipt, Receipts,
    TransactionSigned, B256, U256,
};
use reth_provider::{
    AsyncBlockExecutor, BlockExecutorStats, BundleStateWithReceipts, PrunableAsyncBlockExecutor,
};
use reth_revm_executor::{
    eth_dao_fork::{DAO_HARDFORK_BENEFICIARY, DAO_HARDKFORK_ACCOUNTS},
    processor::verify_receipt,
    state_change::{execute_beacon_root_contract_call, post_block_balance_increments},
    ExecutionData,
};
use revm::{
    db::WrapDatabaseRef,
    primitives::{EVMResult, Env, ExecutionResult, ResultAndState},
    DatabaseRef, EVM,
};
use std::{
    pin::Pin,
    sync::{Arc, RwLockWriteGuard},
    task::{Context, Poll},
};
use tokio::sync::oneshot::{self, error::RecvError};

/// Database boxed with a lifetime and Send.
pub type DatabaseRefBox<'a, E> = Box<dyn DatabaseRef<Error = E> + Send + Sync + 'a>;

/// TODO: add docs
#[allow(missing_debug_implementations)]
pub struct ParallelExecutor<'a> {
    /// Store for transaction execution order.
    store: Arc<BlockQueueStore>,
    /// Execution data.
    data: ExecutionData,
    /// EVM state database.
    state: Arc<LockedSharedState<DatabaseRefBox<'a, RethError>>>,
    /// Thread pool for spawning transaction execution onto.
    pool: rayon::ThreadPool,
}

impl<'a> ParallelExecutor<'a> {
    /// Create new parallel executor.
    pub fn new(
        chain_spec: Arc<ChainSpec>,
        store: Arc<BlockQueueStore>,
        database: DatabaseRefBox<'a, RethError>,
        num_threads: Option<usize>,
    ) -> RethResult<Self> {
        Ok(Self {
            store,
            data: ExecutionData::new(chain_spec),
            state: Arc::new(LockedSharedState::new(SharedState::new(database))),
            pool: rayon::ThreadPoolBuilder::new()
                .num_threads(num_threads.unwrap_or_else(num_cpus::get))
                .build()
                .map_err(|error| {
                    RethError::Custom(format!("thread pool builder error: {error}"))
                })?,
        })
    }

    /// Return cloned pointer to the shared state.
    pub fn state(&self) -> Arc<LockedSharedState<DatabaseRefBox<'a, RethError>>> {
        Arc::clone(&self.state)
    }

    /// Return mutable reference to state.
    pub fn state_mut(&self) -> RwLockWriteGuard<'_, SharedState<DatabaseRefBox<'a, RethError>>> {
        self.state.write().unwrap()
    }

    /// Execute a batch of transactions in parallel.
    pub async fn execute_batch(
        &mut self,
        env: &Env,
        batch: &TransactionBatch,
        transactions: &[TransactionSigned],
        senders: &[Address],
    ) -> Result<Vec<(usize, ExecutionResult)>, BlockExecutionError> {
        let mut fut_batch = FuturesOrdered::default();
        for tx_idx in batch.iter() {
            let tx_idx = *tx_idx as usize;
            let transaction = transactions.get(tx_idx).unwrap(); // TODO:
            let sender = senders.get(tx_idx).unwrap(); // TODO:
            let mut env = env.clone();
            fill_tx_env(&mut env.tx, transaction, *sender);

            let (tx, rx) = oneshot::channel();
            self.pool.scope(|scope| {
                let state = self.state.clone();
                scope.spawn(move |_scope| {
                    let mut evm = EVM::with_env(env);
                    evm.database(state);
                    let _result = tx.send(evm.transact_ref());
                });
            });
            fut_batch.push_back(TransactionExecutionFut::new(tx_idx, transaction.hash, rx));
        }

        let mut results = Vec::with_capacity(batch.len());
        let mut states = Vec::with_capacity(batch.len());
        while let Some((tx_idx, hash, result)) = fut_batch.next().await {
            let ResultAndState { state, result } = result.unwrap().map_err(|e| {
                BlockExecutionError::Validation(BlockValidationError::EVM { hash, error: e.into() })
            })?;
            results.push((tx_idx, result));
            states.push((tx_idx, state));
        }
        self.state_mut().commit(states);

        Ok(results)
    }

    /// Apply post execution state changes, including block rewards, withdrawals, and irregular DAO
    /// hardfork state change.
    pub fn apply_post_execution_state_change(
        &mut self,
        block: &Block,
        total_difficulty: U256,
    ) -> Result<(), BlockExecutionError> {
        let mut balance_increments = post_block_balance_increments(
            &self.data.chain_spec,
            block.number,
            block.difficulty,
            block.beneficiary,
            block.timestamp,
            total_difficulty,
            &block.ommers,
            block.withdrawals.as_deref(),
        );

        // Irregular state change at Ethereum DAO hardfork
        if self.data.chain_spec.fork(Hardfork::Dao).transitions_at_block(block.number) {
            // drain balances from hardcoded addresses.
            let drained_balance: u128 = self
                .state_mut()
                .drain_balances(DAO_HARDKFORK_ACCOUNTS)
                .map_err(|_| BlockValidationError::IncrementBalanceFailed)?
                .into_iter()
                .sum();

            // return balance to DAO beneficiary.
            *balance_increments.entry(DAO_HARDFORK_BENEFICIARY).or_default() += drained_balance;
        }
        // increment balances
        self.state_mut()
            .increment_balances(balance_increments.into_iter().map(|(k, v)| (k, v)))
            .map_err(|_| BlockValidationError::IncrementBalanceFailed)?;

        Ok(())
    }

    /// Inner block execution.
    pub async fn execute_inner(
        &mut self,
        block: &Block,
        total_difficulty: U256,
        senders: Option<Vec<Address>>,
    ) -> Result<Vec<Receipt>, BlockExecutionError> {
        // Set state clear flag.
        let state_clear_enabled = self.data.state_clear_enabled(block.number);
        self.state_mut().set_state_clear_flag(state_clear_enabled);

        let mut env = Env::default();
        fill_cfg_and_block_env(
            &mut env.cfg,
            &mut env.block,
            &self.data.chain_spec,
            &block.header,
            total_difficulty,
        );

        // Applies the pre-block call to the EIP-4788 beacon block root contract.
        let mut evm = EVM::with_env(env.clone());
        evm.database(WrapDatabaseRef(&self.state));
        if let Some(state) = execute_beacon_root_contract_call(
            &self.data.chain_spec,
            block.timestamp,
            block.number,
            block.parent_beacon_block_root,
            &mut evm,
        )? {
            self.state_mut().commit(Vec::from([(0, state)]));
        }

        // perf: do not execute empty blocks
        if block.body.is_empty() {
            return Ok(Vec::new())
        }

        let mut results = Vec::with_capacity(block.body.len());
        let block_queue = self.store.get_queue(block.number).cloned().unwrap_or_else(|| {
            BlockQueue::from((0..block.body.len() as u32).map(|idx| Vec::from([idx])))
        });
        for batch in block_queue.iter() {
            results.extend(
                self.execute_batch(
                    &env,
                    batch,
                    &block.body,
                    senders.as_ref().unwrap(), /* TODO: */
                )
                .await?,
            );
        }
        results.sort_unstable_by_key(|(idx, _)| *idx);

        let mut cumulative_gas_used = 0;
        let mut receipts = Vec::with_capacity(block.body.len());
        for (transaction, (_, result)) in block.body.iter().zip(results) {
            cumulative_gas_used += result.gas_used();
            receipts.push(Receipt {
                tx_type: transaction.tx_type(),
                // Success flag was added in `EIP-658: Embedding transaction status code in
                // receipts`.
                success: result.is_success(),
                cumulative_gas_used,
                // convert to reth log
                logs: result.into_logs().into_iter().map(into_reth_log).collect(),
            });
        }

        // Check if gas used matches the value set in header.
        if block.gas_used != cumulative_gas_used {
            let receipts = Receipts::from_block_receipt(receipts);
            return Err(BlockValidationError::BlockGasUsed {
                got: cumulative_gas_used,
                expected: block.gas_used,
                gas_spent_by_tx: receipts.gas_spent_by_tx()?,
            }
            .into())
        }

        self.apply_post_execution_state_change(block, total_difficulty)?;

        let retention = self.data.retention_for_block(block.number);
        self.state_mut().merge_transitions(retention);

        if self.data.first_block.is_none() {
            self.data.first_block = Some(block.number);
        }

        Ok(receipts)
    }

    /// Saves receipts to the executor.
    pub fn save_receipts(&mut self, receipts: Vec<Receipt>) -> Result<(), BlockExecutionError> {
        let mut receipts = receipts.into_iter().map(Option::Some).collect();
        // Prune receipts if necessary.
        self.data.prune_receipts(&mut receipts)?;
        // Save receipts.
        self.data.receipts.push(receipts);
        Ok(())
    }
}

#[async_trait::async_trait]
impl AsyncBlockExecutor for ParallelExecutor<'_> {
    /// Execute block in parallel.
    async fn execute(
        &mut self,
        block: &Block,
        total_difficulty: U256,
        senders: Option<Vec<Address>>,
    ) -> Result<(), BlockExecutionError> {
        let receipts = self.execute_inner(block, total_difficulty, senders).await?;
        self.save_receipts(receipts)
    }

    /// Execute block in parallel and verify receipts.
    async fn execute_and_verify_receipt(
        &mut self,
        block: &Block,
        total_difficulty: U256,
        senders: Option<Vec<Address>>,
    ) -> Result<(), BlockExecutionError> {
        // execute block
        let receipts = self.execute_inner(block, total_difficulty, senders).await?;

        // TODO Before Byzantium, receipts contained state root that would mean that expensive
        // operation as hashing that is needed for state root got calculated in every
        // transaction This was replaced with is_success flag.
        // See more about EIP here: https://eips.ethereum.org/EIPS/eip-658
        if self.data.chain_spec.fork(Hardfork::Byzantium).active_at_block(block.header.number) {
            if let Err(error) =
                verify_receipt(block.header.receipts_root, block.header.logs_bloom, receipts.iter())
            {
                tracing::debug!(target: "evm::parallels", ?error, ?receipts, "receipts verification failed");
                return Err(error)
            };
        }

        self.save_receipts(receipts)
    }

    /// Return the bundle state.
    fn take_output_state(&mut self) -> BundleStateWithReceipts {
        let bundle_state = self.state_mut().take_bundle();
        let receipts = std::mem::take(&mut self.data.receipts);
        BundleStateWithReceipts::new(
            bundle_state,
            receipts,
            self.data.first_block.unwrap_or_default(),
        )
    }

    fn stats(&self) -> BlockExecutorStats {
        // TODO:
        BlockExecutorStats::default()
    }

    fn size_hint(&self) -> Option<usize> {
        Some(self.state.read().unwrap().bundle_size_hint())
    }
}

impl PrunableAsyncBlockExecutor for ParallelExecutor<'_> {
    fn set_tip(&mut self, tip: BlockNumber) {
        self.data.tip = Some(tip);
    }

    fn set_prune_modes(&mut self, prune_modes: PruneModes) {
        self.data.prune_modes = prune_modes;
    }
}

struct TransactionExecutionFut {
    tx_idx: usize,
    tx_hash: B256,
    rx: oneshot::Receiver<EVMResult<RethError>>,
}

impl TransactionExecutionFut {
    fn new(tx_idx: usize, tx_hash: B256, rx: oneshot::Receiver<EVMResult<RethError>>) -> Self {
        Self { tx_idx, tx_hash, rx }
    }
}

impl Future for TransactionExecutionFut {
    type Output = (usize, B256, Result<EVMResult<RethError>, RecvError>);

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.rx.poll_unpin(cx).map(|result| (this.tx_idx, this.tx_hash, result))
    }
}
