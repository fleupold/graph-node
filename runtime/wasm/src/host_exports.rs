use crate::MappingContext;
use crate::UnresolvedContractCall;
use ethabi::Token;
use futures::sync::oneshot;
use graph::components::ethereum::*;
use graph::components::store::EntityKey;
use graph::data::store;
use graph::prelude::{slog::b, slog::record_static, *};
use graph::serde_json;
use graph::web3::types::H160;
use semver::Version;
use std::collections::HashMap;
use std::fmt;
use std::ops::Deref;
use std::str::FromStr;
use std::time::{Duration, Instant};

use crate::module::WasmiModule;

pub(crate) const TIMEOUT_ENV_VAR: &str = "GRAPH_MAPPING_HANDLER_TIMEOUT";

pub(crate) trait ExportError: fmt::Debug + fmt::Display + Send + Sync + 'static {}

impl<E> ExportError for E where E: fmt::Debug + fmt::Display + Send + Sync + 'static {}

/// Error raised in host functions.
#[derive(Debug)]
pub(crate) struct HostExportError<E>(pub(crate) E);

impl<E: fmt::Display> fmt::Display for HostExportError<E> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<graph::prelude::Error> for HostExportError<String> {
    fn from(e: graph::prelude::Error) -> Self {
        HostExportError(e.to_string())
    }
}

pub(crate) struct HostExports<E, L, S, U> {
    subgraph_id: SubgraphDeploymentId,
    pub api_version: Version,
    data_source: DataSource,
    ethereum_adapter: Arc<E>,
    link_resolver: Arc<L>,
    store: Arc<S>,
    task_sink: U,
}

impl<E, L, S, U> HostExports<E, L, S, U>
where
    E: EthereumAdapter,
    L: LinkResolver,
    S: Store + Send + Sync,
    U: Sink<SinkItem = Box<Future<Item = (), Error = ()> + Send>> + Clone + Send + Sync + 'static,
{
    pub(crate) fn new(
        subgraph_id: SubgraphDeploymentId,
        api_version: Version,
        data_source: DataSource,
        ethereum_adapter: Arc<E>,
        link_resolver: Arc<L>,
        store: Arc<S>,
        task_sink: U,
    ) -> Self {
        HostExports {
            subgraph_id,
            api_version,
            data_source,
            ethereum_adapter,
            link_resolver,
            store,
            task_sink,
        }
    }

    pub(crate) fn abort(
        &self,
        message: Option<String>,
        file_name: Option<String>,
        line_number: Option<u32>,
        column_number: Option<u32>,
    ) -> Result<(), HostExportError<impl ExportError>> {
        let message = message
            .map(|message| format!("message: {}", message))
            .unwrap_or_else(|| "no message".into());
        let location = match (file_name, line_number, column_number) {
            (None, None, None) => "an unknown location".into(),
            (Some(file_name), None, None) => file_name,
            (Some(file_name), Some(line_number), None) => {
                format!("{}, line {}", file_name, line_number)
            }
            (Some(file_name), Some(line_number), Some(column_number)) => format!(
                "{}, line {}, column {}",
                file_name, line_number, column_number
            ),
            _ => unreachable!(),
        };
        Err(HostExportError(format!(
            "Mapping aborted at {}, with {}",
            location, message
        )))
    }

    pub(crate) fn store_set(
        &self,
        ctx: &mut MappingContext,
        entity_type: String,
        entity_id: String,
        mut data: HashMap<String, Value>,
    ) -> Result<(), HostExportError<impl ExportError>> {
        // Automatically add an "id" value
        match data.insert("id".to_string(), Value::String(entity_id.clone())) {
            Some(ref v) if v != &Value::String(entity_id.clone()) => {
                return Err(HostExportError(format!(
                    "Value of {} attribute 'id' conflicts with ID passed to `store.set()`: \
                     {} != {}",
                    entity_type, v, entity_id,
                )));
            }
            _ => (),
        }

        ctx.state.entity_operations.push(EntityOperation::Set {
            key: EntityKey {
                subgraph_id: self.subgraph_id.clone(),
                entity_type,
                entity_id,
            },
            data: Entity::from(data),
        });

        Ok(())
    }

    pub(crate) fn store_remove(
        &self,
        ctx: &mut MappingContext,
        entity_type: String,
        entity_id: String,
    ) {
        ctx.state.entity_operations.push(EntityOperation::Remove {
            key: EntityKey {
                subgraph_id: self.subgraph_id.clone(),
                entity_type,
                entity_id,
            },
        });
    }

    pub(crate) fn store_get(
        &self,
        ctx: &MappingContext,
        entity_type: String,
        entity_id: String,
    ) -> Result<Option<Entity>, HostExportError<impl ExportError>> {
        let start_time = Instant::now();
        let store_key = EntityKey {
            subgraph_id: self.subgraph_id.clone(),
            entity_type: entity_type.clone(),
            entity_id: entity_id.clone(),
        };

        // Get all operations for this entity
        let matching_operations: Vec<_> = ctx
            .state
            .entity_operations
            .iter()
            .filter(|op| op.matches_entity(&store_key))
            .collect();

        // Shortcut 1: If the latest operation for this entity was a removal,
        // return 0 (= null) to the runtime
        if matching_operations
            .iter()
            .peekable()
            .peek()
            .map(|op| op.is_remove())
            .unwrap_or(false)
        {
            return Ok(None);
        }

        // Shortcut 2: If there is a removal in the operations, the
        // entity will be the result of the operations after that, so we
        // don't have to hit the store for anything
        if matching_operations.iter().any(|op| op.is_remove()) {
            return EntityOperation::apply_all(None, &matching_operations)
                .map_err(QueryExecutionError::StoreError)
                .map_err(HostExportError);
        }

        // No removal in the operations => read the entity from the store, then apply
        // the operations to it to obtain the result
        let result = self
            .store
            .get(store_key)
            .and_then(|entity| {
                EntityOperation::apply_all(entity, &matching_operations)
                    .map_err(QueryExecutionError::StoreError)
            })
            .map_err(HostExportError);
        debug!(ctx.logger, "Store get finished";
               "type" => &entity_type, 
               "id" => &entity_id,
               "time" => format!("{}ms", start_time.elapsed().as_millis()));
        result
    }

    pub(crate) fn ethereum_call(
        &self,
        ctx: &MappingContext,
        unresolved_call: UnresolvedContractCall,
    ) -> Result<Vec<Token>, HostExportError<impl ExportError>> {
        let start_time = Instant::now();

        // Obtain the path to the contract ABI
        let contract = self
            .data_source
            .mapping
            .abis
            .iter()
            .find(|abi| abi.name == unresolved_call.contract_name)
            .ok_or_else(|| {
                HostExportError(format!(
                    "Could not find ABI for contract \"{}\", try adding it to the 'abis' section \
                     of the subgraph manifest",
                    unresolved_call.contract_name
                ))
            })?
            .contract
            .clone();

        let function = contract
            .function(unresolved_call.function_name.as_str())
            .map_err(|e| {
                HostExportError(format!(
                    "Unknown function \"{}::{}\" called from WASM runtime: {}",
                    unresolved_call.contract_name, unresolved_call.function_name, e
                ))
            })?;

        let call = EthereumContractCall {
            address: unresolved_call.contract_address.clone(),
            block_ptr: ctx.block.as_ref().deref().into(),
            function: function.clone(),
            args: unresolved_call.function_args.clone(),
        };

        // Run Ethereum call in tokio runtime
        let function_name = unresolved_call.function_name.clone();
        let contract_name = unresolved_call.contract_name.clone();
        let eth_adapter = self.ethereum_adapter.clone();
        let logger = ctx.logger.clone();
        let result = self.block_on(future::lazy(move || {
            eth_adapter.contract_call(&logger, call).map_err(move |e| {
                HostExportError(format!(
                    "Failed to call function \"{}\" of contract \"{}\": {}",
                    function_name, contract_name, e
                ))
            })
        }));

        debug!(ctx.logger, "Contract call finished";
              "address" => &unresolved_call.contract_address.to_string(),
              "contract" => &unresolved_call.contract_name,
              "function" => &unresolved_call.function_name,
              "time" => format!("{}ms", start_time.elapsed().as_millis()));

        result
    }

    pub(crate) fn bytes_to_string(
        &self,
        bytes: Vec<u8>,
    ) -> Result<String, HostExportError<impl ExportError>> {
        let s = String::from_utf8(bytes).map_err(|e| {
            HostExportError(format!(
                "Failed to parse byte array using `toString()`. This may be caused by attempting \
                 to convert a value such as an address that cannot be parsed to a unicode string. \
                 Try 'toHexString()' instead. Bytes: `{bytes:?}`. Error: {error}",
                error = e.utf8_error(),
                bytes = e.into_bytes(),
            ))
        })?;
        // The string may have been encoded in a fixed length
        // buffer and padded with null characters, so trim
        // trailing nulls.
        Ok(s.trim_end_matches('\u{0000}').to_string())
    }

    /// Converts bytes to a hex string.
    /// References:
    /// https://godoc.org/github.com/ethereum/go-ethereum/common/hexutil#hdr-Encoding_Rules
    /// https://github.com/ethereum/web3.js/blob/f98fe1462625a6c865125fecc9cb6b414f0a5e83/packages/web3-utils/src/utils.js#L283
    pub(crate) fn bytes_to_hex(&self, bytes: Vec<u8>) -> String {
        // Even an empty string must be prefixed with `0x`.
        // Encodes each byte as a two hex digits.
        format!("0x{}", ::hex::encode(bytes))
    }

    pub(crate) fn big_int_to_string(&self, n: BigInt) -> String {
        format!("{}", n)
    }

    /// Prints the module of `n` in hex.
    /// Integers are encoded using the least amount of digits (no leading zero digits).
    /// Their encoding may be of uneven length. The number zero encodes as "0x0".
    ///
    /// https://godoc.org/github.com/ethereum/go-ethereum/common/hexutil#hdr-Encoding_Rules
    pub(crate) fn big_int_to_hex(&self, n: BigInt) -> String {
        if n == 0.into() {
            return "0x0".to_string();
        }

        let bytes = n.to_bytes_be().1;
        format!("0x{}", ::hex::encode(bytes).trim_start_matches('0'))
    }

    pub(crate) fn big_int_to_i32(
        &self,
        n: BigInt,
    ) -> Result<i32, HostExportError<impl ExportError>> {
        if n >= i32::min_value().into() && n <= i32::max_value().into() {
            let n_bytes = n.to_signed_bytes_le();
            let mut i_bytes: [u8; 4] = if n < 0.into() { [255; 4] } else { [0; 4] };
            i_bytes[..n_bytes.len()].copy_from_slice(&n_bytes);
            let i = i32::from_le_bytes(i_bytes);
            Ok(i)
        } else {
            Err(HostExportError(format!(
                "BigInt value does not fit into i32: {}",
                n
            )))
        }
    }

    pub(crate) fn json_from_bytes(
        &self,
        bytes: Vec<u8>,
    ) -> Result<serde_json::Value, HostExportError<impl ExportError>> {
        serde_json::from_reader(&*bytes).map_err(|e| {
            HostExportError(format!(
                "Failed to parse JSON from byte array. Bytes: `{bytes:?}`. Error: {error}",
                bytes = bytes,
                error = e,
            ))
        })
    }

    pub(crate) fn ipfs_cat(
        &self,
        logger: &Logger,
        link: String,
    ) -> Result<Vec<u8>, HostExportError<impl ExportError>> {
        self.block_on(
            self.link_resolver
                .cat(logger, &Link { link })
                .map_err(HostExportError),
        )
    }

    // Read the IPFS file `link`, split it into JSON objects, and invoke
    // the exported function `callback` on each JSON object. The successful
    // return value contains all entity operations that were produced by the
    // callback invocations. Each invocation of `callback` happens in its own
    // instance of a WASM module, which is identical to `module` when it was
    // first started. The signature of the callback must be
    // `callback(JSONValue, Value)`, and the `userData` parameter is passed
    // to the callback without any changes
    pub(crate) fn ipfs_map(
        &self,
        module: &WasmiModule<E, L, S, U>,
        link: String,
        callback: &str,
        user_data: store::Value,
        flags: Vec<String>,
    ) -> Result<Vec<EntityOperation>, HostExportError<impl ExportError>> {
        const JSON_FLAG: &str = "json";
        if !flags.contains(&JSON_FLAG.to_string()) {
            return Err(HostExportError(format!("Flags must contain 'json'")));
        }

        let valid_module = module.valid_module.clone();
        let ctx = module.ctx.clone();
        let callback = callback.to_owned();
        // Create a base error message to avoid borrowing headaches
        let errmsg = format!(
            "ipfs_map: callback '{}' failed when processing file '{}'",
            &*callback, &link
        );

        let start = Instant::now();
        let mut last_log = Instant::now();
        let logger = ctx.logger.new(o!("ipfs_map" => link.clone()));
        let operations = self.block_on(
            self.link_resolver
                .json_stream(&Link { link })
                .and_then(move |stream| {
                    stream
                        .and_then(move |sv| {
                            let module = WasmiModule::from_valid_module_with_ctx(
                                valid_module.clone(),
                                ctx.clone(),
                            )?;
                            let result =
                                module.handle_json_callback(&*callback, &sv.value, &user_data);
                            // Log progress every 15s
                            if last_log.elapsed() > Duration::from_secs(15) {
                                debug!(
                                    logger,
                                    "Processed {} lines in {}s so far",
                                    sv.line,
                                    start.elapsed().as_secs()
                                );
                                last_log = Instant::now();
                            }
                            result
                        })
                        .collect()
                })
                .map_err(move |e| HostExportError(format!("{}: {}", errmsg, e.to_string()))),
        )?;
        // Collect all results into one Vec
        Ok(operations.into_iter().flatten().collect())
    }

    /// Expects a decimal string.
    pub(crate) fn json_to_i64(
        &self,
        json: String,
    ) -> Result<i64, HostExportError<impl ExportError>> {
        i64::from_str(&json)
            .map_err(|_| HostExportError(format!("JSON `{}` cannot be parsed as i64", json)))
    }

    /// Expects a decimal string.
    pub(crate) fn json_to_u64(
        &self,
        json: String,
    ) -> Result<u64, HostExportError<impl ExportError>> {
        u64::from_str(&json)
            .map_err(|_| HostExportError(format!("JSON `{}` cannot be parsed as u64", json)))
    }

    /// Expects a decimal string.
    pub(crate) fn json_to_f64(
        &self,
        json: String,
    ) -> Result<f64, HostExportError<impl ExportError>> {
        f64::from_str(&json)
            .map_err(|_| HostExportError(format!("JSON `{}` cannot be parsed as f64", json)))
    }

    /// Expects a decimal string.
    pub(crate) fn json_to_big_int(
        &self,
        json: String,
    ) -> Result<Vec<u8>, HostExportError<impl ExportError>> {
        let big_int = BigInt::from_str(&json)
            .map_err(|_| HostExportError(format!("JSON `{}` is not a decimal string", json)))?;
        Ok(big_int.to_signed_bytes_le())
    }

    pub(crate) fn crypto_keccak_256(&self, input: Vec<u8>) -> [u8; 32] {
        ::tiny_keccak::keccak256(&input)
    }

    pub(crate) fn big_int_plus(&self, x: BigInt, y: BigInt) -> BigInt {
        x + y
    }

    pub(crate) fn big_int_minus(&self, x: BigInt, y: BigInt) -> BigInt {
        x - y
    }

    pub(crate) fn big_int_times(&self, x: BigInt, y: BigInt) -> BigInt {
        x * y
    }

    pub(crate) fn big_int_divided_by(
        &self,
        x: BigInt,
        y: BigInt,
    ) -> Result<BigInt, HostExportError<impl ExportError>> {
        if y == 0.into() {
            return Err(HostExportError(format!(
                "attempted to divide BigInt `{}` by zero",
                x
            )));
        }
        Ok(x / y)
    }

    pub(crate) fn big_int_mod(&self, x: BigInt, y: BigInt) -> BigInt {
        x % y
    }

    pub(crate) fn block_on<I: Send + 'static, ER: Send + 'static>(
        &self,
        future: impl Future<Item = I, Error = ER> + Send + 'static,
    ) -> Result<I, ER> {
        let (return_sender, return_receiver) = oneshot::channel();
        self.task_sink
            .clone()
            .send(Box::new(future.then(|res| {
                return_sender.send(res).map_err(|_| unreachable!())
            })))
            .wait()
            .map_err(|_| panic!("task receiver dropped"))
            .unwrap();
        return_receiver.wait().expect("`return_sender` dropped")
    }

    pub(crate) fn check_timeout(
        &self,
        start_time: Instant,
    ) -> Result<(), HostExportError<impl ExportError>> {
        let mapping_handler_timeout = std::env::var(TIMEOUT_ENV_VAR)
            .ok()
            .and_then(|s| u64::from_str(&s).ok())
            .map(Duration::from_secs);
        if let Some(timeout) = mapping_handler_timeout {
            if start_time.elapsed() > timeout {
                return Err(HostExportError(format!("Mapping handler timed out")));
            }
        }
        Ok(())
    }

    /// Useful for IPFS hashes stored as bytes
    pub(crate) fn bytes_to_base58(&self, bytes: Vec<u8>) -> String {
        ::bs58::encode(&bytes).into_string()
    }

    pub(crate) fn big_decimal_plus(&self, x: BigDecimal, y: BigDecimal) -> BigDecimal {
        x + y
    }

    pub(crate) fn big_decimal_minus(&self, x: BigDecimal, y: BigDecimal) -> BigDecimal {
        x - y
    }

    pub(crate) fn big_decimal_times(&self, x: BigDecimal, y: BigDecimal) -> BigDecimal {
        x * y
    }

    /// Maximum precision of 100 decimal digits.
    pub(crate) fn big_decimal_divided_by(
        &self,
        x: BigDecimal,
        y: BigDecimal,
    ) -> Result<BigDecimal, HostExportError<impl ExportError>> {
        if y == 0.into() {
            return Err(HostExportError(format!(
                "attempted to divide BigDecimal `{}` by zero",
                x
            )));
        }

        Ok(x / y)
    }

    pub(crate) fn big_decimal_equals(&self, x: BigDecimal, y: BigDecimal) -> bool {
        x == y
    }

    pub(crate) fn big_decimal_to_string(&self, x: BigDecimal) -> String {
        x.to_string()
    }

    pub(crate) fn big_decimal_from_string(
        &self,
        s: String,
    ) -> Result<BigDecimal, HostExportError<impl ExportError>> {
        BigDecimal::from_str(&s)
            .map_err(|e| HostExportError(format!("failed to parse BigDecimal: {}", e)))
    }

    pub(crate) fn data_source_create(
        &self,
        ctx: &mut MappingContext,
        name: String,
        params: Vec<String>,
    ) -> Result<(), HostExportError<impl ExportError>> {
        info!(
            ctx.logger,
            "Create data source";
            "name" => &name,
            "params" => format!("{}", params.join(","))
        );

        // Resolve the name into the right template
        self.data_source
            .templates
            .as_ref()
            .ok_or_else(|| {
                HostExportError(format!(
                    "Failed to create data source from name `{}`. \
                     Parent data source `{}` contains no templates",
                    name, self.data_source.name
                ))
            })?
            .iter()
            .find(|template| template.name == name)
            .ok_or_else(|| {
                HostExportError(format!(
                    "Failed to create data source from name `{}`: \
                     No template with this name in parent data source `{}`. \
                     Available names: {}.",
                    name,
                    self.data_source.name,
                    self.data_source
                        .templates
                        .as_ref()
                        .unwrap()
                        .iter()
                        .map(|template| template.name.clone())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?;

        // Remember that we need to create this data source
        ctx.state.created_data_sources.push(DataSourceTemplateInfo {
            data_source: self.data_source.name.clone(),
            template: name,
            params,
        });

        Ok(())
    }

    pub(crate) fn ens_name_by_hash(
        &self,
        hash: &str,
    ) -> Result<Option<String>, HostExportError<impl ExportError>> {
        self.store.find_ens_name(hash).map_err(HostExportError)
    }

    pub(crate) fn log_log(&self, ctx: &MappingContext, level: slog::Level, msg: String) {
        let rs = record_static!(level, self.data_source.name.as_str());
        ctx.logger.log(&slog::Record::new(
            &rs,
            &format_args!("{}", msg),
            b!("data_source" => &self.data_source.name),
        ));

        if level == slog::Level::Critical {
            panic!("Critical error logged in mapping");
        }
    }
}

pub(crate) fn string_to_h160(string: &str) -> Result<H160, HostExportError<impl ExportError>> {
    // `H160::from_str` takes a hex string with no leading `0x`.
    let string = string.trim_start_matches("0x");
    H160::from_str(string)
        .map_err(|e| HostExportError(format!("Failed to convert string to Address/H160: {}", e)))
}

#[test]
fn test_string_to_h160_with_0x() {
    assert_eq!(
        H160::from_str("A16081F360e3847006dB660bae1c6d1b2e17eC2A").unwrap(),
        string_to_h160("0xA16081F360e3847006dB660bae1c6d1b2e17eC2A").unwrap()
    )
}
