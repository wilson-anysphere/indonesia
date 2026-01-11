(module
  ;; Nova Extension WASM ABI v1 example.
  ;;
  ;; - Implements diagnostics only (capability bit 0).
  ;; - Returns a diagnostic if the incoming JSON request contains the string "TODO".
  ;;
  ;; Build / run:
  ;; - Use `wat2wasm` (wabt) or `wat::parse_str` (Rust) to compile this to `.wasm`.

  (memory (export "memory") 1)

  (global $heap (mut i32) (i32.const 1024))

  (func $nova_ext_alloc (export "nova_ext_alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (local.set $ptr (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $ptr)
  )

  ;; Per ABI v1 the host will call `nova_ext_free` for request/response buffers.
  ;; This example uses a bump allocator so `free` is a no-op.
  (func $nova_ext_free (export "nova_ext_free") (param i32 i32)
    nop
  )

  (func (export "nova_ext_abi_version") (result i32) (i32.const 1))
  (func (export "nova_ext_capabilities") (result i32) (i32.const 1))

  (data (i32.const 0) "[{\"message\":\"TODO found\",\"severity\":\"info\",\"span\":{\"start\":0,\"end\":4}}]")

  (func $contains_todo (param $ptr i32) (param $len i32) (result i32)
    (local $i i32)
    (local $end i32)
    (if (i32.lt_u (local.get $len) (i32.const 4))
      (then (return (i32.const 0))))
    (local.set $end (i32.sub (local.get $len) (i32.const 4)))
    (local.set $i (i32.const 0))
    (block $break
      (loop $loop
        (br_if $break (i32.gt_u (local.get $i) (local.get $end)))
        (if
          (i32.and
            (i32.eq (i32.load8_u (i32.add (local.get $ptr) (local.get $i))) (i32.const 84)) ;; 'T'
            (i32.and
              (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 1)))) (i32.const 79)) ;; 'O'
              (i32.and
                (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 2)))) (i32.const 68)) ;; 'D'
                (i32.eq (i32.load8_u (i32.add (local.get $ptr) (i32.add (local.get $i) (i32.const 3)))) (i32.const 79)) ;; 'O'
              )
            )
          )
          (then (return (i32.const 1)))
        )
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $loop)
      )
    )
    (i32.const 0)
  )

  (func (export "nova_ext_diagnostics") (param $req_ptr i32) (param $req_len i32) (result i64)
    (local $out_ptr i32)
    (local $out_len i32)

    ;; If the request doesn't contain TODO, return 0 (empty list).
    (if (i32.eq (call $contains_todo (local.get $req_ptr) (local.get $req_len)) (i32.const 0))
      (then (return (i64.const 0))))

    (local.set $out_len (i32.const 71))
    (local.set $out_ptr (call $nova_ext_alloc (local.get $out_len)))
    (memory.copy (local.get $out_ptr) (i32.const 0) (local.get $out_len))

    ;; Pack `(ptr,len)` into an i64: `(len << 32) | ptr`.
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out_len)) (i64.const 32))
      (i64.extend_i32_u (local.get $out_ptr))
    )
  )
)
