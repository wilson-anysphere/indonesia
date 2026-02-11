// Consolidated integration test suite.
//
// This module is compiled by `tests/real_jvm.rs` so
// `bash scripts/cargo_agent.sh test --locked -p nova-dap --test tests`
// builds a single integration test binary containing most of the crate's
// end-to-end tests (see repo guidance about using one integration test harness
// per crate).
mod attach_hostname;
mod breakpoint_events;
mod breakpoint_locations;
mod cancel;
mod config_stdio;
mod configuration_done_before_launch;
mod configuration_done_resumes_command_launch;
mod dap_conformance;
mod dap_disconnect_terminate_attach;
mod dap_exited_event;
mod dap_java_launch;
mod dap_launch;
mod dap_launch_detach;
mod dap_restart;
mod dap_session;
mod debugger_ux;
mod enable_method_return_values;
mod exception_breakpoints;
mod jdwp_client;
mod json_error_sanitization;
mod logpoints_hitcounts;
mod outgoing_backpressure;
mod output_truncation;
mod panic_isolation;
mod pre_attach_breakpoints;
mod pre_attach_function_breakpoints;
mod process_event;
mod real_jvm;
mod slow_client_backpressure;
mod source_mapping;
mod stack_trace_paging;
mod stream_debug;
mod watchpoints;
mod wire_breakpoint_mapping;
mod wire_format;
mod wire_stream_debug;
mod wire_stream_debug_deadlock;
mod wire_stream_debug_internal_eval;
mod wire_stream_eval;
mod wire_variables_preview;

#[tokio::test]
async fn dap_hot_swap_can_compile_changed_files_with_javac() {
    dap_session::dap_hot_swap_can_compile_changed_files_with_javac().await;
}
