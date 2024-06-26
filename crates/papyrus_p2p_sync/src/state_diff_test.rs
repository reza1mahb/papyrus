use std::time::Duration;

use assert_matches::assert_matches;
use futures::future::ready;
use futures::{FutureExt, SinkExt, StreamExt};
use indexmap::{indexmap, IndexMap};
use papyrus_network::{DataType, Direction, Query, SignedBlockHeader};
use papyrus_storage::state::StateStorageReader;
use rand::RngCore;
use starknet_api::block::{BlockHeader, BlockNumber};
use starknet_api::core::{ClassHash, CompiledClassHash, ContractAddress, Nonce};
use starknet_api::hash::{StarkFelt, StarkHash};
use starknet_api::state::{StorageKey, ThinStateDiff};
use static_assertions::const_assert;
use test_utils::get_rng;

use crate::test_utils::{
    create_block_hashes_and_signatures,
    setup,
    HEADER_QUERY_LENGTH,
    SLEEP_DURATION_TO_LET_SYNC_ADVANCE,
    STATE_DIFF_QUERY_LENGTH,
};
use crate::P2PSyncError;

const TIMEOUT_FOR_TEST: Duration = Duration::from_secs(5);

fn create_random_state_diff(rng: &mut impl RngCore) -> ThinStateDiff {
    let contract0 = ContractAddress::from(rng.next_u64());
    let contract1 = ContractAddress::from(rng.next_u64());
    let contract2 = ContractAddress::from(rng.next_u64());
    let class_hash = ClassHash(rng.next_u64().into());
    let compiled_class_hash = CompiledClassHash(rng.next_u64().into());
    let deprecated_class_hash = ClassHash(rng.next_u64().into());
    ThinStateDiff {
        deployed_contracts: indexmap! {
            contract0 => class_hash, contract1 => class_hash, contract2 => deprecated_class_hash
        },
        storage_diffs: indexmap! {
            contract0 => indexmap! {
                1u64.into() => StarkFelt::ONE, 2u64.into() => StarkFelt::TWO
            },
            contract1 => indexmap! {
                3u64.into() => StarkFelt::TWO, 4u64.into() => StarkFelt::ONE
            },
        },
        declared_classes: indexmap! { class_hash => compiled_class_hash },
        deprecated_declared_classes: vec![deprecated_class_hash],
        nonces: indexmap! {
            contract0 => Nonce(StarkFelt::ONE), contract2 => Nonce(StarkFelt::TWO)
        },
        replaced_classes: Default::default(),
    }
}

fn split_state_diff(state_diff: ThinStateDiff) -> Vec<ThinStateDiff> {
    let mut result = Vec::new();
    if !state_diff.deployed_contracts.is_empty() {
        result.push(ThinStateDiff {
            deployed_contracts: state_diff.deployed_contracts,
            ..Default::default()
        })
    }
    if !state_diff.storage_diffs.is_empty() {
        result.push(ThinStateDiff { storage_diffs: state_diff.storage_diffs, ..Default::default() })
    }
    if !state_diff.declared_classes.is_empty() {
        result.push(ThinStateDiff {
            declared_classes: state_diff.declared_classes,
            ..Default::default()
        })
    }
    if !state_diff.deprecated_declared_classes.is_empty() {
        result.push(ThinStateDiff {
            deprecated_declared_classes: state_diff.deprecated_declared_classes,
            ..Default::default()
        })
    }
    if !state_diff.nonces.is_empty() {
        result.push(ThinStateDiff { nonces: state_diff.nonces, ..Default::default() })
    }
    if !state_diff.replaced_classes.is_empty() {
        result.push(ThinStateDiff {
            replaced_classes: state_diff.replaced_classes,
            ..Default::default()
        })
    }
    result
}

#[tokio::test]
async fn state_diff_basic_flow() {
    // Asserting the constants so the test can assume there will be 2 state diff queries for a
    // single header query and the second will be smaller than the first.
    const_assert!(STATE_DIFF_QUERY_LENGTH < HEADER_QUERY_LENGTH);
    const_assert!(HEADER_QUERY_LENGTH < 2 * STATE_DIFF_QUERY_LENGTH);

    let (
        p2p_sync,
        storage_reader,
        query_receiver,
        mut signed_headers_sender,
        mut state_diffs_sender,
    ) = setup();

    let block_hashes_and_signatures =
        create_block_hashes_and_signatures(HEADER_QUERY_LENGTH.try_into().unwrap());
    let mut rng = get_rng();
    let state_diffs =
        (0..HEADER_QUERY_LENGTH).map(|_| create_random_state_diff(&mut rng)).collect::<Vec<_>>();

    // We don't need to read the header query in order to know which headers to send, and we
    // already validate the header query in a different test.
    let mut query_receiver =
        query_receiver.filter(|query| ready(matches!(query.data_type, DataType::StateDiff)));

    // Create a future that will receive queries, send responses and validate the results.
    let parse_queries_future = async move {
        // We wait for the state diff sync to see that there are no headers and start sleeping
        tokio::time::sleep(SLEEP_DURATION_TO_LET_SYNC_ADVANCE).await;

        // Check that before we send headers there is no state diff query.
        assert!(query_receiver.next().now_or_never().is_none());

        // Send headers for entire query.
        for (i, ((block_hash, block_signature), state_diff)) in
            block_hashes_and_signatures.iter().zip(state_diffs.iter()).enumerate()
        {
            // Send responses
            signed_headers_sender
                .send(Some(SignedBlockHeader {
                    block_header: BlockHeader {
                        block_number: BlockNumber(i.try_into().unwrap()),
                        block_hash: *block_hash,
                        state_diff_length: Some(state_diff.len()),
                        ..Default::default()
                    },
                    signatures: vec![*block_signature],
                }))
                .await
                .unwrap();
        }
        for (start_block_number, num_blocks) in [
            (0u64, STATE_DIFF_QUERY_LENGTH),
            (
                STATE_DIFF_QUERY_LENGTH.try_into().unwrap(),
                HEADER_QUERY_LENGTH - STATE_DIFF_QUERY_LENGTH,
            ),
        ] {
            // Get a state diff query and validate it
            let query = query_receiver.next().await.unwrap();
            assert_eq!(
                query,
                Query {
                    start_block: BlockNumber(start_block_number),
                    direction: Direction::Forward,
                    limit: num_blocks,
                    step: 1,
                    data_type: DataType::StateDiff,
                }
            );

            for block_number in
                start_block_number..(start_block_number + u64::try_from(num_blocks).unwrap())
            {
                let expected_state_diff: &ThinStateDiff =
                    &state_diffs[usize::try_from(block_number).unwrap()];
                let state_diff_parts = split_state_diff(expected_state_diff.clone());

                let block_number = BlockNumber(block_number);
                for state_diff_part in state_diff_parts {
                    // Check that before we've sent all parts the state diff wasn't written yet.
                    let txn = storage_reader.begin_ro_txn().unwrap();
                    assert_eq!(block_number, txn.get_state_marker().unwrap());

                    state_diffs_sender.send(Some(state_diff_part)).await.unwrap();
                }

                tokio::time::sleep(SLEEP_DURATION_TO_LET_SYNC_ADVANCE).await;

                // Check state diff was written to the storage. This way we make sure that the sync
                // writes to the storage each block's state diff before receiving all query
                // responses.
                let txn = storage_reader.begin_ro_txn().unwrap();
                assert_eq!(block_number.unchecked_next(), txn.get_state_marker().unwrap());
                let state_diff = txn.get_state_diff(block_number).unwrap().unwrap();
                assert_eq!(state_diff, *expected_state_diff);
            }
            state_diffs_sender.send(None).await.unwrap();
        }
    };

    tokio::select! {
        sync_result = p2p_sync.run() => {
            sync_result.unwrap();
            panic!("P2P sync aborted with no failure.");
        }
        _ = parse_queries_future => {}
    }
}

async fn validate_state_diff_fails(
    state_diff_length_in_header: usize,
    state_diff_parts: Vec<Option<ThinStateDiff>>,
    error_validator: impl Fn(P2PSyncError),
) {
    let (
        p2p_sync,
        storage_reader,
        query_receiver,
        mut signed_headers_sender,
        mut state_diffs_sender,
    ) = setup();

    let (block_hash, block_signature) = *create_block_hashes_and_signatures(1).first().unwrap();

    // We don't need to read the header query in order to know which headers to send, and we
    // already validate the header query in a different test.
    let mut query_receiver =
        query_receiver.filter(|query| ready(matches!(query.data_type, DataType::StateDiff)));

    // Create a future that will receive queries, send responses and validate the results.
    let parse_queries_future = async move {
        // Send a single header. There's no need to fill the entire query.
        signed_headers_sender
            .send(Some(SignedBlockHeader {
                block_header: BlockHeader {
                    block_number: BlockNumber(0),
                    block_hash,
                    state_diff_length: Some(state_diff_length_in_header),
                    ..Default::default()
                },
                signatures: vec![block_signature],
            }))
            .await
            .unwrap();

        // Get a state diff query and validate it
        let query = query_receiver.next().await.unwrap();
        assert_eq!(
            query,
            Query {
                start_block: BlockNumber(0),
                direction: Direction::Forward,
                limit: 1,
                step: 1,
                data_type: DataType::StateDiff,
            }
        );

        // Send state diffs.
        for state_diff_part in state_diff_parts {
            // Check that before we've sent all parts the state diff wasn't written yet.
            let txn = storage_reader.begin_ro_txn().unwrap();
            assert_eq!(0, txn.get_state_marker().unwrap().0);

            state_diffs_sender.send(state_diff_part).await.unwrap();
        }
        tokio::time::sleep(TIMEOUT_FOR_TEST).await;
        panic!("P2P sync did not receive error");
    };

    tokio::select! {
        sync_result = p2p_sync.run() => {
            let sync_err = sync_result.unwrap_err();
            error_validator(sync_err);
        }
        _ = parse_queries_future => {}
    }
}

#[tokio::test]
async fn state_diff_empty_state_diff() {
    validate_state_diff_fails(1, vec![Some(ThinStateDiff::default())], |error| {
        assert_matches!(error, P2PSyncError::EmptyStateDiffPart)
    })
    .await;

    validate_state_diff_fails(
        1,
        vec![Some(ThinStateDiff {
            storage_diffs: indexmap! {ContractAddress::default() => IndexMap::default()},
            ..Default::default()
        })],
        |error| assert_matches!(error, P2PSyncError::EmptyStateDiffPart),
    )
    .await;
}

#[tokio::test]
async fn state_diff_stopped_in_middle() {
    validate_state_diff_fails(
        2,
        vec![
            Some(ThinStateDiff {
                deprecated_declared_classes: vec![ClassHash::default()],
                ..Default::default()
            }),
            None,
        ],
        |error| assert_matches!(error, P2PSyncError::WrongStateDiffLength { expected_length, possible_lengths } if expected_length == 2 && possible_lengths == vec![1]),
    )
    .await;
}

#[tokio::test]
async fn state_diff_not_splitted_correctly() {
    validate_state_diff_fails(
        2,
        vec![
            Some(ThinStateDiff {
                deprecated_declared_classes: vec![ClassHash::default()],
                ..Default::default()
            }),
            Some(ThinStateDiff {
                deprecated_declared_classes: vec![
                    ClassHash(StarkHash::ONE), ClassHash(StarkHash::TWO)
                ],
                ..Default::default()
            }),
        ],
        |error| assert_matches!(error, P2PSyncError::WrongStateDiffLength { expected_length, possible_lengths } if expected_length == 2 && possible_lengths == vec![1, 3]),
    )
    .await;
}

#[tokio::test]
async fn state_diff_conflicting() {
    validate_state_diff_fails(
        2,
        vec![
            Some(ThinStateDiff {
                deployed_contracts: indexmap! { ContractAddress::default() => ClassHash::default() },
                ..Default::default()
            }),
            Some(ThinStateDiff {
                deployed_contracts: indexmap! { ContractAddress::default() => ClassHash::default() },
                ..Default::default()
            }),
        ],
        |error| assert_matches!(error, P2PSyncError::ConflictingStateDiffParts),
    )
    .await;
    validate_state_diff_fails(
        2,
        vec![
            Some(ThinStateDiff {
                storage_diffs: indexmap! { ContractAddress::default() => indexmap! {
                    StorageKey::default() => StarkFelt::default()
                }},
                ..Default::default()
            }),
            Some(ThinStateDiff {
                storage_diffs: indexmap! { ContractAddress::default() => indexmap! {
                    StorageKey::default() => StarkFelt::default()
                }},
                ..Default::default()
            }),
        ],
        |error| assert_matches!(error, P2PSyncError::ConflictingStateDiffParts),
    )
    .await;
    validate_state_diff_fails(
        2,
        vec![
            Some(ThinStateDiff {
                declared_classes: indexmap! {
                    ClassHash::default() => CompiledClassHash::default()
                },
                ..Default::default()
            }),
            Some(ThinStateDiff {
                declared_classes: indexmap! {
                    ClassHash::default() => CompiledClassHash::default()
                },
                ..Default::default()
            }),
        ],
        |error| assert_matches!(error, P2PSyncError::ConflictingStateDiffParts),
    )
    .await;
    validate_state_diff_fails(
        2,
        vec![
            Some(ThinStateDiff {
                deprecated_declared_classes: vec![ClassHash::default()],
                ..Default::default()
            }),
            Some(ThinStateDiff {
                deprecated_declared_classes: vec![ClassHash::default()],
                ..Default::default()
            }),
        ],
        |error| assert_matches!(error, P2PSyncError::ConflictingStateDiffParts),
    )
    .await;
    validate_state_diff_fails(
        2,
        vec![
            Some(ThinStateDiff {
                nonces: indexmap! { ContractAddress::default() => Nonce::default() },
                ..Default::default()
            }),
            Some(ThinStateDiff {
                nonces: indexmap! { ContractAddress::default() => Nonce::default() },
                ..Default::default()
            }),
        ],
        |error| assert_matches!(error, P2PSyncError::ConflictingStateDiffParts),
    )
    .await;
    validate_state_diff_fails(
        2,
        vec![
            Some(ThinStateDiff {
                replaced_classes: indexmap! { ContractAddress::default() => ClassHash::default() },
                ..Default::default()
            }),
            Some(ThinStateDiff {
                replaced_classes: indexmap! { ContractAddress::default() => ClassHash::default() },
                ..Default::default()
            }),
        ],
        |error| assert_matches!(error, P2PSyncError::ConflictingStateDiffParts),
    )
    .await;
}
