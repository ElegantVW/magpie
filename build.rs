// build.rs — compiles the nose's C++ seam (Xapian) when the `nose`
// feature is on (default). Without it, magpie builds as pure Rust:
// `cargo build --no-default-features`.
#[cfg(feature = "nose")]
fn main() {
    // Xapian discovery first: a clear house-style error beats c++ noise.
    // (build.sh checks this too, before cargo ever runs.)
    pkg_config::Config::new()
        .atleast_version("1.4")
        .probe("xapian-core")
        .expect("nose needs xapian-core >= 1.4 (libxapian dev files); or build with --no-default-features");
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .file("src/nose_shim.cc")
        .include("src")
        .flag_if_supported("-Wno-deprecated-declarations")
        .compile("nose_shim");
    println!("cargo:rerun-if-changed=src/nose_shim.cc");
    println!("cargo:rerun-if-changed=src/nose_shim.h");
}

#[cfg(not(feature = "nose"))]
fn main() {}
