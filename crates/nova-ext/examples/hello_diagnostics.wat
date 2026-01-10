(module
  (memory (export "memory") 1)
  (data (i32.const 0) "[{\"message\":\"Hello from wasm\",\"severity\":\"info\",\"span\":{\"start\":0,\"end\":1}}]\00")
  (func (export "diagnostics_ptr") (result i32) (i32.const 0))
)
