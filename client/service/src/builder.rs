// This file is part of Substrate.

// Copyright (C) 2017-2020 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use crate::{
	NetworkStatus, NetworkState, error::Error, DEFAULT_PROTOCOL_ID, MallocSizeOfWasm,
	start_rpc_servers, build_network_future, TransactionPoolAdapter, TaskManager, SpawnTaskHandle,
	status_sinks, metrics::MetricsService,
	client::{light, Client, ClientConfig},
	config::{Configuration, KeystoreConfig, PrometheusConfig, OffchainWorkerConfig},
};
use sc_client_api::{
	light::RemoteBlockchain, ForkBlocks, BadBlocks, CloneableSpawn, UsageProvider,
	ExecutorProvider,
};
use sp_utils::mpsc::{tracing_unbounded, TracingUnboundedSender, TracingUnboundedReceiver};
use sc_chain_spec::get_extension;
use sp_consensus::{
	block_validation::{BlockAnnounceValidator, DefaultBlockAnnounceValidator, Chain},
	import_queue::ImportQueue,
};
use futures::{
	Future, FutureExt, StreamExt,
	future::ready,
};
use jsonrpc_pubsub::manager::SubscriptionManager;
use log::{info, warn, error};
use sc_network::config::{Role, FinalityProofProvider, OnDemand, BoxFinalityProofRequestBuilder};
use sc_network::NetworkService;
use parking_lot::{Mutex, RwLock};
use sp_runtime::generic::BlockId;
use sp_runtime::traits::{
	Block as BlockT, Header as HeaderT, SaturatedConversion, HashFor, Zero, BlockIdTo,
};
use sp_api::{ProvideRuntimeApi, CallApiAt};
use sc_executor::{NativeExecutor, NativeExecutionDispatch, RuntimeInfo};
use std::{collections::HashMap, sync::Arc, pin::Pin};
use wasm_timer::SystemTime;
use sc_telemetry::{telemetry, SUBSTRATE_INFO};
use sp_transaction_pool::MaintainedTransactionPool;
use prometheus_endpoint::Registry;
use sc_client_db::{Backend, DatabaseSettings};
use sp_core::traits::CodeExecutor;
use sp_runtime::BuildStorage;
use sc_client_api::{
	BlockBackend, BlockchainEvents,
	backend::StorageProvider,
	proof_provider::ProofProvider,
	execution_extensions::ExecutionExtensions
};
use sp_blockchain::{HeaderMetadata, HeaderBackend};
use crate::{ServiceComponents, TelemetryOnConnectSinks, RpcHandlers, NetworkStatusSinks};

use sc_keystore::Store as Keystore;
pub type KeystorePtr = Arc<RwLock<sc_keystore::Store>>;

/// A utility trait for building an RPC extension given a `DenyUnsafe` instance.
/// This is useful since at service definition time we don't know whether the
/// specific interface where the RPC extension will be exposed is safe or not.
/// This trait allows us to lazily build the RPC extension whenever we bind the
/// service to an interface.
pub trait RpcExtensionBuilder {
	/// The type of the RPC extension that will be built.
	type Output: sc_rpc::RpcExtension<sc_rpc::Metadata>;

	/// Returns an instance of the RPC extension for a particular `DenyUnsafe`
	/// value, e.g. the RPC extension might not expose some unsafe methods.
	fn build(&self, deny: sc_rpc::DenyUnsafe) -> Self::Output;
}

impl<F, R> RpcExtensionBuilder for F where
	F: Fn(sc_rpc::DenyUnsafe) -> R,
	R: sc_rpc::RpcExtension<sc_rpc::Metadata>,
{
	type Output = R;

	fn build(&self, deny: sc_rpc::DenyUnsafe) -> Self::Output {
		(*self)(deny)
	}
}

/// A utility struct for implementing an `RpcExtensionBuilder` given a cloneable
/// `RpcExtension`, the resulting builder will simply ignore the provided
/// `DenyUnsafe` instance and return a static `RpcExtension` instance.
pub struct NoopRpcExtensionBuilder<R>(pub R);

impl<R> RpcExtensionBuilder for NoopRpcExtensionBuilder<R> where
	R: Clone + sc_rpc::RpcExtension<sc_rpc::Metadata>,
{
	type Output = R;

	fn build(&self, _deny: sc_rpc::DenyUnsafe) -> Self::Output {
		self.0.clone()
	}
}

impl<R> From<R> for NoopRpcExtensionBuilder<R> where
	R: sc_rpc::RpcExtension<sc_rpc::Metadata>,
{
	fn from(e: R) -> NoopRpcExtensionBuilder<R> {
		NoopRpcExtensionBuilder(e)
	}
}


/// Full client type.
pub type TFullClient<TBl, TRtApi, TExecDisp> = Client<
	TFullBackend<TBl>,
	TFullCallExecutor<TBl, TExecDisp>,
	TBl,
	TRtApi,
>;

/// Full client backend type.
pub type TFullBackend<TBl> = sc_client_db::Backend<TBl>;

/// Full client call executor type.
pub type TFullCallExecutor<TBl, TExecDisp> = crate::client::LocalCallExecutor<
	sc_client_db::Backend<TBl>,
	NativeExecutor<TExecDisp>,
>;

/// Light client type.
pub type TLightClient<TBl, TRtApi, TExecDisp> = Client<
	TLightBackend<TBl>,
	TLightCallExecutor<TBl, TExecDisp>,
	TBl,
	TRtApi,
>;

/// Light client backend type.
pub type TLightBackend<TBl> = sc_light::Backend<
	sc_client_db::light::LightStorage<TBl>,
	HashFor<TBl>,
>;

/// Light call executor type.
pub type TLightCallExecutor<TBl, TExecDisp> = sc_light::GenesisCallExecutor<
	sc_light::Backend<
		sc_client_db::light::LightStorage<TBl>,
		HashFor<TBl>
	>,
	crate::client::LocalCallExecutor<
		sc_light::Backend<
			sc_client_db::light::LightStorage<TBl>,
			HashFor<TBl>
		>,
		NativeExecutor<TExecDisp>
	>,
>;

type TFullParts<TBl, TRtApi, TExecDisp> = (
	TFullClient<TBl, TRtApi, TExecDisp>,
	Arc<TFullBackend<TBl>>,
	KeystorePtr,
	TaskManager,
);

type TLightParts<TBl, TRtApi, TExecDisp> = (
	Arc<TLightClient<TBl, TRtApi, TExecDisp>>,
	Arc<TLightBackend<TBl>>,
	KeystorePtr,
	TaskManager,
	Arc<OnDemand<TBl>>,
);

/// Creates a new full client for the given config.
pub fn new_full_client<TBl, TRtApi, TExecDisp>(
	config: &Configuration,
) -> Result<TFullClient<TBl, TRtApi, TExecDisp>, Error> where
	TBl: BlockT,
	TExecDisp: NativeExecutionDispatch + 'static,
{
	new_full_parts(config).map(|parts| parts.0)
}

/// Create the initial parts of a full node.
pub fn new_full_parts<TBl, TRtApi, TExecDisp>(
	config: &Configuration,
) -> Result<TFullParts<TBl, TRtApi, TExecDisp>,	Error> where
	TBl: BlockT,
	TExecDisp: NativeExecutionDispatch + 'static,
{
	let keystore = match &config.keystore {
		KeystoreConfig::Path { path, password } => Keystore::open(
			path.clone(),
			password.clone()
		)?,
		KeystoreConfig::InMemory => Keystore::new_in_memory(),
	};

	let task_manager = {
		let registry = config.prometheus_config.as_ref().map(|cfg| &cfg.registry);
		TaskManager::new(config.task_executor.clone(), registry)?
	};

	let executor = NativeExecutor::<TExecDisp>::new(
		config.wasm_method,
		config.default_heap_pages,
		config.max_runtime_instances,
	);

	let chain_spec = &config.chain_spec;
	let fork_blocks = get_extension::<ForkBlocks<TBl>>(chain_spec.extensions())
		.cloned()
		.unwrap_or_default();

	let bad_blocks = get_extension::<BadBlocks<TBl>>(chain_spec.extensions())
		.cloned()
		.unwrap_or_default();

	let (client, backend) = {
		let db_config = sc_client_db::DatabaseSettings {
			state_cache_size: config.state_cache_size,
			state_cache_child_ratio:
			config.state_cache_child_ratio.map(|v| (v, 100)),
			pruning: config.pruning.clone(),
			source: config.database.clone(),
		};

		let extensions = sc_client_api::execution_extensions::ExecutionExtensions::new(
			config.execution_strategies.clone(),
			Some(keystore.clone()),
		);

		new_client(
			db_config,
			executor,
			chain_spec.as_storage_builder(),
			fork_blocks,
			bad_blocks,
			extensions,
			Box::new(task_manager.spawn_handle()),
			config.prometheus_config.as_ref().map(|config| config.registry.clone()),
			ClientConfig {
				offchain_worker_enabled : config.offchain_worker.enabled ,
				offchain_indexing_api: config.offchain_worker.indexing_enabled,
			},
		)?
	};

	Ok((client, backend, keystore, task_manager))
}

/// Create the initial parts of a light node.
pub fn new_light_parts<TBl, TRtApi, TExecDisp>(
	config: &Configuration
) -> Result<TLightParts<TBl, TRtApi, TExecDisp>, Error> where
	TBl: BlockT,
	TExecDisp: NativeExecutionDispatch + 'static,
{

	let task_manager = {
		let registry = config.prometheus_config.as_ref().map(|cfg| &cfg.registry);
		TaskManager::new(config.task_executor.clone(), registry)?
	};

	let keystore = match &config.keystore {
		KeystoreConfig::Path { path, password } => Keystore::open(
			path.clone(),
			password.clone()
		)?,
		KeystoreConfig::InMemory => Keystore::new_in_memory(),
	};

	let executor = NativeExecutor::<TExecDisp>::new(
		config.wasm_method,
		config.default_heap_pages,
		config.max_runtime_instances,
	);

	let db_storage = {
		let db_settings = sc_client_db::DatabaseSettings {
			state_cache_size: config.state_cache_size,
			state_cache_child_ratio:
				config.state_cache_child_ratio.map(|v| (v, 100)),
			pruning: config.pruning.clone(),
			source: config.database.clone(),
		};
		sc_client_db::light::LightStorage::new(db_settings)?
	};
	let light_blockchain = sc_light::new_light_blockchain(db_storage);
	let fetch_checker = Arc::new(
		sc_light::new_fetch_checker::<_, TBl, _>(
			light_blockchain.clone(),
			executor.clone(),
			Box::new(task_manager.spawn_handle()),
		),
	);
	let on_demand = Arc::new(sc_network::config::OnDemand::new(fetch_checker));
	let backend = sc_light::new_light_backend(light_blockchain);
	let client = Arc::new(light::new_light(
		backend.clone(),
		config.chain_spec.as_storage_builder(),
		executor,
		Box::new(task_manager.spawn_handle()),
		config.prometheus_config.as_ref().map(|config| config.registry.clone()),
	)?);

	Ok((client, backend, keystore, task_manager, on_demand))
}

/// Create an instance of db-backed client.
pub fn new_client<E, Block, RA>(
	settings: DatabaseSettings,
	executor: E,
	genesis_storage: &dyn BuildStorage,
	fork_blocks: ForkBlocks<Block>,
	bad_blocks: BadBlocks<Block>,
	execution_extensions: ExecutionExtensions<Block>,
	spawn_handle: Box<dyn CloneableSpawn>,
	prometheus_registry: Option<Registry>,
	config: ClientConfig,
) -> Result<(
	crate::client::Client<
		Backend<Block>,
		crate::client::LocalCallExecutor<Backend<Block>, E>,
		Block,
		RA,
	>,
	Arc<Backend<Block>>,
),
	sp_blockchain::Error,
>
	where
		Block: BlockT,
		E: CodeExecutor + RuntimeInfo,
{
	const CANONICALIZATION_DELAY: u64 = 4096;

	let backend = Arc::new(Backend::new(settings, CANONICALIZATION_DELAY)?);
	let executor = crate::client::LocalCallExecutor::new(backend.clone(), executor, spawn_handle, config.clone());
	Ok((
		crate::client::Client::new(
			backend.clone(),
			executor,
			genesis_storage,
			fork_blocks,
			bad_blocks,
			execution_extensions,
			prometheus_registry,
			config,
		)?,
		backend,
	))
}

/// Parameters to pass into `build`.
pub struct ServiceParams<TBl: BlockT, TCl, TImpQu, TExPool, TRpc, Backend> {
	/// The service configuration.
	pub config: Configuration,
	/// A shared client returned by `new_full_parts`/`new_light_parts`.
	pub client: Arc<TCl>,
	/// A shared backend returned by `new_full_parts`/`new_light_parts`.
	pub backend: Arc<Backend>,
	/// A task manager returned by `new_full_parts`/`new_light_parts`.
	pub task_manager: TaskManager,
	/// A shared keystore returned by `new_full_parts`/`new_light_parts`.
	pub keystore: KeystorePtr,
	/// An optional, shared data fetcher for light clients.
	pub on_demand: Option<Arc<OnDemand<TBl>>>,
	/// An import queue.
	pub import_queue: TImpQu,
	/// An optional finality proof request builder.
	pub finality_proof_request_builder: Option<BoxFinalityProofRequestBuilder<TBl>>,
	/// An optional, shared finality proof request provider.
	pub finality_proof_provider: Option<Arc<dyn FinalityProofProvider<TBl>>>,
	/// A shared transaction pool.
	pub transaction_pool: Arc<TExPool>,
	/// A RPC extension builder. Use `NoopRpcExtensionBuilder` if you just want to pass in the
	/// extensions directly.
	pub rpc_extensions_builder: Box<dyn RpcExtensionBuilder<Output = TRpc> + Send>,
	/// An optional, shared remote blockchain instance. Used for light clients.
	pub remote_blockchain: Option<Arc<dyn RemoteBlockchain<TBl>>>,
	/// A block annouce validator builder.
	pub block_announce_validator_builder: Option<Box<dyn FnOnce(Arc<TCl>) -> Box<dyn BlockAnnounceValidator<TBl> + Send> + Send>>,
}

pub trait BlockImportBuilder<
	Block: BlockT,
	RuntimeApi:
		sp_api::ConstructRuntimeApi<Block, TLightClient<Block, RuntimeApi, Executor>> +
		sp_api::ConstructRuntimeApi<Block, TFullClient<Block, RuntimeApi, Executor>>,
	Executor: NativeExecutionDispatch + 'static
> {
	type LightBlockImport:
		sp_consensus::BlockImport<Block, Error=sp_consensus::Error, Transaction=sp_api::TransactionFor<TLightClient<Block, RuntimeApi, Executor>, Block>>
		+ Clone;
	type FullBlockImport:
		sp_consensus::BlockImport<Block, Error=sp_consensus::Error, Transaction=sp_api::TransactionFor<TFullClient<Block, RuntimeApi, Executor>, Block>>
		+ Clone;
	type SelectChainBuilder: SelectChainBuilder<Block>;
	type Link;

	fn build_light(
		client: Arc<TLightClient<Block, RuntimeApi, Executor>>,
		backend: Arc<TLightBackend<Block>>,
		on_demand: Arc<OnDemand<Block>>,
	) -> Result<(Self::LightBlockImport, BoxFinalityProofRequestBuilder<Block>), Error>;

	fn build_full(
		client: Arc<TFullClient<Block, RuntimeApi, Executor>>,
		select_chain: <Self::SelectChainBuilder as SelectChainBuilder<Block>>::FullSelectChain
	) -> Result<(Self::FullBlockImport, Self::Link), Error>;
}

pub struct GrandpaBlockImportBuilder<SelectChainBuilder>(std::marker::PhantomData<SelectChainBuilder>);

impl<Block: BlockT, RuntimeApi, Executor, SelectChainBuilder> BlockImportBuilder<
	Block, RuntimeApi, Executor,
> for GrandpaBlockImportBuilder<SelectChainBuilder>
where
		Executor: NativeExecutionDispatch + 'static,
		RuntimeApi: Send + Sync,
		sp_api::NumberFor<Block>: grandpa::BlockNumberOps,
		RuntimeApi:
			sp_api::ConstructRuntimeApi<Block, TLightClient<Block, RuntimeApi, Executor>> +
			sp_api::ConstructRuntimeApi<Block, TFullClient<Block, RuntimeApi, Executor>> +
			'static,
		<RuntimeApi as sp_api::ConstructRuntimeApi<Block, TLightClient<Block, RuntimeApi, Executor>>>::RuntimeApi:
			sp_api::Core<Block> +
			sp_api::ApiExt<Block, StateBackend = <TLightBackend<Block> as sc_client_api::backend::Backend<Block>>::State> +
			sp_api::ApiErrorExt<Error = sp_blockchain::Error>,
		<RuntimeApi as sp_api::ConstructRuntimeApi<Block, TFullClient<Block, RuntimeApi, Executor>>>::RuntimeApi:
			sp_api::Core<Block> +
			sp_api::ApiExt<Block, StateBackend = <TFullBackend<Block> as sc_client_api::backend::Backend<Block>>::State> +
			sp_api::ApiErrorExt<Error = sp_blockchain::Error>,
		SelectChainBuilder: self::SelectChainBuilder<Block>,
{
	type LightBlockImport = grandpa::GrandpaLightBlockImport<
		TLightBackend<Block>, Block, TLightClient<Block, RuntimeApi, Executor>
	>;

	type FullBlockImport = grandpa::GrandpaBlockImport<
		TFullBackend<Block>, Block, TFullClient<Block, RuntimeApi, Executor>,
		<Self::SelectChainBuilder as self::SelectChainBuilder<Block>>::FullSelectChain,
	>;

	type SelectChainBuilder = SelectChainBuilder;

	type Link = grandpa::LinkHalf<
		Block, TFullClient<Block, RuntimeApi, Executor>,
		<Self::SelectChainBuilder as self::SelectChainBuilder<Block>>::FullSelectChain
	>;

	fn build_light(
		client: Arc<TLightClient<Block, RuntimeApi, Executor>>,
		backend: Arc<TLightBackend<Block>>,
		on_demand: Arc<OnDemand<Block>>,
	) -> Result<(Self::LightBlockImport, BoxFinalityProofRequestBuilder<Block>), Error> {
		let block_import = grandpa::light_block_import(
			client.clone(), backend, &(client as Arc<_>),
			Arc::new(on_demand.checker().clone()) as Arc<_>,
		)?;

		let fprb = block_import.create_finality_proof_request_builder();

		Ok((block_import, fprb))
	}

	fn build_full(
		client: Arc<TFullClient<Block, RuntimeApi, Executor>>,
		select_chain: <Self::SelectChainBuilder as self::SelectChainBuilder<Block>>::FullSelectChain
	) -> Result<(Self::FullBlockImport, Self::Link), Error> {
		grandpa::block_import(
			client.clone(), &(client as Arc<_>), select_chain,
		).map_err(|err| err.into())
	}
}

pub trait TransactionPoolBuilder<Builder: self::Builder> {
	type FullTransactionPool:
		sp_transaction_pool::TransactionPool<Block = BlockFor<Builder>> +
		sp_transaction_pool::MaintainedTransactionPool<Hash=<BlockFor<Builder> as BlockT>::Hash> +
		MallocSizeOfWasm + 'static;
	type LightTransactionPool:
		sp_transaction_pool::TransactionPool<Block = BlockFor<Builder>> +
		sp_transaction_pool::MaintainedTransactionPool<Hash=<BlockFor<Builder> as BlockT>::Hash> +
		MallocSizeOfWasm + 'static;

	fn build_light(
		config: &Configuration,
		client: Arc<LightClientFor<Builder>>,
		task_manager: &TaskManager,
		on_demand: Arc<OnDemand<BlockFor<Builder>>>,
	) -> Arc<Self::LightTransactionPool>;

	fn build_full(
		config: &Configuration,
		client: Arc<FullClientFor<Builder>>,
		task_manager: &TaskManager,
	) -> Arc<Self::FullTransactionPool>;
}

pub struct BasicPoolBuilder;

impl<Builder: self::Builder> TransactionPoolBuilder<Builder> for BasicPoolBuilder
	where
		RuntimeApiFor<Builder>: sp_api::ConstructRuntimeApi<BlockFor<Builder>, FullClientFor<Builder>> + Send + Sync + 'static,
		<RuntimeApiFor<Builder> as sp_api::ConstructRuntimeApi<BlockFor<Builder>, FullClientFor<Builder>>>::RuntimeApi: sp_transaction_pool::runtime_api::TaggedTransactionQueue<BlockFor<Builder>>,
		<<RuntimeApiFor<Builder> as sp_api::ConstructRuntimeApi<BlockFor<Builder>, FullClientFor<Builder>>>::RuntimeApi as sp_api::ApiErrorExt>::Error: Send + std::fmt::Display,
		ExecutorFor<Builder>: NativeExecutionDispatch + 'static,
{
	type FullTransactionPool = sc_transaction_pool::BasicPool<
		sc_transaction_pool::FullChainApi<FullClientFor<Builder>, BlockFor<Builder>>, BlockFor<Builder>,
	>;

	type LightTransactionPool = sc_transaction_pool::BasicPool<
		sc_transaction_pool::LightChainApi<
			LightClientFor<Builder>, OnDemand<BlockFor<Builder>>, BlockFor<Builder>
		>,
		BlockFor<Builder>,
	>;

	fn build_light(
		config: &Configuration,
		client: Arc<LightClientFor<Builder>>,
		task_manager: &TaskManager,
		on_demand: Arc<OnDemand<BlockFor<Builder>>>,
	) -> Arc<Self::LightTransactionPool> {
		let transaction_pool_api = Arc::new(sc_transaction_pool::LightChainApi::new(
			client, on_demand,
		));
		Arc::new(sc_transaction_pool::BasicPool::new_light(
			config.transaction_pool.clone(),
			transaction_pool_api,
			config.prometheus_registry(),
			task_manager.spawn_handle(),
		))
	}

	fn build_full(
		config: &Configuration,
		client: Arc<FullClientFor<Builder>>,
		task_manager: &TaskManager,
	) -> Arc<Self::FullTransactionPool> {
		let transaction_pool_api = sc_transaction_pool::FullChainApi::new(client.clone());

		sc_transaction_pool::BasicPool::new_full(
			config.transaction_pool.clone(),
			Arc::new(transaction_pool_api),
			config.prometheus_registry(),
			task_manager.spawn_handle(),
			client.clone(),
		)
	}
}

pub trait ImportQueueBuilder<
	Block: BlockT,
	RuntimeApi:
		sp_api::ConstructRuntimeApi<Block, TLightClient<Block, RuntimeApi, Executor>> +
		sp_api::ConstructRuntimeApi<Block, TFullClient<Block, RuntimeApi, Executor>>,
	Executor: NativeExecutionDispatch + 'static
> {
	type LightImportQueue: sp_consensus::import_queue::ImportQueue<Block> + 'static;
	type FullImportQueue: sp_consensus::import_queue::ImportQueue<Block> + 'static;
	type BlockImportBuilder: self::BlockImportBuilder<Block, RuntimeApi, Executor>;
	type Link: Clone;
	type ImportQueueBlockImport;

	fn build_light<SC: sp_consensus::SelectChain<Block> + 'static>(
		config: &Configuration,
		client: Arc<TLightClient<Block, RuntimeApi, Executor>>,
		inherent_data_providers: sp_inherents::InherentDataProviders,
		task_manager: &TaskManager,
		block_import: <Self::BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::LightBlockImport,
		select_chain: SC,
	) -> Result<Self::LightImportQueue, Error>;

	fn build_full<SC: sp_consensus::SelectChain<Block> + 'static>(
		config: &Configuration,
		client: Arc<TFullClient<Block, RuntimeApi, Executor>>,
		inherent_data_providers: sp_inherents::InherentDataProviders,
		task_manager: &TaskManager,
		block_import: <Self::BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::FullBlockImport,
		select_chain: SC,
	) -> Result<(Self::FullImportQueue, Self::Link, Self::ImportQueueBlockImport), Error>;
}

use sp_consensus_aura::sr25519::{AuthorityPair as AuraPair, AuthorityId as AuraPublic};

pub struct AuraImportQueueBuilder<BlockImportBuilder>(std::marker::PhantomData<BlockImportBuilder>);

impl<Block: BlockT, RuntimeApi, Executor, BlockImportBuilder> ImportQueueBuilder<Block, RuntimeApi, Executor> for AuraImportQueueBuilder<BlockImportBuilder>
	where
		RuntimeApi:
			sp_api::ConstructRuntimeApi<Block, TLightClient<Block, RuntimeApi, Executor>> +
			sp_api::ConstructRuntimeApi<Block, TFullClient<Block, RuntimeApi, Executor>> +
			Send + Sync + 'static,
		<RuntimeApi as sp_api::ConstructRuntimeApi<Block, TLightClient<Block, RuntimeApi, Executor>>>::RuntimeApi:
			sp_consensus_aura::AuraApi<Block, AuraPublic, Error=sp_blockchain::Error> +
			sp_block_builder::BlockBuilder<Block>,
		<RuntimeApi as sp_api::ConstructRuntimeApi<Block, TFullClient<Block, RuntimeApi, Executor>>>::RuntimeApi:
			sp_consensus_aura::AuraApi<Block, AuraPublic, Error=sp_blockchain::Error> +
			sp_block_builder::BlockBuilder<Block>,
		Executor: NativeExecutionDispatch + 'static,
		BlockImportBuilder: self::BlockImportBuilder<Block, RuntimeApi, Executor>,
		<BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::FullBlockImport:
			sp_consensus::JustificationImport<Block, Error=sp_consensus::Error> + Send + Sync + 'static,
		<BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::LightBlockImport:
			sp_consensus::FinalityProofImport<Block, Error=sp_consensus::Error> + Send + Sync + 'static,
{
	type LightImportQueue = sc_consensus_aura::AuraImportQueue<
		Block, sp_api::TransactionFor<TLightClient<Block, RuntimeApi, Executor>, Block>
	>;

	type FullImportQueue = sc_consensus_aura::AuraImportQueue<
		Block, sp_api::TransactionFor<TFullClient<Block, RuntimeApi, Executor>, Block>
	>;

	type Link = ();

	type ImportQueueBlockImport = sc_consensus_aura::AuraBlockImport<
		Block,
		TFullClient<Block, RuntimeApi, Executor>,
		<Self::BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::FullBlockImport,
		AuraPair
	>;

	type BlockImportBuilder = BlockImportBuilder;

	fn build_light<SC: sp_consensus::SelectChain<Block> + 'static>(
		config: &Configuration,
		client: Arc<TLightClient<Block, RuntimeApi, Executor>>,
		inherent_data_providers: sp_inherents::InherentDataProviders,
		task_manager: &TaskManager,
		block_import: <Self::BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::LightBlockImport,
		select_chain: SC,
	) -> Result<Self::LightImportQueue, Error> {
		sc_consensus_aura::import_queue::<_, _, _, AuraPair, _>(
			sc_consensus_aura::slot_duration(&*client)?,
			block_import.clone(),
			None,
			Some(Box::new(block_import)),
			client.clone(),
			inherent_data_providers,
			&task_manager.spawn_handle(),
			config.prometheus_registry(),
		).map_err(|err| err.into())
	}

	fn build_full<SC: sp_consensus::SelectChain<Block> + 'static>(
		config: &Configuration,
		client: Arc<TFullClient<Block, RuntimeApi, Executor>>,
		inherent_data_providers: sp_inherents::InherentDataProviders,
		task_manager: &TaskManager,
		block_import: <Self::BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::FullBlockImport,
		select_chain: SC,
	) -> Result<(Self::FullImportQueue, Self::Link, Self::ImportQueueBlockImport), Error> {
		let aura_block_import = sc_consensus_aura::AuraBlockImport::<_, _, _, AuraPair>::new(
			block_import.clone(), client.clone(),
		);

		let import_queue = sc_consensus_aura::import_queue::<_, _, TFullClient<Block, RuntimeApi, Executor>, AuraPair, _>(
			sc_consensus_aura::slot_duration(&*client)?,
			aura_block_import.clone(),
			Some(Box::new(block_import)),
			None,
			client.clone(),
			inherent_data_providers,
			&task_manager.spawn_handle(),
			config.prometheus_registry(),
		)?;

		Ok((import_queue, (), aura_block_import))
	}
}

pub struct BabeImportQueueBuilder<BlockImportBuilder>(std::marker::PhantomData<BlockImportBuilder>);

impl<Block: BlockT, RuntimeApi, Executor, BlockImportBuilder> ImportQueueBuilder<Block, RuntimeApi, Executor> for BabeImportQueueBuilder<BlockImportBuilder>
	where
		RuntimeApi:
			sp_api::ConstructRuntimeApi<Block, TLightClient<Block, RuntimeApi, Executor>> +
			sp_api::ConstructRuntimeApi<Block, TFullClient<Block, RuntimeApi, Executor>> +
			Send + Sync + 'static,
		<RuntimeApi as sp_api::ConstructRuntimeApi<Block, TLightClient<Block, RuntimeApi, Executor>>>::RuntimeApi:
			sp_consensus_babe::BabeApi<Block, Error=sp_blockchain::Error> +
			sp_block_builder::BlockBuilder<Block>,
		<RuntimeApi as sp_api::ConstructRuntimeApi<Block, TFullClient<Block, RuntimeApi, Executor>>>::RuntimeApi:
			sp_consensus_babe::BabeApi<Block, Error=sp_blockchain::Error> +
			sp_block_builder::BlockBuilder<Block>,
		BlockImportBuilder: self::BlockImportBuilder<Block, RuntimeApi, Executor>,
		Executor: NativeExecutionDispatch + 'static,
		<BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::FullBlockImport:
			sp_consensus::JustificationImport<Block, Error=sp_consensus::Error> + Clone + Send + Sync + 'static,
		<BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::LightBlockImport:
			sp_consensus::FinalityProofImport<Block, Error=sp_consensus::Error> + Clone + Send + Sync + 'static,
{
	type LightImportQueue = sc_consensus_babe::BabeImportQueue<
		Block, sp_api::TransactionFor<TLightClient<Block, RuntimeApi, Executor>, Block>
	>;

	type FullImportQueue = sc_consensus_babe::BabeImportQueue<
		Block, sp_api::TransactionFor<TFullClient<Block, RuntimeApi, Executor>, Block>
	>;

	type BlockImportBuilder = BlockImportBuilder;

	type Link = sc_consensus_babe::BabeLink<Block>;

	type ImportQueueBlockImport = sc_consensus_babe::BabeBlockImport<
		Block, TFullClient<Block, RuntimeApi, Executor>,
		<Self::BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::FullBlockImport
	>;

	fn build_light<SC: sp_consensus::SelectChain<Block> + 'static>(
		config: &Configuration,
		client: Arc<TLightClient<Block, RuntimeApi, Executor>>,
		inherent_data_providers: sp_inherents::InherentDataProviders,
		task_manager: &TaskManager,
		block_import: <Self::BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::LightBlockImport,
		select_chain: SC,
	) -> Result<Self::LightImportQueue, Error> {
		let (babe_block_import, babe_link) = sc_consensus_babe::block_import(
			sc_consensus_babe::Config::get_or_compute(&*client)?,
			block_import.clone(),
			client.clone(),
		)?;

		sc_consensus_babe::import_queue(
			babe_link,
			babe_block_import,
			None,
			Some(Box::new(block_import)),
			client.clone(),
			select_chain,
			inherent_data_providers.clone(),
			&task_manager.spawn_handle(),
			config.prometheus_registry(),
		).map_err(|err| err.into())
	}

	fn build_full<SC: sp_consensus::SelectChain<Block> + 'static>(
		config: &Configuration,
		client: Arc<TFullClient<Block, RuntimeApi, Executor>>,
		inherent_data_providers: sp_inherents::InherentDataProviders,
		task_manager: &TaskManager,
		block_import: <Self::BlockImportBuilder as self::BlockImportBuilder<Block, RuntimeApi, Executor>>::FullBlockImport,
		select_chain: SC,
	) -> Result<(Self::FullImportQueue, Self::Link, Self::ImportQueueBlockImport), Error> {
		let (babe_block_import, babe_link) = sc_consensus_babe::block_import(
			sc_consensus_babe::Config::get_or_compute(&*client)?,
			block_import.clone(),
			client.clone(),
		)?;

		let import_queue = sc_consensus_babe::import_queue(
			babe_link.clone(),
			babe_block_import.clone(),
			Some(Box::new(block_import)),
			None,
			client.clone(),
			select_chain,
			inherent_data_providers.clone(),
			&task_manager.spawn_handle(),
			config.prometheus_registry(),
		)?;

		Ok((import_queue, babe_link, babe_block_import))
	}
}

pub trait FinalityProofProviderBuilder<Block: BlockT, RuntimeApi, Executor> {
	type LightFPP: sc_network::config::FinalityProofProvider<Block> + 'static;
	type FullFPP: sc_network::config::FinalityProofProvider<Block> + 'static;

	fn build_light(
		backend: Arc<TLightBackend<Block>>,
		client: Arc<TLightClient<Block, RuntimeApi, Executor>>,
	) -> Self::LightFPP;

	fn build_full(
		backend: Arc<TFullBackend<Block>>,
		client: Arc<TFullClient<Block, RuntimeApi, Executor>>,
	) -> Self::FullFPP;
}

pub struct GrandpaFinalityProofProviderBuilder;

impl<Block, RuntimeApi, Executor> FinalityProofProviderBuilder<Block, RuntimeApi, Executor> for GrandpaFinalityProofProviderBuilder
	where
		Block: BlockT,
		RuntimeApi: Send + Sync + 'static,
		Executor: NativeExecutionDispatch + 'static,
		sp_api::NumberFor<Block>: grandpa::BlockNumberOps,
{
	type LightFPP = grandpa::FinalityProofProvider<TLightBackend<Block>, Block>;
	type FullFPP = grandpa::FinalityProofProvider<TFullBackend<Block>, Block>;

	fn build_light(
		backend: Arc<TLightBackend<Block>>,
		client: Arc<TLightClient<Block, RuntimeApi, Executor>>,
	) -> Self::LightFPP {
		grandpa::FinalityProofProvider::new(backend, client as Arc<_>)
	}

	fn build_full(
		backend: Arc<TFullBackend<Block>>,
		client: Arc<TFullClient<Block, RuntimeApi, Executor>>,
	) -> Self::FullFPP {
		grandpa::FinalityProofProvider::new(backend, client as Arc<_>)
	}
}

pub trait SelectChainBuilder<Block: BlockT> {
	type FullSelectChain: sp_consensus::SelectChain<Block> + 'static;
	type LightSelectChain: sp_consensus::SelectChain<Block> + 'static;

	fn build_full(backend: Arc<TFullBackend<Block>>) -> Self::FullSelectChain;
	fn build_light(backend: Arc<TLightBackend<Block>>) -> Self::LightSelectChain;
}

pub struct LongestChainBuilder;

impl<Block: BlockT> SelectChainBuilder<Block> for LongestChainBuilder {
	type FullSelectChain = sc_consensus::LongestChain<TFullBackend<Block>, Block>;
	type LightSelectChain = sc_consensus::LongestChain<TLightBackend<Block>, Block>;

	fn build_full(backend: Arc<TFullBackend<Block>>) -> Self::FullSelectChain {
		sc_consensus::LongestChain::new(backend)
	}

	fn build_light(backend: Arc<TLightBackend<Block>>) -> Self::LightSelectChain {
		sc_consensus::LongestChain::new(backend)
	}
}

pub type BlockFor<Builder> = <Builder as self::Builder>::Block;
pub type RuntimeApiFor<Builder> = <Builder as self::Builder>::RuntimeApi;
pub type ExecutorFor<Builder> = <Builder as self::Builder>::Executor;

pub type FullClientFor<Builder> = TFullClient<BlockFor<Builder>, RuntimeApiFor<Builder>, ExecutorFor<Builder>>;
pub type LightClientFor<Builder> = TLightClient<BlockFor<Builder>, RuntimeApiFor<Builder>, ExecutorFor<Builder>>;
pub type LightBackendFor<Builder> = TLightBackend<BlockFor<Builder>>;

pub type FullTransactionPoolFor<Builder> =
	<<Builder as self::Builder>::TransactionPoolBuilder as
		TransactionPoolBuilder<Builder>>::FullTransactionPool;

pub type LightTransactionPoolFor<Builder> =
	<<Builder as self::Builder>::TransactionPoolBuilder as
		TransactionPoolBuilder<Builder>>::LightTransactionPool;

pub type SelectChainFor<Builder> =
	<<Builder as self::Builder>::SelectChainBuilder as SelectChainBuilder<BlockFor<Builder>>>::FullSelectChain;

pub type BlockImportLinkFor<Builder> =
	<<Builder as self::Builder>::BlockImportBuilder as
		BlockImportBuilder<BlockFor<Builder>, RuntimeApiFor<Builder>, ExecutorFor<Builder>>>::Link;

pub type ImportQueueLinkFor<Builder> =
	<<Builder as self::Builder>::ImportQueueBuilder as
		ImportQueueBuilder<BlockFor<Builder>, RuntimeApiFor<Builder>, ExecutorFor<Builder>>>::Link;

pub type LightImportQueueFor<Builder> =
	<<Builder as self::Builder>::ImportQueueBuilder as
		ImportQueueBuilder<BlockFor<Builder>, RuntimeApiFor<Builder>, ExecutorFor<Builder>>>::LightImportQueue;
	

pub trait Builder: Sized {
	type Block: BlockT;
	type RuntimeApi:
		sp_api::ConstructRuntimeApi<Self::Block, TLightClient<Self::Block, Self::RuntimeApi, Self::Executor>> +
		sp_api::ConstructRuntimeApi<Self::Block, TFullClient<Self::Block, Self::RuntimeApi, Self::Executor>>;
	type Executor: NativeExecutionDispatch + 'static;

	type TransactionPoolBuilder: TransactionPoolBuilder<Self>;
	type BlockImportBuilder: BlockImportBuilder<
		Self::Block, Self::RuntimeApi, Self::Executor,
		SelectChainBuilder=Self::SelectChainBuilder
	>;
	type ImportQueueBuilder: ImportQueueBuilder<
		Self::Block, Self::RuntimeApi, Self::Executor,
		BlockImportBuilder=Self::BlockImportBuilder
	>;
	type FinalityProofProviderBuilder: FinalityProofProviderBuilder<Self::Block, Self::RuntimeApi, Self::Executor>;
	type SelectChainBuilder: SelectChainBuilder<Self::Block>;
	type RpcExtensions: RpcExtensions<Builder=Self>;

	fn build_light(config: Configuration) -> Result<ServiceParams<
		Self::Block, LightClientFor<Self>,
		LightImportQueueFor<Self>,
		LightTransactionPoolFor<Self>,
		(),
		LightBackendFor<Self>
	>, Error>
		where
			Self::Executor: NativeExecutionDispatch + 'static,
			Self::RuntimeApi:
				sp_api::ConstructRuntimeApi<Self::Block, TLightClient<Self::Block, Self::RuntimeApi, Self::Executor>>
				+ Send + Sync + 'static,
			<Self::RuntimeApi as sp_api::ConstructRuntimeApi<Self::Block, TLightClient<Self::Block, Self::RuntimeApi, Self::Executor>>>::RuntimeApi:
				sp_api::Metadata<Self::Block> +
				sc_offchain::OffchainWorkerApi<Self::Block> +
				sp_transaction_pool::runtime_api::TaggedTransactionQueue<Self::Block> +
				sp_session::SessionKeys<Self::Block> +
				sp_api::ApiErrorExt<Error = sp_blockchain::Error> +
				sp_api::ApiExt<Self::Block, StateBackend = <TLightBackend<Self::Block> as sc_client_api::backend::Backend<Self::Block>>::State>
	{
		use sc_client_api::RemoteBackend;

		let (client, backend, keystore, task_manager, on_demand) =
			new_light_parts::<Self::Block, Self::RuntimeApi, Self::Executor>(&config)?;

		let transaction_pool = Self::TransactionPoolBuilder::build_light(
			&config, client.clone(), &task_manager, on_demand.clone(),
		);

		let (block_import, finality_proof_request_builder) = Self::BlockImportBuilder::build_light(
			client.clone(), backend.clone(), on_demand.clone(),
		)?;

		let select_chain = Self::SelectChainBuilder::build_light(backend.clone());

		let import_queue = Self::ImportQueueBuilder::build_light(
			&config, client.clone(), sp_inherents::InherentDataProviders::new(), &task_manager,
			block_import, select_chain,
		)?;

		let finality_proof_provider = Self::FinalityProofProviderBuilder::build_light(
			backend.clone(), client.clone(),
		);
	
		Ok(ServiceParams {	
			block_announce_validator_builder: None,
			finality_proof_request_builder: Some(finality_proof_request_builder),
			finality_proof_provider: Some(Arc::new(finality_proof_provider)),
			on_demand: Some(on_demand),
			remote_blockchain: Some(backend.remote_blockchain()),
			rpc_extensions_builder: Box::new(|_| ()),
			transaction_pool,
			config, client, import_queue, keystore, backend, task_manager
		})
	}

	fn build_full(config: Configuration, rpc_extensions: Self::RpcExtensions) -> Result<(
		ServiceParams<
			Self::Block, TFullClient<Self::Block, Self::RuntimeApi, Self::Executor>,
			<Self::ImportQueueBuilder as ImportQueueBuilder<Self::Block, Self::RuntimeApi, Self::Executor>>::FullImportQueue,
			<Self::TransactionPoolBuilder as TransactionPoolBuilder<Self>>::FullTransactionPool,
			<Self::RpcExtensions as RpcExtensions>::Rpc,
			TFullBackend<Self::Block>
		>,
		<Self::SelectChainBuilder as SelectChainBuilder<Self::Block>>::FullSelectChain,
		sp_inherents::InherentDataProviders,
		<Self::BlockImportBuilder as BlockImportBuilder<Self::Block, Self::RuntimeApi, Self::Executor>>::FullBlockImport,
		<Self::BlockImportBuilder as BlockImportBuilder<Self::Block, Self::RuntimeApi, Self::Executor>>::Link,
		<Self::ImportQueueBuilder as ImportQueueBuilder<Self::Block, Self::RuntimeApi, Self::Executor>>::Link,
		<Self::ImportQueueBuilder as ImportQueueBuilder<Self::Block, Self::RuntimeApi, Self::Executor>>::ImportQueueBlockImport,
		<Self::RpcExtensions as RpcExtensions>::RpcSetup,
	), Error>
		where
			Self::Executor: NativeExecutionDispatch + 'static,
			Self::RuntimeApi:
				sp_api::ConstructRuntimeApi<Self::Block, TFullClient<Self::Block, Self::RuntimeApi, Self::Executor>>
				+ Send + Sync + 'static,
			<Self::RuntimeApi as sp_api::ConstructRuntimeApi<Self::Block, TFullClient<Self::Block, Self::RuntimeApi, Self::Executor>>>::RuntimeApi:
				sp_api::Metadata<Self::Block> +
				sc_offchain::OffchainWorkerApi<Self::Block> +
				sp_transaction_pool::runtime_api::TaggedTransactionQueue<Self::Block> +
				sp_session::SessionKeys<Self::Block> +
				sp_api::ApiErrorExt<Error = sp_blockchain::Error> +
				sp_api::ApiExt<Self::Block, StateBackend = <TFullBackend<Self::Block> as sc_client_api::backend::Backend<Self::Block>>::State>
	{
		let (client, backend, keystore, task_manager) =
			new_full_parts::<Self::Block, Self::RuntimeApi, Self::Executor>(&config)?;
		let client = Arc::new(client);

		let transaction_pool = Self::TransactionPoolBuilder::build_full(
			&config, client.clone(), &task_manager,
		);

		let select_chain = Self::SelectChainBuilder::build_full(backend.clone());

		let (block_import, block_import_link) = Self::BlockImportBuilder::build_full(
			client.clone(), select_chain.clone(),
		)?;

		let inherent_data_providers = sp_inherents::InherentDataProviders::new();

		let (import_queue, import_queue_link, import_queue_block_import) =
			Self::ImportQueueBuilder::build_full(
				&config, client.clone(), inherent_data_providers.clone(),
				&task_manager, block_import.clone(), select_chain.clone(),
			)?;

		let finality_proof_provider = Self::FinalityProofProviderBuilder::build_full(
			backend.clone(), client.clone(),
		);

		let (rpc_extensions_builder, rpc_setup) = rpc_extensions.rpc_extensions(
			client.clone(), transaction_pool.clone(), select_chain.clone(),
			keystore.clone(), &block_import_link, &import_queue_link
		);

		let params = ServiceParams {	
			backend, client, import_queue, keystore, task_manager, transaction_pool,
			config: config,
			block_announce_validator_builder: None,
			finality_proof_request_builder: None,
			finality_proof_provider: Some(Arc::new(finality_proof_provider)),
			on_demand: None,
			remote_blockchain: None,
			rpc_extensions_builder,
		};

		Ok((
			params, select_chain, inherent_data_providers, block_import, block_import_link,
			import_queue_link, import_queue_block_import, rpc_setup
		))
	}

	fn build_ops(config: Configuration) -> Result<(
		Arc<TFullClient<Self::Block, Self::RuntimeApi, Self::Executor>>,
		Arc<TFullBackend<Self::Block>>,
		<Self::ImportQueueBuilder as ImportQueueBuilder<Self::Block, Self::RuntimeApi, Self::Executor>>::FullImportQueue,
		TaskManager,
	), Error> {
		let (client, backend, keystore, task_manager) =
			new_full_parts::<Self::Block, Self::RuntimeApi, Self::Executor>(&config)?;
		let client = Arc::new(client);

		let select_chain = Self::SelectChainBuilder::build_full(backend.clone());

		let (block_import, _) = Self::BlockImportBuilder::build_full(
			client.clone(), select_chain.clone(),
		)?;

		let (import_queue, _, _) = Self::ImportQueueBuilder::build_full(
			&config, client.clone(), sp_inherents::InherentDataProviders::new(),
			&task_manager, block_import, select_chain,
		)?;

		Ok((client, backend, import_queue, task_manager))
	}
}


pub trait RpcExtensions{
	type RpcSetup;
	type Rpc;
	type Builder: Builder;

	fn rpc_extensions(
		&self,
		client: Arc<FullClientFor<Self::Builder>>,
		transaction_pool: Arc<FullTransactionPoolFor<Self::Builder>>,
		select_chain: SelectChainFor<Self::Builder>,
		keystore: KeystorePtr,
		block_import_link: &BlockImportLinkFor<Self::Builder>,
		import_queue_link: &ImportQueueLinkFor<Self::Builder>,
	) -> (Box<dyn RpcExtensionBuilder<Output = Self::Rpc> + Send>, Self::RpcSetup);
}

pub struct NoRpc<Builder>(std::marker::PhantomData<Builder>);

impl<Builder> Default for NoRpc<Builder> {
	fn default() -> Self {
		Self(std::marker::PhantomData)
	}
}

impl<Builder: self::Builder> RpcExtensions for NoRpc<Builder> {
	type RpcSetup = ();
	type Rpc = ();
	type Builder = Builder;

	fn rpc_extensions(
		&self,
		client: Arc<FullClientFor<Self::Builder>>,
		transaction_pool: Arc<FullTransactionPoolFor<Self::Builder>>,
		select_chain: SelectChainFor<Self::Builder>,
		keystore: KeystorePtr,
		block_import_link: &BlockImportLinkFor<Self::Builder>,
		import_queue_link: &ImportQueueLinkFor<Self::Builder>,
	) -> (Box<dyn RpcExtensionBuilder<Output = Self::Rpc> + Send>, Self::RpcSetup) {
		(Box::new(|_| ()), ())
	}
}

pub struct RpcFunction<Builder: self::Builder, Rpc, RpcSetup>(
	pub fn(
		Arc<FullClientFor<Builder>>,
		Arc<FullTransactionPoolFor<Builder>>,
		SelectChainFor<Builder>,
		KeystorePtr,
		&BlockImportLinkFor<Builder>,
		&ImportQueueLinkFor<Builder>,
	) -> (Box<dyn RpcExtensionBuilder<Output = Rpc> + Send>, RpcSetup)
);

impl<Builder: self::Builder, RpcSetup, Rpc> RpcExtensions for RpcFunction<Builder, Rpc, RpcSetup> {
	type RpcSetup = RpcSetup;
	type Rpc = Rpc;
	type Builder = Builder;

	fn rpc_extensions(
		&self,
		client: Arc<FullClientFor<Self::Builder>>,
		transaction_pool: Arc<FullTransactionPoolFor<Self::Builder>>,
		select_chain: SelectChainFor<Self::Builder>,
		keystore: KeystorePtr,
		block_import_link: &BlockImportLinkFor<Self::Builder>,
		import_queue_link: &ImportQueueLinkFor<Self::Builder>,
	) -> (Box<dyn RpcExtensionBuilder<Output = Self::Rpc> + Send>, Self::RpcSetup) {
		(self.0)(client, transaction_pool, select_chain, keystore, block_import_link, import_queue_link)
	}
}

/// Put together the components of a service from the parameters.
pub fn build<TBl, TBackend, TImpQu, TExPool, TRpc, TCl>(
	builder: ServiceParams<TBl, TCl, TImpQu, TExPool, TRpc, TBackend>,
) -> Result<ServiceComponents<TBl, TBackend, TCl>, Error>
	where
		TCl: ProvideRuntimeApi<TBl> + HeaderMetadata<TBl, Error=sp_blockchain::Error> + Chain<TBl> +
		BlockBackend<TBl> + BlockIdTo<TBl, Error=sp_blockchain::Error> + ProofProvider<TBl> +
		HeaderBackend<TBl> + BlockchainEvents<TBl> + ExecutorProvider<TBl> + UsageProvider<TBl> +
		StorageProvider<TBl, TBackend> + CallApiAt<TBl, Error=sp_blockchain::Error> +
		Send + 'static,
		<TCl as ProvideRuntimeApi<TBl>>::Api:
			sp_api::Metadata<TBl> +
			sc_offchain::OffchainWorkerApi<TBl> +
			sp_transaction_pool::runtime_api::TaggedTransactionQueue<TBl> +
			sp_session::SessionKeys<TBl> +
			sp_api::ApiErrorExt<Error = sp_blockchain::Error> +
			sp_api::ApiExt<TBl, StateBackend = TBackend::State>,
		TBl: BlockT,
		TBackend: 'static + sc_client_api::backend::Backend<TBl> + Send,
		TImpQu: 'static + ImportQueue<TBl>,
		TExPool: MaintainedTransactionPool<Block=TBl, Hash = <TBl as BlockT>::Hash> + MallocSizeOfWasm + 'static,
		TRpc: sc_rpc::RpcExtension<sc_rpc::Metadata>
{
	let ServiceParams {
		mut config,
		mut task_manager,
		client,
		on_demand,
		backend,
		keystore,
		import_queue,
		finality_proof_request_builder,
		finality_proof_provider,
		transaction_pool,
		rpc_extensions_builder,
		remote_blockchain,
		block_announce_validator_builder,
	} = builder;

	let chain_info = client.usage_info().chain;

	sp_session::generate_initial_session_keys(
		client.clone(),
		&BlockId::Hash(chain_info.best_hash),
		config.dev_key_seed.clone().map(|s| vec![s]).unwrap_or_default(),
	)?;

	info!("📦 Highest known block at #{}", chain_info.best_number);
	telemetry!(
		SUBSTRATE_INFO;
		"node.start";
		"height" => chain_info.best_number.saturated_into::<u64>(),
		"best" => ?chain_info.best_hash
	);

	let (system_rpc_tx, system_rpc_rx) = tracing_unbounded("mpsc_system_rpc");

	let (network, network_status_sinks, network_future) = build_network(
		&config, client.clone(), transaction_pool.clone(), task_manager.spawn_handle(),
		on_demand.clone(), block_announce_validator_builder, finality_proof_request_builder,
		finality_proof_provider, system_rpc_rx, import_queue
	)?;

	let spawn_handle = task_manager.spawn_handle();

	// The network worker is responsible for gathering all network messages and processing
	// them. This is quite a heavy task, and at the time of the writing of this comment it
	// frequently happens that this future takes several seconds or in some situations
	// even more than a minute until it has processed its entire queue. This is clearly an
	// issue, and ideally we would like to fix the network future to take as little time as
	// possible, but we also take the extra harm-prevention measure to execute the networking
	// future using `spawn_blocking`.
	spawn_handle.spawn_blocking("network-worker", network_future);

	let offchain_storage = backend.offchain_storage();
	let offchain_workers = match (config.offchain_worker.clone(), offchain_storage.clone()) {
		(OffchainWorkerConfig {enabled: true, .. }, Some(db)) => {
			Some(Arc::new(sc_offchain::OffchainWorkers::new(client.clone(), db)))
		},
		(OffchainWorkerConfig {enabled: true, .. }, None) => {
			warn!("Offchain workers disabled, due to lack of offchain storage support in backend.");
			None
		},
		_ => None,
	};

	// Inform the tx pool about imported and finalized blocks.
	spawn_handle.spawn(
		"txpool-notifications",
		sc_transaction_pool::notification_future(client.clone(), transaction_pool.clone()),
	);

	// Inform the offchain worker about new imported blocks
	if let Some(offchain) = offchain_workers.clone() {
		spawn_handle.spawn(
			"offchain-notifications",
			sc_offchain::notification_future(
				config.role.is_authority(),
				client.clone(),
				offchain,
				task_manager.spawn_handle(),
				network.clone()
			)
		);
	}

	spawn_handle.spawn(
		"on-transaction-imported",
		transaction_notifications(transaction_pool.clone(), network.clone()),
	);

	// Prometheus metrics.
	let metrics_service = if let Some(PrometheusConfig { port, registry }) = config.prometheus_config.clone() {
		// Set static metrics.
		let metrics = MetricsService::with_prometheus(&registry, &config)?;
		spawn_handle.spawn(
			"prometheus-endpoint",
			prometheus_endpoint::init_prometheus(port, registry).map(drop)
		);

		metrics
	} else {
		MetricsService::new()
	};

	// Periodically notify the telemetry.
	spawn_handle.spawn("telemetry-periodic-send", telemetry_periodic_send(
		client.clone(), transaction_pool.clone(), metrics_service, network_status_sinks.clone()
	));

	// Periodically send the network state to the telemetry.
	spawn_handle.spawn(
		"telemetry-periodic-network-state",
		telemetry_periodic_network_state(network_status_sinks.clone()),
	);

	// RPC
	let gen_handler = |deny_unsafe: sc_rpc::DenyUnsafe| gen_handler(
		deny_unsafe, &config, task_manager.spawn_handle(), client.clone(), transaction_pool.clone(),
		keystore.clone(), on_demand.clone(), remote_blockchain.clone(), &*rpc_extensions_builder,
		offchain_storage.clone(), system_rpc_tx.clone()
	);
	let rpc = start_rpc_servers(&config, gen_handler)?;
	// This is used internally, so don't restrict access to unsafe RPC
	let rpc_handlers = Arc::new(RpcHandlers(gen_handler(sc_rpc::DenyUnsafe::No)));

	let telemetry_connection_sinks: Arc<Mutex<Vec<TracingUnboundedSender<()>>>> = Default::default();

	// Telemetry
	let telemetry = config.telemetry_endpoints.clone().map(|endpoints| {
		let genesis_hash = match client.block_hash(Zero::zero()) {
			Ok(Some(hash)) => hash,
			_ => Default::default(),
		};

		build_telemetry(
			&mut config, endpoints, telemetry_connection_sinks.clone(), network.clone(),
			task_manager.spawn_handle(), genesis_hash,
		)
	});

	// Instrumentation
	if let Some(tracing_targets) = config.tracing_targets.as_ref() {
		let subscriber = sc_tracing::ProfilingSubscriber::new(
			config.tracing_receiver, tracing_targets
		);
		match tracing::subscriber::set_global_default(subscriber) {
			Ok(_) => (),
			Err(e) => error!(target: "tracing", "Unable to set global default subscriber {}", e),
		}
	}

	// Spawn informant task
	spawn_handle.spawn("informant", sc_informant::build(
		client.clone(),
		network_status_sinks.clone(),
		transaction_pool.clone(),
		config.informant_output_format,
	));

	task_manager.keep_alive((telemetry, config.base_path, rpc, rpc_handlers.clone()));

	Ok(ServiceComponents {
		task_manager, network, rpc_handlers, offchain_workers,
		telemetry_on_connect_sinks: TelemetryOnConnectSinks(telemetry_connection_sinks),
		network_status_sinks: NetworkStatusSinks::new(network_status_sinks),
	})
}

async fn transaction_notifications<TBl, TExPool>(
	transaction_pool: Arc<TExPool>,
	network: Arc<NetworkService<TBl, <TBl as BlockT>::Hash>>
)
	where
		TBl: BlockT,
		TExPool: MaintainedTransactionPool<Block=TBl, Hash = <TBl as BlockT>::Hash>,
{
	// transaction notifications
	transaction_pool.import_notification_stream()
		.for_each(move |hash| {
			network.propagate_transaction(hash);
			let status = transaction_pool.status();
			telemetry!(SUBSTRATE_INFO; "txpool.import";
				"ready" => status.ready,
				"future" => status.future
			);
			ready(())
		})
		.await;
}

// Periodically notify the telemetry.
async fn telemetry_periodic_send<TBl, TExPool, TCl>(
	client: Arc<TCl>,
	transaction_pool: Arc<TExPool>,
	mut metrics_service: MetricsService,
	network_status_sinks: Arc<status_sinks::StatusSinks<(NetworkStatus<TBl>, NetworkState)>>
)
	where
		TBl: BlockT,
		TCl: ProvideRuntimeApi<TBl> + UsageProvider<TBl>,
		TExPool: MaintainedTransactionPool<Block=TBl, Hash = <TBl as BlockT>::Hash>,
{
	let (state_tx, state_rx) = tracing_unbounded::<(NetworkStatus<_>, NetworkState)>("mpsc_netstat1");
	network_status_sinks.push(std::time::Duration::from_millis(5000), state_tx);
	state_rx.for_each(move |(net_status, _)| {
		let info = client.usage_info();
		metrics_service.tick(
			&info,
			&transaction_pool.status(),
			&net_status,
		);
		ready(())
	}).await;
}

async fn telemetry_periodic_network_state<TBl: BlockT>(
	network_status_sinks: Arc<status_sinks::StatusSinks<(NetworkStatus<TBl>, NetworkState)>>
) {
	// Periodically send the network state to the telemetry.
	let (netstat_tx, netstat_rx) = tracing_unbounded::<(NetworkStatus<_>, NetworkState)>("mpsc_netstat2");
	network_status_sinks.push(std::time::Duration::from_secs(30), netstat_tx);
	netstat_rx.for_each(move |(_, network_state)| {
		telemetry!(
			SUBSTRATE_INFO;
			"system.network_state";
			"state" => network_state,
		);
		ready(())
	}).await;
}

fn build_telemetry<TBl: BlockT>(
	config: &mut Configuration,
	endpoints: sc_telemetry::TelemetryEndpoints,
	telemetry_connection_sinks: Arc<Mutex<Vec<TracingUnboundedSender<()>>>>,
	network: Arc<NetworkService<TBl, <TBl as BlockT>::Hash>>,
	spawn_handle: SpawnTaskHandle,
	genesis_hash: <TBl as BlockT>::Hash,
) -> sc_telemetry::Telemetry {
	let is_authority = config.role.is_authority();
	let network_id = network.local_peer_id().to_base58();
	let name = config.network.node_name.clone();
	let impl_name = config.impl_name.clone();
	let impl_version = config.impl_version.clone();
	let chain_name = config.chain_spec.name().to_owned();
	let telemetry = sc_telemetry::init_telemetry(sc_telemetry::TelemetryConfig {
		endpoints,
		wasm_external_transport: config.telemetry_external_transport.take(),
	});
	let startup_time = SystemTime::UNIX_EPOCH.elapsed()
		.map(|dur| dur.as_millis())
		.unwrap_or(0);
	
	spawn_handle.spawn(
		"telemetry-worker",
		telemetry.clone()
			.for_each(move |event| {
				// Safe-guard in case we add more events in the future.
				let sc_telemetry::TelemetryEvent::Connected = event;

				telemetry!(SUBSTRATE_INFO; "system.connected";
					"name" => name.clone(),
					"implementation" => impl_name.clone(),
					"version" => impl_version.clone(),
					"config" => "",
					"chain" => chain_name.clone(),
					"genesis_hash" => ?genesis_hash,
					"authority" => is_authority,
					"startup_time" => startup_time,
					"network_id" => network_id.clone()
				);

				telemetry_connection_sinks.lock().retain(|sink| {
					sink.unbounded_send(()).is_ok()
				});
				ready(())
			})
	);

	telemetry
}

fn gen_handler<TBl, TBackend, TExPool, TRpc, TCl>(
	deny_unsafe: sc_rpc::DenyUnsafe,
	config: &Configuration,
	spawn_handle: SpawnTaskHandle,
	client: Arc<TCl>,
	transaction_pool: Arc<TExPool>,
	keystore: KeystorePtr,
	on_demand: Option<Arc<OnDemand<TBl>>>,
	remote_blockchain: Option<Arc<dyn RemoteBlockchain<TBl>>>,
	rpc_extensions_builder: &(dyn RpcExtensionBuilder<Output = TRpc> + Send),
	offchain_storage: Option<<TBackend as sc_client_api::backend::Backend<TBl>>::OffchainStorage>,
	system_rpc_tx: TracingUnboundedSender<sc_rpc::system::Request<TBl>>
) -> jsonrpc_pubsub::PubSubHandler<sc_rpc::Metadata>
	where
		TBl: BlockT,
		TCl: ProvideRuntimeApi<TBl> + BlockchainEvents<TBl> + HeaderBackend<TBl> +
		HeaderMetadata<TBl, Error=sp_blockchain::Error> + ExecutorProvider<TBl> +
		CallApiAt<TBl, Error=sp_blockchain::Error> + ProofProvider<TBl> +
		StorageProvider<TBl, TBackend> + BlockBackend<TBl> + Send + Sync + 'static,
		TExPool: MaintainedTransactionPool<Block=TBl, Hash = <TBl as BlockT>::Hash> + 'static,
		TBackend: sc_client_api::backend::Backend<TBl> + 'static,
		TRpc: sc_rpc::RpcExtension<sc_rpc::Metadata>,
		<TCl as ProvideRuntimeApi<TBl>>::Api:
			sp_session::SessionKeys<TBl> +
			sp_api::Metadata<TBl, Error = sp_blockchain::Error>,
{
	use sc_rpc::{chain, state, author, system, offchain};

	let system_info = sc_rpc::system::SystemInfo {
		chain_name: config.chain_spec.name().into(),
		impl_name: config.impl_name.clone(),
		impl_version: config.impl_version.clone(),
		properties: config.chain_spec.properties(),
		chain_type: config.chain_spec.chain_type(),
	};

	let subscriptions = SubscriptionManager::new(Arc::new(spawn_handle));

	let (chain, state, child_state) = if let (Some(remote_blockchain), Some(on_demand)) =
		(remote_blockchain, on_demand) {
		// Light clients
		let chain = sc_rpc::chain::new_light(
			client.clone(),
			subscriptions.clone(),
			remote_blockchain.clone(),
			on_demand.clone()
		);
		let (state, child_state) = sc_rpc::state::new_light(
			client.clone(),
			subscriptions.clone(),
			remote_blockchain.clone(),
			on_demand.clone()
		);
		(chain, state, child_state)

	} else {
		// Full nodes
		let chain = sc_rpc::chain::new_full(client.clone(), subscriptions.clone());
		let (state, child_state) = sc_rpc::state::new_full(client.clone(), subscriptions.clone());
		(chain, state, child_state)
	};

	let author = sc_rpc::author::Author::new(
		client.clone(),
		transaction_pool.clone(),
		subscriptions,
		keystore.clone(),
		deny_unsafe,
	);
	let system = system::System::new(system_info, system_rpc_tx.clone(), deny_unsafe);

	let maybe_offchain_rpc = offchain_storage.clone()
	.map(|storage| {
		let offchain = sc_rpc::offchain::Offchain::new(storage, deny_unsafe);
		// FIXME: Use plain Option (don't collect into HashMap) when we upgrade to jsonrpc 14.1
		// https://github.com/paritytech/jsonrpc/commit/20485387ed06a48f1a70bf4d609a7cde6cf0accf
		let delegate = offchain::OffchainApi::to_delegate(offchain);
			delegate.into_iter().collect::<HashMap<_, _>>()
	}).unwrap_or_default();

	sc_rpc_server::rpc_handler((
		state::StateApi::to_delegate(state),
		state::ChildStateApi::to_delegate(child_state),
		chain::ChainApi::to_delegate(chain),
		maybe_offchain_rpc,
		author::AuthorApi::to_delegate(author),
		system::SystemApi::to_delegate(system),
		rpc_extensions_builder.build(deny_unsafe),
	))
}

fn build_network<TBl, TExPool, TImpQu, TCl>(
	config: &Configuration,
	client: Arc<TCl>,
	transaction_pool: Arc<TExPool>,
	spawn_handle: SpawnTaskHandle,
	on_demand: Option<Arc<OnDemand<TBl>>>,
	block_announce_validator_builder: Option<Box<
		dyn FnOnce(Arc<TCl>) -> Box<dyn BlockAnnounceValidator<TBl> + Send> + Send
	>>,
	finality_proof_request_builder: Option<BoxFinalityProofRequestBuilder<TBl>>,
	finality_proof_provider: Option<Arc<dyn FinalityProofProvider<TBl>>>,
	system_rpc_rx: TracingUnboundedReceiver<sc_rpc::system::Request<TBl>>,
	import_queue: TImpQu
) -> Result<
	(
		Arc<NetworkService<TBl, <TBl as BlockT>::Hash>>,
		Arc<status_sinks::StatusSinks<(NetworkStatus<TBl>, NetworkState)>>,
		Pin<Box<dyn Future<Output = ()> + Send>>
	),
	Error
>
	where
		TBl: BlockT,
		TCl: ProvideRuntimeApi<TBl> + HeaderMetadata<TBl, Error=sp_blockchain::Error> + Chain<TBl> +
		BlockBackend<TBl> + BlockIdTo<TBl, Error=sp_blockchain::Error> + ProofProvider<TBl> +
		HeaderBackend<TBl> + BlockchainEvents<TBl> + 'static,
		TExPool: MaintainedTransactionPool<Block=TBl, Hash = <TBl as BlockT>::Hash> + 'static,
		TImpQu: ImportQueue<TBl> + 'static,
{
	let transaction_pool_adapter = Arc::new(TransactionPoolAdapter {
		imports_external_transactions: !matches!(config.role, Role::Light),
		pool: transaction_pool.clone(),
		client: client.clone(),
	});

	let protocol_id = {
		let protocol_id_full = match config.chain_spec.protocol_id() {
			Some(pid) => pid,
			None => {
				warn!("Using default protocol ID {:?} because none is configured in the \
					chain specs", DEFAULT_PROTOCOL_ID
				);
				DEFAULT_PROTOCOL_ID
			}
		}.as_bytes();
		sc_network::config::ProtocolId::from(protocol_id_full)
	};

	let block_announce_validator = if let Some(f) = block_announce_validator_builder {
		f(client.clone())
	} else {
		Box::new(DefaultBlockAnnounceValidator)
	};

	let network_params = sc_network::config::Params {
		role: config.role.clone(),
		executor: {
			Some(Box::new(move |fut| {
				spawn_handle.spawn("libp2p-node", fut);
			}))
		},
		network_config: config.network.clone(),
		chain: client.clone(),
		finality_proof_provider,
		finality_proof_request_builder,
		on_demand: on_demand.clone(),
		transaction_pool: transaction_pool_adapter.clone() as _,
		import_queue: Box::new(import_queue),
		protocol_id,
		block_announce_validator,
		metrics_registry: config.prometheus_config.as_ref().map(|config| config.registry.clone())
	};

	let has_bootnodes = !network_params.network_config.boot_nodes.is_empty();
	let network_mut = sc_network::NetworkWorker::new(network_params)?;
	let network = network_mut.service().clone();
	let network_status_sinks = Arc::new(status_sinks::StatusSinks::new());

	let future = build_network_future(
		config.role.clone(),
		network_mut,
		client.clone(),
		network_status_sinks.clone(),
		system_rpc_rx,
		has_bootnodes,
		config.announce_block,
	).boxed();

	Ok((network, network_status_sinks, future))
}
