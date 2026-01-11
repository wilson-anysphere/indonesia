use std::time::{Duration, Instant};

use nova_remote_proto::{
    decode_message, encode_message, FileText, RpcMessage, ScoredSymbol, ShardIndexInfo,
    SymbolRankKey, WorkerStats,
};

#[test]
fn rpc_roundtrips() -> anyhow::Result<()> {
    let msgs = vec![
        RpcMessage::WorkerHello {
            shard_id: 1,
            auth_token: Some("token".into()),
            has_cached_index: true,
        },
        RpcMessage::RouterHello {
            worker_id: 9,
            shard_id: 1,
            revision: 42,
            protocol_version: nova_remote_proto::PROTOCOL_VERSION,
        },
        RpcMessage::LoadFiles {
            revision: 123,
            files: vec![FileText {
                path: "src/Main.java".into(),
                text: "class Main {}".into(),
            }],
        },
        RpcMessage::IndexShard {
            revision: 123,
            files: vec![FileText {
                path: "src/Main.java".into(),
                text: "class Main {}".into(),
            }],
        },
        RpcMessage::UpdateFile {
            revision: 124,
            file: FileText {
                path: "src/Other.java".into(),
                text: "class Other {}".into(),
            },
        },
        RpcMessage::WorkerStats(WorkerStats {
            shard_id: 1,
            revision: 42,
            index_generation: 7,
            file_count: 3,
        }),
        RpcMessage::ShardIndexInfo(ShardIndexInfo {
            shard_id: 1,
            revision: 42,
            index_generation: 7,
            symbol_count: 123,
        }),
        RpcMessage::SearchSymbols {
            query: "foo".into(),
            limit: 10,
        },
        RpcMessage::SearchSymbolsResult {
            items: vec![ScoredSymbol {
                name: "Foo".into(),
                path: "src/Foo.java".into(),
                rank_key: SymbolRankKey {
                    kind_rank: 1,
                    score: 100,
                },
            }],
        },
        RpcMessage::Ack,
        RpcMessage::Shutdown,
        RpcMessage::Error {
            message: "oops".into(),
        },
    ];

    for msg in msgs {
        let bytes = encode_message(&msg)?;
        let decoded = decode_message(&bytes)?;
        assert_eq!(decoded, msg);
    }

    Ok(())
}

#[test]
fn decode_rejects_huge_vec_lengths_without_allocating() {
    // Start with a valid SearchSymbolsResult message then corrupt the items length prefix.
    let msg = RpcMessage::SearchSymbolsResult {
        items: vec![ScoredSymbol {
            name: "Foo".into(),
            path: "Foo.java".into(),
            rank_key: SymbolRankKey {
                kind_rank: 1,
                score: 10,
            },
        }],
    };
    let mut bytes = encode_message(&msg).expect("encode");

    // Layout:
    //   tag: u8 (1)
    //   items_len: u32 (4)  <-- overwrite this
    let items_len_offset = 1;
    bytes[items_len_offset..items_len_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());

    let start = Instant::now();
    assert!(decode_message(&bytes).is_err());
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "decode took too long"
    );

    // Also ensure we reject bogus file counts quickly.
    let msg = RpcMessage::IndexShard {
        revision: 1,
        files: vec![FileText {
            path: "a".into(),
            text: "b".into(),
        }],
    };
    let mut bytes = encode_message(&msg).expect("encode");

    // Layout:
    //   tag: u8 (1)
    //   revision: u64 (8)
    //   files_len: u32 (4)  <-- overwrite this
    let files_len_offset = 1 + 8;
    bytes[files_len_offset..files_len_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());

    let start = Instant::now();
    assert!(decode_message(&bytes).is_err());
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "decode took too long"
    );
}

#[test]
fn decode_rejects_huge_string_lengths_without_allocating() {
    // Start with a valid UpdateFile message then corrupt the file text length prefix.
    let msg = RpcMessage::UpdateFile {
        revision: 1,
        file: FileText {
            path: "a".into(),
            text: "b".into(),
        },
    };
    let mut bytes = encode_message(&msg).expect("encode");

    // Layout:
    //   tag: u8 (1)
    //   revision: u64 (8)
    //   path_len: u32 (4)
    //   path bytes (len)
    //   text_len: u32 (4)  <-- overwrite this
    let path_len_offset = 1 + 8;
    let path_len = u32::from_le_bytes(
        bytes[path_len_offset..path_len_offset + 4]
            .try_into()
            .unwrap(),
    ) as usize;
    let text_len_offset = path_len_offset + 4 + path_len;

    bytes[text_len_offset..text_len_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());

    let start = Instant::now();
    assert!(decode_message(&bytes).is_err());
    assert!(
        start.elapsed() < Duration::from_millis(250),
        "decode took too long"
    );
}
