fn main() {
    // Cargo exposes the compilation target triple to build scripts via the `TARGET`
    // environment variable. We forward it into the crate so `meta.json` can include
    // a stable, compile-time target identifier without relying on the embedding
    // process environment.
    let target = std::env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=NOVA_BUGREPORT_TARGET_TRIPLE={target}");
    println!("cargo:rerun-if-env-changed=TARGET");
}

