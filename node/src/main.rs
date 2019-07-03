extern crate clap;
extern crate env_logger;
extern crate futures;
extern crate itertools;
#[macro_use]
extern crate sentry;
extern crate graph;
extern crate graph_core;
extern crate graph_datasource_ethereum;
extern crate graph_runtime_wasm;
extern crate graph_server_http;
extern crate graph_server_json_rpc;
extern crate graph_server_websocket;
extern crate graph_store_postgres;
extern crate http;
extern crate ipfs_api;
extern crate lazy_static;
extern crate url;

use clap::{App, Arg};
use futures::sync::mpsc;
use git_testament::{git_testament, render_testament};
use lazy_static::lazy_static;
use std::env;
use std::str::FromStr;
use std::time::Duration;

use graph::components::forward;
use graph::components::subgraph::{DummyRuntimeHost, DummySubgraphProvider};
use graph::log::logger;
use graph::prelude::{*};
use graph::tokio_executor;
use graph::tokio_timer;
use graph::tokio_timer::timer::Timer;
use graph::util::security::SafeDisplay;
use graph_core::{SubgraphInstanceManager};
use graph_datasource_ethereum::{BlockStreamBuilder, Transport};
use graph_server_http::GraphQLServer as GraphQLQueryServer;
use graph_server_websocket::SubscriptionServer as GraphQLSubscriptionServer;
use graph_store_postgres::{Store as DieselStore, StoreConfig};

lazy_static! {
    // Default to an Ethereum reorg threshold to 50 blocks
    static ref REORG_THRESHOLD: u64 = env::var("ETHEREUM_REORG_THRESHOLD")
        .ok()
        .map(|s| u64::from_str(&s)
            .unwrap_or_else(|_| panic!("failed to parse env var ETHEREUM_REORG_THRESHOLD")))
        .unwrap_or(50);

    // Default to an ancestor count of 50 blocks
    static ref ANCESTOR_COUNT: u64 = env::var("ETHEREUM_ANCESTOR_COUNT")
        .ok()
        .map(|s| u64::from_str(&s)
             .unwrap_or_else(|_| panic!("failed to parse env var ETHEREUM_ANCESTOR_COUNT")))
        .unwrap_or(50);

    // Start block to use when indexing Ethereum.
    static ref ETHEREUM_START_BLOCK: u64 = env::var("ETHEREUM_START_BLOCK")
        .ok()
        .map(|s| u64::from_str(&s)
             .unwrap_or_else(|_| panic!("failed to parse env var ETHEREUM_START_BLOCK")))
        .unwrap_or(0);
}

git_testament!(TESTAMENT);

fn main() {
    use std::sync::Mutex;
    use tokio::runtime;

    // Create components for tokio context: multi-threaded runtime, executor
    // context on the runtime, and Timer handle.
    //
    // Configure the runtime to shutdown after a panic.
    let runtime: Arc<Mutex<Option<runtime::Runtime>>> = Arc::new(Mutex::new(None));
    let handler_runtime = runtime.clone();
    *runtime.lock().unwrap() = Some(
        runtime::Builder::new()
            .core_threads(100)
            .panic_handler(move |_| {
                let runtime = handler_runtime.clone();
                std::thread::spawn(move || {
                    if let Some(runtime) = runtime.lock().unwrap().take() {
                        // Try to cleanly shutdown the runtime, but
                        // unconditionally exit after a while.
                        std::thread::spawn(|| {
                            std::thread::sleep(Duration::from_millis(3000));
                            std::process::exit(1);
                        });
                        runtime
                            .shutdown_now()
                            .wait()
                            .expect("Failed to shutdown Tokio Runtime");
                        println!("Runtime cleaned up and shutdown successfully");
                    }
                });
            })
            .build()
            .unwrap(),
    );

    let mut executor = runtime.lock().unwrap().as_ref().unwrap().executor();
    let mut enter = tokio_executor::enter()
        .expect("Failed to enter runtime executor, multiple executors at once");
    let timer = Timer::default();
    let timer_handle = timer.handle();

    // Setup runtime context with defaults and run the main application
    tokio_executor::with_default(&mut executor, &mut enter, |enter| {
        tokio_timer::with_default(&timer_handle, enter, |enter| {
            enter
                .block_on(future::lazy(|| async_main()))
                .expect("Failed to run main function");
        })
    });
}

fn async_main() -> impl Future<Item = (), Error = ()> + Send + 'static {
    env_logger::init();
    // Setup CLI using Clap, provide general info and capture postgres url
    let matches = App::new("graph-node")
        .version("0.1.0")
        .author("Graph Protocol, Inc.")
        .about("Scalable queries for a decentralized future")
        .arg(
            Arg::with_name("subgraph")
                .takes_value(true)
                .long("subgraph")
                .value_name("[NAME:]IPFS_HASH")
                .help("name and IPFS hash of the subgraph manifest"),
        )
        .arg(
            Arg::with_name("postgres-url")
                .takes_value(true)
                .required(true)
                .long("postgres-url")
                .value_name("URL")
                .help("Location of the Postgres database used for storing entities"),
        )
        .arg(
            Arg::with_name("ethereum-rpc")
                .takes_value(true)
                .required_unless_one(&["ethereum-ws", "ethereum-ipc"])
                .conflicts_with_all(&["ethereum-ws", "ethereum-ipc"])
                .long("ethereum-rpc")
                .value_name("NETWORK_NAME:URL")
                .help(
                    "Ethereum network name (e.g. 'mainnet') and \
                     Ethereum RPC URL, separated by a ':'",
                ),
        )
        .arg(
            Arg::with_name("ethereum-ws")
                .takes_value(true)
                .required_unless_one(&["ethereum-rpc", "ethereum-ipc"])
                .conflicts_with_all(&["ethereum-rpc", "ethereum-ipc"])
                .long("ethereum-ws")
                .value_name("NETWORK_NAME:URL")
                .help(
                    "Ethereum network name (e.g. 'mainnet') and \
                     Ethereum WebSocket URL, separated by a ':'",
                ),
        )
        .arg(
            Arg::with_name("ethereum-ipc")
                .takes_value(true)
                .required_unless_one(&["ethereum-rpc", "ethereum-ws"])
                .conflicts_with_all(&["ethereum-rpc", "ethereum-ws"])
                .long("ethereum-ipc")
                .value_name("NETWORK_NAME:FILE")
                .help(
                    "Ethereum network name (e.g. 'mainnet') and \
                     Ethereum IPC pipe, separated by a ':'",
                ),
        )
        .arg(
            Arg::with_name("ipfs")
                .takes_value(true)
                .required(true)
                .long("ipfs")
                .value_name("HOST:PORT")
                .help("HTTP address of an IPFS node"),
        )
        .arg(
            Arg::with_name("http-port")
                .default_value("8000")
                .long("http-port")
                .value_name("PORT")
                .help("Port for the GraphQL HTTP server"),
        )
        .arg(
            Arg::with_name("ws-port")
                .default_value("8001")
                .long("ws-port")
                .value_name("PORT")
                .help("Port for the GraphQL WebSocket server"),
        )
        .arg(
            Arg::with_name("admin-port")
                .default_value("8020")
                .long("admin-port")
                .value_name("PORT")
                .help("Port for the JSON-RPC admin server"),
        )
        .arg(
            Arg::with_name("node-id")
                .default_value("default")
                .long("node-id")
                .value_name("NODE_ID")
                .help("a unique identifier for this node"),
        )
        .arg(
            Arg::with_name("debug")
                .long("debug")
                .help("Enable debug logging"),
        )
        .arg(
            Arg::with_name("elasticsearch-url")
                .long("elasticsearch-url")
                .value_name("URL")
                .env("ELASTICSEARCH_URL")
                .help("Elasticsearch service to write subgraph logs to"),
        )
        .arg(
            Arg::with_name("elasticsearch-user")
                .long("elasticsearch-user")
                .value_name("USER")
                .env("ELASTICSEARCH_USER")
                .help("User to use for Elasticsearch logging"),
        )
        .arg(
            Arg::with_name("elasticsearch-password")
                .long("elasticsearch-password")
                .value_name("PASSWORD")
                .env("ELASTICSEARCH_PASSWORD")
                .hide_env_values(true)
                .help("Password to use for Elasticsearch logging"),
        )
        .arg(
            Arg::with_name("ethereum-polling-interval")
                .long("ethereum-polling-interval")
                .value_name("MILLISECONDS")
                .default_value("500")
                .env("ETHEREUM_POLLING_INTERVAL")
                .help("How often to poll the Ethereum node for new blocks"),
        )
        .get_matches();

    // Set up logger
    let logger = logger(matches.is_present("debug"));

    // Log version information
    info!(
        logger,
        "Graph Node version: {}",
        render_testament!(TESTAMENT)
    );

    // Safe to unwrap because a value is required by CLI
    let postgres_url = matches.value_of("postgres-url").unwrap().to_string();

    let node_id = NodeId::new(matches.value_of("node-id").unwrap())
        .expect("Node ID must contain only a-z, A-Z, 0-9, and '_'");

    // Obtain the Ethereum parameters
    let ethereum_rpc = matches.value_of("ethereum-rpc");
    let ethereum_ipc = matches.value_of("ethereum-ipc");
    let ethereum_ws = matches.value_of("ethereum-ws");

    let block_polling_interval = Duration::from_millis(
        matches
            .value_of("ethereum-polling-interval")
            .unwrap()
            .parse()
            .expect("Ethereum polling interval must be a nonnegative integer"),
    );

    debug!(logger, "Setting up Sentry");

    // Set up Sentry, with release tracking and panic handling;
    // fall back to an empty URL, which will result in no errors being reported
    let sentry_url = env::var_os("THEGRAPH_SENTRY_URL").unwrap_or_else(|| "".into());
    let _sentry = sentry::init((
        sentry_url,
        sentry::ClientOptions {
            release: sentry_crate_release!(),
            ..Default::default()
        },
    ));
    sentry::integrations::panic::register_panic_handler();
    info!(logger, "Starting up");


    // Create a component and subgraph logger factory
    let logger_factory = LoggerFactory::new(logger.clone(), None);

    // Parse the Ethereum URL
    let (ethereum_network_name, ethereum_node_url) = parse_ethereum_network_and_node(
        [ethereum_ipc, ethereum_rpc, ethereum_ws]
            .into_iter()
            .filter_map(|x| x.to_owned())
            .next()
            .expect("one of --ethereum-ipc, --ethereum-rpc or --ethereum-ws must be provided"),
    )
    .expect("failed to parse Ethereum connection string");

    // Set up Ethereum transport
    let (transport_event_loop, transport) = ethereum_ipc
        .map(|_| Transport::new_ipc(ethereum_node_url))
        .or_else(|| ethereum_ws.map(|_| Transport::new_ws(ethereum_node_url)))
        .or_else(|| ethereum_rpc.map(|_| Transport::new_rpc(ethereum_node_url)))
        .expect("One of --ethereum-ipc, --ethereum-rpc or --ethereum-ws must be provided");

    // If we drop the event loop the transport will stop working.
    // For now it's fine to just leak it.
    std::mem::forget(transport_event_loop);

    // Warn if the start block is != genesis
    if *ETHEREUM_START_BLOCK > 0 {
        warn!(
            logger,
            "Using {} as the block to start indexing at. \
             This may cause subgraphs to be only indexed partially",
            *ETHEREUM_START_BLOCK,
        );
    }

    // Create Ethereum adapter
    let eth_adapter = Arc::new(graph_datasource_ethereum::EthereumAdapter::new(
        transport,
        *ETHEREUM_START_BLOCK,
    ));

    // Ask Ethereum node for network identifiers
    info!(
        logger, "Connecting to Ethereum...";
        "network" => &ethereum_network_name,
        "node" => SafeDisplay(ethereum_node_url),
    );
    let eth_net_identifiers = match eth_adapter.net_identifiers(&logger).wait() {
        Ok(net) => {
            info!(
                logger, "Connected to Ethereum";
                "network" => &ethereum_network_name,
                "node" => SafeDisplay(ethereum_node_url),
            );
            net
        }
        Err(e) => {
            error!(logger, "Was a valid Ethereum node provided?");
            panic!("Failed to connect to Ethereum node: {}", e);
        }
    };

    // Set up Store
    info!(
        logger,
        "Connecting to Postgres";
        "url" => SafeDisplay(postgres_url.as_str())
    );
    let store = Arc::new(DieselStore::new(
        StoreConfig {
            postgres_url,
            network_name: ethereum_network_name.to_owned(),
            start_block: *ETHEREUM_START_BLOCK,
        },
        &logger,
        eth_net_identifiers,
    ));
    let graphql_runner = Arc::new(graph_core::GraphQlRunner::new(&logger, store.clone()));
    let mut graphql_server = GraphQLQueryServer::new(
        &logger_factory,
        graphql_runner.clone(),
        store.clone(),
        node_id.clone(),
    );
    let mut subscription_server =
        GraphQLSubscriptionServer::new(&logger, graphql_runner.clone(), store.clone());

    if env::var_os("DISABLE_BLOCK_INGESTOR").unwrap_or("".into()) != "true" {
        // BlockIngestor must be configured to keep at least REORG_THRESHOLD ancestors,
        // otherwise BlockStream will not work properly.
        // BlockStream expects the blocks after the reorg threshold to be present in the
        // database.
        assert!(*ANCESTOR_COUNT >= *REORG_THRESHOLD);

        // Create Ethereum block ingestor
        let block_ingestor = graph_datasource_ethereum::BlockIngestor::new(
            store.clone(),
            eth_adapter.clone(),
            *ANCESTOR_COUNT,
            ethereum_network_name.to_string(),
            &logger_factory,
            block_polling_interval,
        )
        .expect("failed to create Ethereum block ingestor");

        // Run the Ethereum block ingestor in the background
        tokio::spawn(block_ingestor.into_polling_stream());
    }

    // Prepare a block stream builder for subgraphs
    let block_stream_builder = BlockStreamBuilder::new(
        store.clone(),
        store.clone(),
        eth_adapter.clone(),
        node_id.clone(),
        *REORG_THRESHOLD,
    );

    let subgraph_instance_manager = SubgraphInstanceManager::new(
        &logger_factory,
        store.clone(),
        DummyRuntimeHost {},
        block_stream_builder,
    );

    use graphql_parser::schema::Document;
    use graph::data::subgraph::{BaseSubgraphManifest};
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
        data_sources: vec![],
    };
    /*
    graph_core::subgraph::registrar::create_subgraph(
        &logger, 
        store.clone(),
        SubgraphName::new("subgraph").unwrap()
    ).unwrap();
    */
    graph_core::subgraph::registrar::create_subgraph_version(
        &logger,
        store.clone(),
        store.clone(), 
        SubgraphName::new("subgraph").unwrap(),
        manifest,
        NodeId::new("nodeId").unwrap(),
        SubgraphVersionSwitchingMode::Instant
    ).unwrap();

    let mut subgraph_provider = DummySubgraphProvider::new();

    // Forward subgraph events from the subgraph provider to the subgraph instance manager
    tokio::spawn(forward(&mut subgraph_provider, &subgraph_instance_manager).unwrap());

    // Check version switching mode environment variable
    let version_switching_mode = SubgraphVersionSwitchingMode::parse(
        env::var_os("EXPERIMENTAL_SUBGRAPH_VERSION_SWITCHING_MODE")
            .unwrap_or_else(|| "instant".into())
            .to_str()
            .expect("invalid version switching mode"),
    );

    // Periodically check for contention in the tokio threadpool. First spawn a
    // task that simply responds to "ping" requests. Then spawn a separate
    // thread to periodically ping it and check responsiveness.
    let (ping_send, ping_receive) = mpsc::channel::<crossbeam_channel::Sender<()>>(1);
    tokio::spawn(
        ping_receive
            .for_each(move |pong_send| pong_send.clone().send(()).map(|_| ()).map_err(|_| ())),
    );
    let contention_logger = logger.clone();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(100));
        let (pong_send, pong_receive) = crossbeam_channel::bounded(1);
        if ping_send.clone().send(pong_send).wait().is_err() {
            debug!(contention_logger, "Shutting down contention checker thread");
            break;
        }
        let mut timeout = Duration::from_millis(1);
        while pong_receive.recv_timeout(timeout)
            == Err(crossbeam_channel::RecvTimeoutError::Timeout)
        {
            warn!(contention_logger, "Possible contention in tokio threadpool";
                                     "timeout_ms" => timeout.as_millis(),
                                     "code" => LogCode::TokioContention);
            if timeout < Duration::from_secs(10) {
                timeout *= 10;
            }
        }
    });

    future::empty()
}

/// Parses an Ethereum connection string and returns the network name and Ethereum node.
fn parse_ethereum_network_and_node(s: &str) -> Result<(&str, &str), Error> {
    // Check for common Ethereum node mistakes
    if s.starts_with("wss://") || s.starts_with("http://") || s.starts_with("https://") {
        return Err(format_err!(
            "Is your Ethereum node string missing a network name? \
             Try 'mainnet:' + the Ethereum node URL."
        ));
    }

    // Parse string (format is "NETWORK_NAME:URL")
    let split_at = s.find(':').ok_or_else(|| {
        format_err!(
            "A network name must be provided alongside the \
             Ethereum node location. Try e.g. 'mainnet:URL'."
        )
    })?;
    let (name, loc_with_delim) = s.split_at(split_at);
    let loc = &loc_with_delim[1..];

    if name.is_empty() {
        return Err(format_err!(
            "Ethereum network name cannot be an empty string"
        ));
    }

    if loc.is_empty() {
        return Err(format_err!("Ethereum node URL cannot be an empty string"));
    }

    Ok((name, loc))
}
