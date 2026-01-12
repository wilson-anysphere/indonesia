// Consolidated integration test suite.
//
// This module is compiled by `tests/real_jvm.rs` so `cargo test -p nova-dap --test real_jvm`
// builds a single integration test binary (see AGENTS.md harness pattern).

mod cancel;
mod config_stdio;
mod configuration_done_before_launch;
mod configuration_done_resumes_command_launch;
mod dap_conformance;
mod dap_java_launch;
mod dap_launch;
mod dap_session;
mod debugger_ux;
mod exception_breakpoints;
mod jdwp_client;
mod logpoints_hitcounts;
mod pre_attach_breakpoints;
mod pre_attach_function_breakpoints;
mod source_mapping;
mod stream_debug;
mod wire_breakpoint_mapping;
mod wire_format;
mod wire_variables_preview;

