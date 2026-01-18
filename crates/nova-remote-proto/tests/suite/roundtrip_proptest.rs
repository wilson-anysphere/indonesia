use nova_remote_proto::{
    transport, FileText, RpcMessage, ScoredSymbol, ShardIndexInfo, SymbolRankKey, WorkerStats,
};
use proptest::prelude::*;

const MAX_PATH_LEN: usize = 128;
const MAX_TEXT_LEN: usize = 512;
const MAX_FILES: usize = 8;
const MAX_SEARCH_ITEMS: usize = 16;
const MAX_AUTH_TOKEN_LEN: usize = 64;
const MAX_ERROR_LEN: usize = 256;

fn arb_ascii_string(max_len: usize) -> impl Strategy<Value = String> {
    prop::collection::vec(proptest::char::range(' ', '~'), 0..=max_len)
        .prop_map(|chars| chars.into_iter().collect())
}

fn arb_file_text() -> impl Strategy<Value = FileText> {
    (
        arb_ascii_string(MAX_PATH_LEN),
        arb_ascii_string(MAX_TEXT_LEN),
    )
        .prop_map(|(path, text)| FileText { path, text })
}

fn arb_symbol_rank_key() -> impl Strategy<Value = SymbolRankKey> {
    (any::<i32>(), any::<i32>()).prop_map(|(kind_rank, score)| SymbolRankKey { kind_rank, score })
}

fn arb_scored_symbol() -> impl Strategy<Value = ScoredSymbol> {
    (
        arb_ascii_string(64),
        arb_ascii_string(MAX_PATH_LEN),
        arb_symbol_rank_key(),
    )
        .prop_map(|(name, path, rank_key)| ScoredSymbol {
            name,
            path,
            rank_key,
        })
}

fn arb_worker_stats() -> impl Strategy<Value = WorkerStats> {
    (any::<u32>(), any::<u64>(), any::<u64>(), any::<u32>()).prop_map(
        |(shard_id, revision, index_generation, file_count)| WorkerStats {
            shard_id,
            revision,
            index_generation,
            file_count,
        },
    )
}

fn arb_shard_index_info() -> impl Strategy<Value = ShardIndexInfo> {
    (any::<u32>(), any::<u64>(), any::<u64>(), any::<u32>()).prop_map(
        |(shard_id, revision, index_generation, symbol_count)| ShardIndexInfo {
            shard_id,
            revision,
            index_generation,
            symbol_count,
        },
    )
}

fn arb_rpc_message() -> impl Strategy<Value = RpcMessage> {
    prop_oneof![
        // Handshake
        (
            any::<u32>(),
            prop::option::of(arb_ascii_string(MAX_AUTH_TOKEN_LEN)),
            any::<bool>()
        )
            .prop_map(
                |(shard_id, auth_token, has_cached_index)| RpcMessage::WorkerHello {
                    shard_id,
                    auth_token,
                    has_cached_index
                }
            ),
        (any::<u32>(), any::<u32>(), any::<u64>(), any::<u32>()).prop_map(
            |(worker_id, shard_id, revision, protocol_version)| RpcMessage::RouterHello {
                worker_id,
                shard_id,
                revision,
                protocol_version
            }
        ),
        // Commands
        (
            any::<u64>(),
            prop::collection::vec(arb_file_text(), 0..=MAX_FILES)
        )
            .prop_map(|(revision, files)| RpcMessage::LoadFiles { revision, files }),
        (
            any::<u64>(),
            prop::collection::vec(arb_file_text(), 0..=MAX_FILES)
        )
            .prop_map(|(revision, files)| RpcMessage::IndexShard { revision, files }),
        (any::<u64>(), arb_file_text())
            .prop_map(|(revision, file)| RpcMessage::UpdateFile { revision, file }),
        Just(RpcMessage::GetWorkerStats),
        (
            arb_ascii_string(64),
            0u32..=(nova_remote_proto::MAX_SEARCH_RESULTS_PER_MESSAGE as u32),
        )
            .prop_map(|(query, limit)| RpcMessage::SearchSymbols { query, limit }),
        // Responses
        arb_worker_stats().prop_map(RpcMessage::WorkerStats),
        arb_shard_index_info().prop_map(RpcMessage::ShardIndexInfo),
        prop::collection::vec(arb_scored_symbol(), 0..=MAX_SEARCH_ITEMS)
            .prop_map(|items| RpcMessage::SearchSymbolsResult { items }),
        Just(RpcMessage::Ack),
        Just(RpcMessage::Shutdown),
        arb_ascii_string(MAX_ERROR_LEN).prop_map(|message| RpcMessage::Error { message }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        .. ProptestConfig::default()
    })]

    #[test]
    fn rpc_message_roundtrip(msg in arb_rpc_message()) {
        let encoded = nova_remote_proto::encode_message(&msg).unwrap();
        let decoded = nova_remote_proto::decode_message(&encoded).unwrap();
        prop_assert_eq!(decoded, msg);
    }

    #[test]
    fn framed_rpc_message_roundtrip(msg in arb_rpc_message()) {
        let _lock = transport::__TRANSPORT_ENV_LOCK_FOR_TESTS
            .lock()
            .expect("__TRANSPORT_ENV_LOCK_FOR_TESTS mutex poisoned");
        let encoded = transport::encode_framed_message(&msg).unwrap();
        let decoded = transport::decode_framed_message(&encoded).unwrap();
        prop_assert_eq!(decoded, msg);
    }
}
