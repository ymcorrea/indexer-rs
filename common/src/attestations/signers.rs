// Copyright 2023-, Edge & Node, GraphOps, and Semiotic Labs.
// SPDX-License-Identifier: Apache-2.0

use eventuals::{Eventual, EventualExt};
use std::collections::HashMap;
use std::sync::Arc;
use thegraph_core::{Address, ChainId};
use tokio::{
    select,
    sync::{
        watch::{self, Receiver},
        Mutex,
    },
};
use tracing::warn;

use crate::prelude::{Allocation, AttestationSigner};

/// An always up-to-date list of attestation signers, one for each of the indexer's allocations.
pub async fn attestation_signers(
    indexer_allocations: Eventual<HashMap<Address, Allocation>>,
    indexer_mnemonic: String,
    chain_id: ChainId,
    mut dispute_manager_rx: Receiver<Option<Address>>,
) -> Receiver<HashMap<Address, AttestationSigner>> {
    let attestation_signers_map: &'static Mutex<HashMap<Address, AttestationSigner>> =
        Box::leak(Box::new(Mutex::new(HashMap::new())));

    // Actively listening to indexer_allocations to update allocations channel
    // Temporary fix until the indexer_allocations is migrated to tokio watch
    let (allocations_tx, mut allocations_rx) =
        watch::channel(indexer_allocations.value_immediate().unwrap_or_default());
    indexer_allocations
        .pipe(move |allocatons| {
            allocations_tx
                .send(allocatons)
                .expect("Failed to update allocations channel");
        })
        .forever();

    let starter_signers_map = modify_sigers(
        Arc::new(indexer_mnemonic.clone()),
        chain_id,
        attestation_signers_map,
        allocations_rx.clone(),
        dispute_manager_rx.clone(),
    )
    .await;

    // Whenever the indexer's active or recently closed allocations change, make sure
    // we have attestation signers for all of them.
    let (signers_tx, signers_rx) = watch::channel(starter_signers_map);
    tokio::spawn(async move {
        loop {
            let updated_signers = select! {
                Ok(())= allocations_rx.changed() =>{
                    modify_sigers(
                        Arc::new(indexer_mnemonic.clone()),
                        chain_id,
                        attestation_signers_map,
                        allocations_rx.clone(),
                        dispute_manager_rx.clone(),
                    ).await
                },
                Ok(())= dispute_manager_rx.changed() =>{
                    modify_sigers(
                        Arc::new(indexer_mnemonic.clone()),
                        chain_id,
                        attestation_signers_map,
                        allocations_rx.clone(),
                        dispute_manager_rx.clone()
                    ).await
                },
                else=>{
                    // Something is wrong.
                    panic!("dispute_manager_rx or allocations_rx was dropped");
                }
            };
            signers_tx
                .send(updated_signers)
                .expect("Failed to update signers channel");
        }
    });

    signers_rx
}
async fn modify_sigers(
    indexer_mnemonic: Arc<String>,
    chain_id: ChainId,
    attestation_signers_map: &'static Mutex<HashMap<Address, AttestationSigner>>,
    allocations_rx: Receiver<HashMap<Address, Allocation>>,
    dispute_manager_rx: Receiver<Option<Address>>,
) -> HashMap<thegraph_core::Address, AttestationSigner> {
    let mut signers = attestation_signers_map.lock().await;
    let allocations = allocations_rx.borrow().clone();
    let Some(dispute_manager) = *dispute_manager_rx.borrow() else {
        return signers.clone();
    };
    // Remove signers for allocations that are no longer active or recently closed
    signers.retain(|id, _| allocations.contains_key(id));

    // Create signers for new allocations
    for (id, allocation) in allocations.iter() {
        if !signers.contains_key(id) {
            let signer =
                AttestationSigner::new(&indexer_mnemonic, allocation, chain_id, dispute_manager);
            if let Err(e) = signer {
                warn!(
                    "Failed to establish signer for allocation {}, deployment {}, createdAtEpoch {}: {}",
                    allocation.id, allocation.subgraph_deployment.id,
                    allocation.created_at_epoch, e
                );
            } else {
                signers.insert(*id, signer.unwrap());
            }
        }
    }

    signers.clone()
}

#[cfg(test)]
mod tests {
    use crate::test_vectors::{
        DISPUTE_MANAGER_ADDRESS, INDEXER_ALLOCATIONS, INDEXER_OPERATOR_MNEMONIC,
    };

    use super::*;

    #[tokio::test]
    async fn test_attestation_signers_update_with_allocations() {
        let (mut allocations_writer, allocations) = Eventual::<HashMap<Address, Allocation>>::new();
        let (dispute_manager_tx, dispute_manager_rx) = watch::channel(None);
        dispute_manager_tx
            .send(Some(*DISPUTE_MANAGER_ADDRESS))
            .unwrap();
        let mut signers = attestation_signers(
            allocations,
            (*INDEXER_OPERATOR_MNEMONIC).to_string(),
            1,
            dispute_manager_rx,
        )
        .await;

        // Test that an empty set of allocations leads to an empty set of signers
        allocations_writer.write(HashMap::new());
        signers.changed().await.unwrap();
        let latest_signers = signers.borrow().clone();
        assert_eq!(latest_signers, HashMap::new());

        // Test that writing our set of test allocations results in corresponding signers for all of them
        allocations_writer.write((*INDEXER_ALLOCATIONS).clone());
        signers.changed().await.unwrap();
        let latest_signers = signers.borrow().clone();
        assert_eq!(latest_signers.len(), INDEXER_ALLOCATIONS.len());

        for signer_allocation_id in latest_signers.keys() {
            assert!(INDEXER_ALLOCATIONS
                .keys()
                .any(|allocation_id| signer_allocation_id == allocation_id));
        }
    }
}
