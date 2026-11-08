//! D14: prost-generated client from the same `.proto` as backend (same wire
//! format; no Google protobuf runtime in WASM).

fn main() {
    println!("cargo:rerun-if-changed=../regencylib/proto/regency/v1/api.proto");
    tonic_prost_build::configure()
        .build_server(false)
        .build_transport(false)
        .compile_protos(
            &["../regencylib/proto/regency/v1/api.proto"],
            &["../regencylib/proto"],
        )
        .expect("compile api.proto");
}
