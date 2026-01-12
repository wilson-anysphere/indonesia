#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Keep allocations bounded in case the engine is invoked without `-max_len`.
    if data.len() > 256 * 1024 {
        return;
    }

    let _ = nova_jdwp::decode_packet_bytes(data);
});

