use graph::prelude::*;

pub fn validate_manifest(
    manifest: SubgraphManifest,
) -> Result<(String, SubgraphManifest), SubgraphRegistrarError> {
    let mut errors: Vec<SubgraphManifestValidationError> = Vec::new();

    // Validate that the manifest has a `source` address in each data source
    // which has call or block handlers
    let has_invalid_data_source = manifest.data_sources.iter().any(|data_source| {
        let no_source_address = data_source.source.address.is_none();
        let has_call_handlers = data_source
            .mapping
            .call_handlers
            .as_ref()
            .map_or(false, |handlers| !handlers.is_empty());
        let has_block_handlers = data_source
            .mapping
            .block_handlers
            .as_ref()
            .map_or(false, |handlers| handlers.is_empty());

        no_source_address && (has_call_handlers || has_block_handlers)
    });

    if has_invalid_data_source {
        errors.push(SubgraphManifestValidationError::SourceAddressRequired)
    }

    // Validate that there are no more than one of each type of
    // block_handler in each data source.
    let has_too_many_block_handlers = manifest.data_sources.iter().any(|data_source| {
        if data_source
            .mapping
            .block_handlers
            .as_ref()
            .map_or(true, |handlers| handlers.is_empty())
        {
            return false;
        }

        let mut non_filtered_block_handler_count = 0;
        let mut call_filtered_block_handler_count = 0;
        if let Some(ref handlers) = data_source.mapping.block_handlers {
            handlers.iter().for_each(|block_handler| {
                if block_handler.filter.is_none() {
                    non_filtered_block_handler_count += 1
                } else {
                    call_filtered_block_handler_count += 1
                }
            });
        }
        return non_filtered_block_handler_count > 1 || call_filtered_block_handler_count > 1;
    });

    if has_too_many_block_handlers {
        errors.push(SubgraphManifestValidationError::DataSourceBlockHandlerLimitExceeded)
    }

    //    let mut network_name = "none".to_string();
    //    let mut ethereum_networks: Vec<Option<String>> = manifest
    //        .data_sources
    //        .iter()
    //        .cloned()
    //        .filter(|d| d.kind == "ethereum/contract".to_string())
    //        .map(|d| d.network)
    //        .collect();
    //    ethereum_networks.sort();
    //    ethereum_networks.dedup();
    //    match ethereum_networks.len() {
    //        0 => errors.push(SubgraphManifestValidationError::EthereumNetworkRequired),
    //        1 => {
    //            match ethereum_networks.first().and_then(|n| n.clone()) {
    //                Some(n) => network_name = n,
    //                None => errors.push(SubgraphManifestValidationError::EthereumNetworkRequired),
    //            };
    //        }
    //        _ => errors.push(SubgraphManifestValidationError::MultipleEthereumNetworks),
    //    };

    let mut network_name = String::from("none");
    match resolve_network_name(manifest.clone()) {
        Ok(n) => network_name = n,
        Err(e) => errors.push(e),
    };

    //    if let Err(e) = network_name {
    //        error.push(e)
    //    }

    if errors.is_empty() {
        return Ok((network_name, manifest));
    }

    return Err(SubgraphRegistrarError::ManifestValidationError(errors));
}

pub fn resolve_network_name(
    manifest: SubgraphManifest,
) -> Result<String, SubgraphManifestValidationError> {
    let mut ethereum_networks: Vec<Option<String>> = manifest
        .data_sources
        .iter()
        .cloned()
        .filter(|d| d.kind == "ethereum/contract".to_string())
        .map(|d| d.network)
        .collect();
    ethereum_networks.sort();
    ethereum_networks.dedup();
    match ethereum_networks.len() {
        0 => Err(SubgraphManifestValidationError::EthereumNetworkRequired),
        1 => match ethereum_networks.first().and_then(|n| n.clone()) {
            Some(n) => Ok(n),
            None => Err(SubgraphManifestValidationError::EthereumNetworkRequired),
        },
        _ => Err(SubgraphManifestValidationError::MultipleEthereumNetworks),
    }
}
