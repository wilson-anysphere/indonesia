#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = nova_remote_proto::v3::decode_rpc_payload(data);
});

