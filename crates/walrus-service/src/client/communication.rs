// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{num::NonZeroU16, sync::Arc};

use anyhow::Result;
use futures::{future::Either, stream::FuturesUnordered, StreamExt};
use rand::rngs::StdRng;
use reqwest::{Client as ReqwestClient, Url};
use tokio::sync::Semaphore;
use tracing::{Level, Span};
use walrus_core::{
    encoding::{EncodingAxis, EncodingConfig, Sliver, SliverPair},
    ensure,
    messages::SignedStorageConfirmation,
    metadata::VerifiedBlobMetadataWithId,
    BlobId,
    Epoch,
    PublicKey,
    ShardIndex,
    Sliver as SliverEnum,
    SliverPairIndex,
};
use walrus_sdk::{client::Client as StorageNodeClient, error::NodeError};
use walrus_sui::types::StorageNode;

use super::{
    config::RequestRateConfig,
    error::{SliverStoreError, StoreError},
    utils::{string_prefix, WeightedResult},
    ClientError,
    ClientErrorKind,
};
use crate::utils::{self, ExponentialBackoff, FutureHelpers};

/// Represents the index of the node in the vector of members of the committee.
pub type NodeIndex = usize;

/// Represents the result of an interaction with a storage node.
///
/// Contains the epoch, the "weight" of the interaction (e.g., the number of shards for which an
/// operation was performed), the storage node that issued it, and the result of the operation.
pub struct NodeResult<T, E>(pub Epoch, pub usize, pub NodeIndex, pub Result<T, E>);

impl<T, E> WeightedResult for NodeResult<T, E> {
    type Inner = T;
    type Error = E;
    fn weight(&self) -> usize {
        self.1
    }
    fn inner_result(&self) -> &Result<Self::Inner, Self::Error> {
        &self.3
    }
    fn take_inner_result(self) -> Result<Self::Inner, Self::Error> {
        self.3
    }
}

pub(crate) struct NodeCommunication<'a> {
    pub node_index: NodeIndex,
    pub epoch: Epoch,
    pub node: &'a StorageNode,
    pub encoding_config: &'a EncodingConfig,
    pub span: Span,
    pub client: StorageNodeClient,
    pub config: RequestRateConfig,
    pub node_connection_limit: Arc<Semaphore>,
    pub global_connection_limit: Arc<Semaphore>,
}

impl<'a> NodeCommunication<'a> {
    /// Creates as new instance of [`NodeCommunication`].
    pub fn new(
        node_index: NodeIndex,
        epoch: Epoch,
        client: &'a ReqwestClient,
        node: &'a StorageNode,
        encoding_config: &'a EncodingConfig,
        config: RequestRateConfig,
        global_connection_limit: Arc<Semaphore>,
    ) -> Result<Self, ClientError> {
        let url = Url::parse(&format!("http://{}", node.network_address)).unwrap();
        let node_connection_limit = Arc::new(Semaphore::new(config.max_node_connections));
        tracing::trace!(
            %node_index,
            %config.max_node_connections,
            "initializing communication with node"
        );
        ensure!(
            !node.shard_ids.is_empty(),
            ClientErrorKind::InvalidConfig.into()
        );
        Ok(Self {
            node_index,
            epoch,
            node,
            encoding_config,
            span: tracing::span!(
                Level::ERROR,
                "node",
                index = node_index,
                epoch,
                pk_prefix = string_prefix(&node.public_key)
            ),
            client: StorageNodeClient::from_url(url, client.clone()),
            config,
            node_connection_limit,
            global_connection_limit,
        })
    }

    /// Returns the number of shards.
    pub fn n_shards(&self) -> NonZeroU16 {
        self.encoding_config.n_shards()
    }

    /// Returns the number of shards owned by the node.
    pub fn n_owned_shards(&self) -> NonZeroU16 {
        NonZeroU16::new(
            self.node
                .shard_ids
                .len()
                .try_into()
                .expect("the number of shards is capped"),
        )
        .expect("each node has >0 shards")
    }

    fn to_node_result<T, E>(&self, weight: usize, result: Result<T, E>) -> NodeResult<T, E> {
        NodeResult(self.epoch, weight, self.node_index, result)
    }

    // Read operations.

    /// Requests the metadata for a blob ID from the node.
    #[tracing::instrument(level = Level::TRACE, parent = &self.span, skip_all)]
    pub async fn retrieve_verified_metadata(
        &self,
        blob_id: &BlobId,
    ) -> NodeResult<VerifiedBlobMetadataWithId, NodeError> {
        tracing::debug!(%blob_id, "retrieving metadata");
        let result = self
            .client
            .get_and_verify_metadata(blob_id, self.encoding_config)
            .await;
        self.to_node_result(self.n_owned_shards().get().into(), result)
    }

    /// Requests a sliver from the storage node, and verifies that it matches the metadata and
    /// encoding config.
    #[tracing::instrument(level = Level::TRACE, parent = &self.span, skip(self, metadata))]
    pub async fn retrieve_verified_sliver<T: EncodingAxis>(
        &self,
        metadata: &VerifiedBlobMetadataWithId,
        shard_index: ShardIndex,
    ) -> NodeResult<Sliver<T>, NodeError>
    where
        Sliver<T>: TryFrom<SliverEnum>,
    {
        tracing::debug!(%shard_index, sliver_type = T::NAME, "retrieving verified sliver");
        let sliver_pair_index = shard_index.to_pair_index(self.n_shards(), metadata.blob_id());
        let sliver = self
            .client
            .get_and_verify_sliver(sliver_pair_index, metadata, self.encoding_config)
            .await;

        // Each sliver is in this case requested individually, so the weight is 1.
        self.to_node_result(1, sliver)
    }

    // Write operations.

    /// Stores metadata and sliver pairs on a node, and requests a storage confirmation.
    ///
    /// Returns a [`NodeResult`], where the weight is the number of shards for which the storage
    /// confirmation was issued.
    #[tracing::instrument(level = Level::TRACE, parent = &self.span, skip_all)]
    pub async fn store_metadata_and_pairs(
        &self,
        metadata: &VerifiedBlobMetadataWithId,
        pairs: impl IntoIterator<Item = &SliverPair>,
    ) -> NodeResult<SignedStorageConfirmation, StoreError> {
        tracing::debug!("storing metadata and sliver pairs",);
        let result = async {
            self.store_metadata_with_retries(metadata)
                .await
                .map_err(StoreError::Metadata)?;

            self.store_pairs(metadata.blob_id(), pairs).await?;

            self.client
                .get_and_verify_confirmation(metadata.blob_id(), self.epoch, self.public_key())
                .await
                .map_err(StoreError::Confirmation)
        }
        .await;

        self.to_node_result(self.n_owned_shards().get().into(), result)
    }

    async fn store_metadata_with_retries(
        &self,
        metadata: &VerifiedBlobMetadataWithId,
    ) -> Result<(), NodeError> {
        utils::retry(self.backoff_strategy(), || {
            self.client
                .store_metadata(metadata)
                // TODO(giac): consider adding timeouts and replace the Reqwest timeout.
                .batch_limit(self.global_connection_limit.clone())
                .batch_limit(self.node_connection_limit.clone())
        })
        .await
    }

    /// Stores the sliver pairs on the node.
    ///
    /// Internally retries to store each of the slivers according to the `backoff_strategy`. If
    /// after `max_reties` a sliver cannot be stored, the function returns a [`SliverStoreError`]
    /// and terminates.
    async fn store_pairs(
        &self,
        blob_id: &BlobId,
        pairs: impl IntoIterator<Item = &SliverPair>,
    ) -> Result<(), SliverStoreError> {
        let mut requests = pairs
            .into_iter()
            .flat_map(|pair| {
                vec![
                    Either::Left(self.store_sliver(blob_id, &pair.primary, pair.index())),
                    Either::Right(self.store_sliver(blob_id, &pair.secondary, pair.index())),
                ]
            })
            .collect::<FuturesUnordered<_>>();

        let n_slivers = requests.len();

        while let Some(result) = requests.next().await {
            if let Err(error) = result {
                tracing::warn!(
                    node_permits=?self.node_connection_limit.available_permits(),
                    global_permits=?self.global_connection_limit.available_permits(),
                    ?error,
                    ?self.config.max_retries,
                    "could not store sliver after retrying; stopping storing on the node"
                );
                return Err(error);
            }
            tracing::trace!(
                node_permits=?self.node_connection_limit.available_permits(),
                global_permits=?self.global_connection_limit.available_permits(),
                progress = format!("{}/{}", n_slivers - requests.len(), n_slivers),
                "sliver stored"
            );
        }
        Ok(())
    }

    /// Stores a sliver on a node.
    async fn store_sliver<T: EncodingAxis>(
        &self,
        blob_id: &BlobId,
        sliver: &Sliver<T>,
        pair_index: SliverPairIndex,
    ) -> Result<(), SliverStoreError> {
        utils::retry(self.backoff_strategy(), || {
            self.client
                .store_sliver_by_axis(blob_id, pair_index, sliver)
                // Ordering matters here. Since we don't want to block global connections while we
                // wait for local connections, the innermost limit must be the global one.
                .batch_limit(self.global_connection_limit.clone())
                .batch_limit(self.node_connection_limit.clone())
        })
        .await
        .map_err(|error| SliverStoreError {
            pair_index,
            sliver_type: T::sliver_type(),
            error,
        })
    }

    /// Gets the backoff strategy for the node.
    fn backoff_strategy(&self) -> ExponentialBackoff<StdRng> {
        ExponentialBackoff::new_with_seed(
            self.config.min_backoff,
            self.config.max_backoff,
            self.config.max_retries,
            self.node_index as u64,
        )
    }

    // Verification flows.

    /// Converts the public key of the node.
    fn public_key(&self) -> &PublicKey {
        &self.node.public_key
    }
}
