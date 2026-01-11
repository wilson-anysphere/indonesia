#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = nova_remote_proto::transport::decode_framed_message(data);
});

