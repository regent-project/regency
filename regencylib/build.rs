//! Protobuf code generation (needs `protoc` 36.1 exactly, like regent-sdk, D6).
//!
//! - always: `proto/regency/v1/envelope.proto` (outcome envelope on RabbitMQ);
//! - feature `api`: `proto/regency/v1/api.proto` messages and the backend
//!   service stubs (tonic, with regent-sdk's codec for Google protobuf messages).

fn main() {
    println!("cargo:rerun-if-changed=proto");
    println!("cargo:rerun-if-env-changed=PROTOC");
    let mut inputs = vec!["regency/v1/envelope.proto"];
    if cfg!(feature = "api") {
        inputs.push("regency/v1/api.proto");
    }
    if let Err(details) = protobuf_codegen::CodeGen::new()
        .inputs(inputs)
        .include("proto")
        .generate_and_compile()
    {
        panic!("protobuf code generation failed: {details}");
    }
    #[cfg(feature = "api")]
    api::stubs();
}

#[cfg(feature = "api")]
mod api {
    include!("api_stubs.rs");
}
