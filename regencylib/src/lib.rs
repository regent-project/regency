//! Types and building blocks shared by regency's services.
//!
//! - [`config`], [`logging`], [`telemetry`]: service conventions (D15);
//! - [`messages`], [`proto`]: message formats on RabbitMQ (D2, D6, D7, D10);
//! - [`model`]: CouchDB documents and their merges (D8, D11, D12, D16);
//! - [`schedule`], [`selector`]: orders (D16);
//! - [`assignment`]: effective expected state of a host (D23);
//! - [`validate`]: inline-secret rejection (D9);
//! - [`hashing`], [`amqp`], [`topology`], [`shard`]: RabbitMQ (D3, D4);
//! - [`couch`]: thin CouchDB client (D18);
//! - [`policy`]: Cedar authorization (D12).

pub mod amqp;
pub mod assignment;
pub mod central;
pub mod config;
pub mod control;
pub mod couch;
pub mod databases;
pub mod hashing;
pub mod logging;
pub mod messages;
pub mod model;
pub mod passwords;
pub mod policy;
pub mod process;
pub mod schedule;
pub mod security;
pub mod selector;
pub mod services;
pub mod shard;
pub mod telemetry;
#[cfg(feature = "testing")]
pub mod testing;
pub mod tokens;
pub mod topology;
pub mod validate;

/// Generated protobuf messages (`proto/regency/v1/*.proto`).
#[allow(clippy::all)]
pub mod proto {
    include!(concat!(
        env!("OUT_DIR"),
        "/protobuf_generated/regency/v1/generated.rs"
    ));
}

/// backend gRPC service stubs (feature `api`).
#[cfg(feature = "api")]
#[allow(clippy::all)]
pub mod api {
    include!(concat!(env!("OUT_DIR"), "/regency.v1.Regency.rs"));
}
