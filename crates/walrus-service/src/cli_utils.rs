// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Utilities for running the walrus cli tools.

use std::{
    fmt::{self, Display},
    num::NonZeroU16,
    path::PathBuf,
};

use anyhow::{anyhow, Context, Result};
use colored::{ColoredString, Colorize};
use indoc::printdoc;
use prettytable::{format, row, Table};
use sui_sdk::{wallet_context::WalletContext, SuiClientBuilder};
use walrus_core::{
    bft,
    encoding::{
        encoded_blob_length_for_n_shards,
        encoded_slivers_length_for_n_shards,
        max_blob_size_for_n_shards,
        max_sliver_size_for_n_secondary,
        metadata_length_for_n_shards,
        source_symbols_for_n_shards,
    },
};
use walrus_sui::{
    client::{SuiContractClient, SuiReadClient},
    types::Committee,
    utils::storage_units_from_size,
};

use crate::client::{default_configuration_paths, string_prefix, Client, Config};

/// Default URL of the testnet RPC node.
pub const TESTNET_RPC: &str = "https://fullnode.testnet.sui.io:443";
/// Default RPC URL to connect to if none is specified explicitly or in the wallet config.
pub const DEFAULT_RPC_URL: &str = TESTNET_RPC;

/// Returns the path if it is `Some` or any of the default paths if they exist (attempt in order).
pub fn path_or_defaults_if_exist(path: &Option<PathBuf>, defaults: &[PathBuf]) -> Option<PathBuf> {
    let mut path = path.clone();
    for default in defaults {
        if path.is_some() {
            break;
        }
        path = default.exists().then_some(default.clone());
    }
    path
}

/// Loads the wallet context from the given path.
///
/// If no path is provided, tries to load the configuration first from the local folder, and then
/// from the standard Sui configuration directory.
// NB: When making changes to the logic, make sure to update the argument docs in
// `crates/walrus-service/bin/client.rs`.
#[allow(dead_code)]
pub fn load_wallet_context(path: &Option<PathBuf>) -> Result<WalletContext> {
    let mut default_paths = vec!["./client.yaml".into(), "./sui_config.yaml".into()];
    if let Some(home_dir) = home::home_dir() {
        default_paths.push(home_dir.join(".sui").join("sui_config").join("client.yaml"))
    }
    let path = path_or_defaults_if_exist(path, &default_paths)
        .ok_or(anyhow!("Could not find a valid wallet config file."))?;
    tracing::info!("Using wallet configuration from {}", path.display());
    WalletContext::new(&path, None, None)
}

/// Loads the Walrus configuration from the given path.
///
/// If no path is provided, tries to load the configuration first from the local folder, and then
/// from the standard Walrus configuration directory.
// NB: When making changes to the logic, make sure to update the argument docs in
// `crates/walrus-service/bin/client.rs`.
pub fn load_configuration(path: &Option<PathBuf>) -> Result<Config> {
    let path = path_or_defaults_if_exist(path, &default_configuration_paths())
        .ok_or(anyhow!("Could not find a valid Walrus configuration file."))?;
    tracing::info!("Using Walrus configuration from {}", path.display());

    serde_yaml::from_str(&std::fs::read_to_string(&path).context(format!(
        "Unable to read Walrus configuration from {}",
        path.display()
    ))?)
    .context(format!(
        "Parsing Walrus configuration from {} failed",
        path.display()
    ))
}

/// Creates a [`Client`] based on the provided [`Config`] with read-only access to Sui.
///
/// The RPC URL is set based on the `rpc_url` parameter (if `Some`), the `wallet` (if `Ok`) or the
/// default [`DEFAULT_RPC_URL`] if `allow_fallback_to_default` is true.
pub async fn get_read_client(
    config: Config,
    rpc_url: Option<String>,
    wallet: Result<WalletContext>,
    allow_fallback_to_default: bool,
) -> Result<Client<()>> {
    let sui_read_client = get_sui_read_client_from_rpc_node_or_wallet(
        &config,
        rpc_url,
        wallet,
        allow_fallback_to_default,
    )
    .await?;
    Ok(Client::new_read_client(config, &sui_read_client).await?)
}

/// Creates a [`Client<ContractClient>`] based on the provided [`Config`] with write access to Sui.
pub async fn get_contract_client(
    config: Config,
    wallet: Result<WalletContext>,
    gas_budget: u64,
) -> Result<Client<SuiContractClient>> {
    let sui_client =
        SuiContractClient::new(wallet?, config.system_pkg, config.system_object, gas_budget)
            .await?;
    Ok(Client::new(config, sui_client).await?)
}

/// Creates a [`SuiReadClient`] from the provided RPC URL or wallet.
///
/// The RPC URL is set based on the `rpc_url` parameter (if `Some`), the `wallet` (if `Ok`) or the
/// default [`DEFAULT_RPC_URL`] if `allow_fallback_to_default` is true.
// NB: When making changes to the logic, make sure to update the docstring of `get_read_client` and
// the argument docs in `crates/walrus-service/bin/client.rs`.
pub async fn get_sui_read_client_from_rpc_node_or_wallet(
    config: &Config,
    rpc_url: Option<String>,
    wallet: Result<WalletContext>,
    allow_fallback_to_default: bool,
) -> Result<SuiReadClient> {
    tracing::debug!(
        ?rpc_url,
        %allow_fallback_to_default,
        "attempting to create a read client from explicitly set RPC URL, wallet config, or default"
    );
    let sui_client = match rpc_url {
        Some(url) => {
            tracing::info!("Using explicitly set RPC URL {url}");
            SuiClientBuilder::default()
                .build(&url)
                .await
                .context(format!("cannot connect to Sui RPC node at {url}"))
        }
        None => match wallet {
            Ok(wallet) => {
                tracing::info!("Using RPC URL set in wallet configuration");
                wallet
                    .get_client()
                    .await
                    .context("cannot connect to Sui RPC node specified in the wallet configuration")
            }
            Err(e) => {
                if allow_fallback_to_default {
                    tracing::info!("Using default RPC URL {DEFAULT_RPC_URL}");
                    SuiClientBuilder::default()
                        .build(DEFAULT_RPC_URL)
                        .await
                        .context(format!(
                            "cannot connect to Sui RPC node at {DEFAULT_RPC_URL}"
                        ))
                } else {
                    Err(e)
                }
            }
        },
    }?;

    Ok(SuiReadClient::new(sui_client, config.system_pkg, config.system_object).await?)
}

/// Returns the string `Success:` colored in green for terminal output.
pub fn success() -> ColoredString {
    "Success:".bold().green()
}

/// Returns the string `Error:` colored in red for terminal output.
pub fn error() -> ColoredString {
    "Error:".bold().red()
}

/// Type to help with formatting bytes as human-readable strings.
///
/// Formatting of `HumanReadableBytes` works as follows:
///
/// 1. If the value is smaller than 1024, print the value with a ` B` suffix (as we always have
///    an integer number of bytes). Otherwise, follow the next steps.
/// 1. Divide the value by 1024 until we get a *normalized value* in the interval `0..1024`.
/// 1. Round the value (see precision below).
/// 1. Print the normalized value and the unit `B` with an appropriate binary prefix.
///
/// The precision specified in format strings is interpreted differently compared to standard
/// floating-point uses:
///
/// - If the number of digits of the integer part of the normalized value is greater than or
///   equal to the precision, print the integer value.
/// - Else, print the value with the number of significant digits set by the precision.
///
/// A specified precision of `0` is replaced by `1`. The default precision is `3`.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HumanReadableBytes(pub u64);

impl std::fmt::Display for HumanReadableBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const BASE: u64 = 1024;
        const UNITS: [&str; 6] = ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"];
        let value = self.0;

        if value < BASE {
            return write!(f, "{value} B");
        }

        // We know that `value >= 1024`, so `exponent >= 1`.
        let exponent = value.ilog(BASE);
        let normalized_value = value as f64 / BASE.pow(exponent) as f64;
        let unit =
            UNITS[usize::try_from(exponent - 1).expect("we assume at least a 32-bit architecture")];

        // Get correct number of significant digits (not rounding integer part).
        let normalized_integer_digits = normalized_value.log10() as usize + 1;
        let set_precision = f.precision().unwrap_or(3).max(1);
        let precision = if set_precision > normalized_integer_digits {
            set_precision - normalized_integer_digits
        } else {
            0
        };

        write!(f, "{normalized_value:.*} {unit}", precision)
    }
}

/// A human readable representation of a price in MIST.
///
/// [`HumanReadableMist`] is a helper type to format prices in MIST. The formatting works as
/// follows:
///
/// 1. If the price is below 1_000_000 MIST, it is printed fully, with thousands separators.
/// 2. Else, it is printed in SUI with 3 decimal places.
#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HumanReadableMist(pub u64);

impl Display for HumanReadableMist {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let value = self.0;
        if value < 1_000_000 {
            let with_separator = thousands_separator(value);
            return write!(f, "{with_separator} MIST");
        }
        let digits = if value < 10_000_000 { 4 } else { 3 };
        let sui = mist_to_sui(value);
        write!(f, "{sui:.digits$} SUI",)
    }
}

/// Computes the MIST price given the unencoded blob size.
fn mist_price_per_blob_size(
    unencoded_length: u64,
    n_shards: NonZeroU16,
    price_per_unit_size: u64,
) -> Option<u64> {
    encoded_blob_length_for_n_shards(n_shards, unencoded_length)
        .map(|size| storage_units_from_size(size) * price_per_unit_size)
}

fn mist_to_sui(mist: u64) -> f64 {
    mist as f64 / 1e9
}

/// Returns a string representation of the input `num`, with digits grouped in threes by a
/// separator.
fn thousands_separator(num: u64) -> String {
    num.to_string()
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(std::str::from_utf8)
        .collect::<Result<Vec<&str>, _>>()
        .expect("going from utf8 to bytes and back always works")
        .join(",")
}

/// Pretty-prints information on the running Walrus system.
pub fn print_walrus_info(committee: &Committee, price_per_unit_size: u64, dev: bool) {
    let n_shards = committee.n_shards();
    let (n_primary_source_symbols, n_secondary_source_symbols) =
        source_symbols_for_n_shards(n_shards);

    let n_nodes = committee.n_members();
    let max_blob_size = max_blob_size_for_n_shards(n_shards);
    let metadata_length = metadata_length_for_n_shards(n_shards);
    let metadata_price = storage_units_from_size(metadata_length) * price_per_unit_size;

    // NOTE: keep price and text in sync with the changes on in the contracts.
    printdoc!(
        "

        {top_heading}

        {storage_heading}
        Number of nodes: {n_nodes}
        Number of shards: {n_shards}

        {size_heading}
        Maximum blob size: {hr_max_blob} ({max_blob_size_sep} B)

        {price_heading}
        Price per encoded storage unit: {price_per_unit_size} MIST/KiB
        Price to store metadata: {metadata_price}
        Marginal price per additional 1 MiB (w/o metadata): {price_per_mib_input}
        Total price per max blob ({hr_max_blob}): {price_max_blob}
        ",
        top_heading = "Walrus system information".bold(),
        storage_heading = "Storage nodes".bold().green(),
        size_heading = "Blob size".bold().green(),
        hr_max_blob = HumanReadableBytes(max_blob_size),
        max_blob_size_sep = thousands_separator(max_blob_size),
        price_heading = "Approximate storage prices per epoch".bold().green(),
        metadata_price = HumanReadableMist(metadata_price),
        price_per_mib_input = HumanReadableMist(
            storage_units_from_size(
                encoded_slivers_length_for_n_shards(n_shards, 1 << 20,)
                    .expect("we can encode 1 MiB")
            ) * price_per_unit_size
        ),
        price_max_blob = HumanReadableMist(
            mist_price_per_blob_size(max_blob_size, n_shards, price_per_unit_size)
                .expect("we can encode the max blob size")
        )
    );

    if !dev {
        return;
    }

    let max_sliver_size = max_sliver_size_for_n_secondary(n_secondary_source_symbols);
    let max_encoded_blob_size =
        encoded_blob_length_for_n_shards(n_shards, max_blob_size_for_n_shards(n_shards))
            .expect("we can compute the encoded length of the max blob size");
    let f = bft::max_n_faulty(n_shards);
    let (min_nodes_above, shards_above) = committee.min_nodes_above_f();

    printdoc!(
        "

        {encoding_heading}
        Number of primary source symbols: {n_primary_source_symbols}
        Number of secondary source symbols: {n_secondary_source_symbols}
        Metadata size: {hr_metadata} ({metadata_length_sep} B)
        Maximum sliver size: {hr_sliver} ({max_sliver_size_sep} B)
        Maximum encoded blob size: {hr_encoded} ({max_encoded_blob_size_sep} B)

        {bft_heading}
        Tolerated faults (f): {f}
        Quorum threshold (2f+1): {two_f_plus_1}
        Minimum number of correct shards (n-f): {min_correct}
        Minimum number of nodes to get above f: {min_nodes_above} ({shards_above} shards)

        {node_heading}
        ",
        encoding_heading = "(dev) Encoding parameters and sizes".bold().yellow(),
        hr_metadata = HumanReadableBytes(metadata_length),
        metadata_length_sep = thousands_separator(metadata_length),
        hr_sliver = HumanReadableBytes(max_sliver_size),
        max_sliver_size_sep = thousands_separator(max_sliver_size),
        hr_encoded = HumanReadableBytes(max_encoded_blob_size),
        max_encoded_blob_size_sep = thousands_separator(max_encoded_blob_size),
        bft_heading = "(dev) BFT system information".bold().yellow(),
        two_f_plus_1 = 2 * f + 1,
        min_correct = bft::min_n_correct(n_shards),
        node_heading = "(dev) Storage node details and shard distribution"
            .bold()
            .yellow()
    );

    let mut table = Table::new();
    table.set_format(default_table_format());
    table.set_titles(row![b->"Idx", b->"# Shards", b->"Pk prefix", b->"Address"]);

    for (i, node) in committee.members().iter().enumerate() {
        let n_owned = node.shard_ids.len();
        let n_owned_percent = (n_owned as f64) / (committee.n_shards().get() as f64) * 100.0;
        table.add_row(row![
            bFg->format!("{i}"),
            format!("{} ({:.2}%)", n_owned, n_owned_percent),
            string_prefix(&node.public_key),
            node.network_address,
        ]);
    }
    table.printstd();
}

/// Default style for tables printed to stdout.
// TODO: Consider deduplicating with `walrus_orchestrator::display`.
fn default_table_format() -> format::TableFormat {
    format::FormatBuilder::new()
        .separators(
            &[
                format::LinePosition::Top,
                format::LinePosition::Bottom,
                format::LinePosition::Title,
            ],
            format::LineSeparator::new('-', '-', '-', '-'),
        )
        .padding(1, 1)
        .build()
}

#[cfg(test)]
mod tests {
    use walrus_test_utils::param_test;

    use super::*;

    param_test! {
        test_display_without_precision: [
            b_0: (0, "0 B"),
            b_1: (1, "1 B"),
            b_1023: (1023, "1023 B"),
            kib_1: (1024, "1.00 KiB"),
            kib_99: (1024 * 99, "99.0 KiB"),
            kib_100: (1024 * 100, "100 KiB"),
            kib_1023: (1024 * 1023, "1023 KiB"),
            eib_1: (1024_u64.pow(6), "1.00 EiB"),
            u64_max: (u64::MAX, "16.0 EiB"),
        ]
    }
    fn test_display_without_precision(bytes: u64, expected_result: &str) {
        assert_eq!(
            format!("{}", HumanReadableBytes(bytes)),
            expected_result.to_string()
        );
    }

    param_test! {
        test_display_with_explicit_precision: [
            b_0_p0: (0, 0, "0 B"),
            b_1_p0: (1, 0, "1 B"),
            b_1023_p0: (1023, 0, "1023 B"),
            kib_1_p0: (1024, 0, "1 KiB"),
            kib_99_p0: (1024 * 99, 0, "99 KiB"),
            kib_100_p0: (1024 * 100, 0, "100 KiB"),
            kib_1023_p0: (1024 * 1023, 0, "1023 KiB"),
            eib_1_p0: (1024_u64.pow(6), 0, "1 EiB"),
            u64_max_p0: (u64::MAX, 0, "16 EiB"),
            b_1_p1: (1, 1, "1 B"),
            b_1023_p1: (1023, 1, "1023 B"),
            kib_1_p1: (1024, 1, "1 KiB"),
            b_1_p5: (1, 5, "1 B"),
            b_1023_p5: (1023, 5, "1023 B"),
            kib_1_p5: (1024, 5, "1.0000 KiB"),
            b1025_p5: (1025, 5, "1.0010 KiB"),
        ]
    }
    fn test_display_with_explicit_precision(bytes: u64, precision: usize, expected_result: &str) {
        assert_eq!(
            format!("{:.*}", precision, HumanReadableBytes(bytes)),
            expected_result.to_string()
        );
    }

    param_test! {
        test_thousands_separator: [
            thousand: (1_000, "1,000"),
            million: (2_000_000, "2,000,000"),
            hundred_million: (123_456_789, "123,456,789"),
        ]
    }
    fn test_thousands_separator(num: u64, expected: &str) {
        assert_eq!(thousands_separator(num), expected);
    }

    param_test! {
        test_human_readable_mist: [
            ten: (10, "10 MIST"),
            ten_thousand: (10_000, "10,000 MIST"),
            million: (1_000_000, "0.0010 SUI"),
            nine_million: (9_123_456, "0.0091 SUI"),
            ten_million: (10_123_456, "0.010 SUI"),
        ]
    }
    fn test_human_readable_mist(mist: u64, expected: &str) {
        assert_eq!(&format!("{}", HumanReadableMist(mist)), expected,)
    }
}
