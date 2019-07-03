use std::str::FromStr;
use failure::Error;
use futures::prelude::*;
use futures::future::*;
use std::sync::Arc;

use crate::prelude::*;
use web3::types::{Log, Transaction, Address, H256};
use tiny_keccak::keccak256;
use parity_wasm::elements::Module;

/// Common trait for runtime host implementations.
pub trait RuntimeHost: Send + Sync + Debug {
    /// Returns true if the RuntimeHost has a handler for an Ethereum event.
    fn matches_log(&self, log: &Log) -> bool;

    /// Returns true if the RuntimeHost has a handler for an Ethereum call.
    fn matches_call(&self, call: &EthereumCall) -> bool;

    /// Returns true if the RuntimeHost has a handler for an Ethereum block.
    fn matches_block(&self, call: EthereumBlockTriggerType) -> bool;

    /// Process an Ethereum event and return a vector of entity operations.
    fn process_log(
        &self,
        logger: Logger,
        block: Arc<EthereumBlock>,
        transaction: Arc<Transaction>,
        log: Arc<Log>,
        state: BlockState,
    ) -> Box<Future<Item = BlockState, Error = Error> + Send>;

    /// Process an Ethereum call and return a vector of entity operations
    fn process_call(
        &self,
        logger: Logger,
        block: Arc<EthereumBlock>,
        transaction: Arc<Transaction>,
        call: Arc<EthereumCall>,
        state: BlockState,
    ) -> Box<Future<Item = BlockState, Error = Error> + Send>;

    /// Process an Ethereum block and return a vector of entity operations
    fn process_block(
        &self,
        logger: Logger,
        block: Arc<EthereumBlock>,
        trigger_type: EthereumBlockTriggerType,
        state: BlockState,
    ) -> Box<Future<Item = BlockState, Error = Error> + Send>;
}

pub trait RuntimeHostBuilder: Clone + Send + Sync + 'static {
    type Host: RuntimeHost;

    /// Build a new runtime host for a subgraph data source.
    fn build(
        &self,
        logger: &Logger,
        subgraph_id: SubgraphDeploymentId,
        data_source: DataSource,
    ) -> Result<Self::Host, Error>;
}

#[derive(Clone, Debug)]
pub struct DummyRuntimeHost {}

impl RuntimeHostBuilder for DummyRuntimeHost {
    type Host = DummyRuntimeHost;
    fn build(
        &self,
        logger: &Logger,
        subgraph_id: SubgraphDeploymentId,
        data_source: DataSource,
    ) -> Result<Self::Host, Error> {
        Ok(DummyRuntimeHost {})
    }
}

impl RuntimeHost for DummyRuntimeHost {
    fn matches_log(&self, log: &Log) -> bool {
        println!("Matching log");
        true
    }

    fn matches_call(&self, call: &EthereumCall) -> bool {
        let target_method_id = &call.input.0[..4];
        println!("Matching call {:?}", target_method_id);
        let fhash = keccak256("getCurrentStateRoot()".as_bytes());
        let actual_method_id = [fhash[0], fhash[1], fhash[2], fhash[3]];
        target_method_id == actual_method_id
    }

    /// Returns true if the RuntimeHost has a handler for an Ethereum block.
    fn matches_block(&self, call: EthereumBlockTriggerType) -> bool {
        println!("Matching Block");
        false
    }

    /// Process an Ethereum event and return a vector of entity operations.
    fn process_log(
        &self,
        logger: Logger,
        block: Arc<EthereumBlock>,
        transaction: Arc<Transaction>,
        log: Arc<Log>,
        state: BlockState,
    ) -> Box<Future<Item = BlockState, Error = Error> + Send> {
        println!("Received Log Event");
        Box::new(ok(state))
    }

    /// Process an Ethereum call and return a vector of entity operations
    fn process_call(
        &self,
        logger: Logger,
        block: Arc<EthereumBlock>,
        transaction: Arc<Transaction>,
        call: Arc<EthereumCall>,
        state: BlockState,
    ) -> Box<Future<Item = BlockState, Error = Error> + Send> {
        panic!("Received Event");
        Box::new(ok(state))
    }

    /// Process an Ethereum block and return a vector of entity operations
    fn process_block(
        &self,
        logger: Logger,
        block: Arc<EthereumBlock>,
        trigger_type: EthereumBlockTriggerType,
        state: BlockState,
    ) -> Box<Future<Item = BlockState, Error = Error> + Send> {
        Box::new(ok(state))
    }
}

pub struct DummySubgraphProvider {
    already_produced: bool,
}

impl DummySubgraphProvider {
    pub fn new() -> DummySubgraphProvider {
        DummySubgraphProvider {
            already_produced: false,
        }
    }
}

impl EventProducer<SubgraphAssignmentProviderEvent> for DummySubgraphProvider {
    fn take_event_stream(&mut self) -> Option<Box<Stream<Item = SubgraphAssignmentProviderEvent, Error = ()> + Send>> {
        Some(Box::new(DummySubgraphProvider::new()))
    }
}

use graphql_parser::schema::Document;
use super::super::super::data::subgraph::{BaseSubgraphManifest, SubgraphDeploymentId, Source, Mapping, MappingCallHandler};

impl Stream for DummySubgraphProvider {
    type Item = SubgraphAssignmentProviderEvent;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        if self.already_produced {
            Ok(Async::Ready(None))
        } else {
            self.already_produced = true;
            let address = Address::from_str("51E06e232EB9959B90ba788c4e4d61ffD528520C").unwrap();
            let data_sources = vec![
                DataSource {
                    kind: "DataSource".to_owned(),
                    network: None,
                    name: "getCurrentStateRoot".to_owned(),
                    source: Source {
                        address: Some(address),
                        abi: "1234".to_owned(),
                    },
                    mapping: Mapping {
                        kind: "Mapping".to_owned(),
                        api_version: "1".to_owned(),
                        language: "language".to_owned(),
                        entities: vec![],
                        abis: vec![],
                        block_handlers: None,
                        call_handlers: None,
                        event_handlers: Some(vec![
                            MappingEventHandler {
                                event: "Foo()".to_owned(),
                                topic0: None,
                                handler: "".to_owned(),
                            }
                        ]),
                        runtime: Arc::new(Module::default()),
                        link: Link::from("Link".to_owned()),
                    },
                    templates: None
                }
            ];
            let schema = Schema::new(
                SubgraphDeploymentId::new("testschema").unwrap(),
                Document {definitions: vec![]},
            );
            let manifest = BaseSubgraphManifest::<Schema, DataSource> {
                id: SubgraphDeploymentId::new("testmanifest").unwrap(),
                location: "test_location".to_owned(),
                spec_version: "test_spec_version".to_owned(),
                description: None,
                repository: None,
                schema,
                data_sources: data_sources,
            };

            let event = SubgraphAssignmentProviderEvent::SubgraphStart(manifest);
            Ok(Async::Ready(Some(event)))
        }
    }
}
