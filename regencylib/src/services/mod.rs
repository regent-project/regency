//! Configuration types of every service (D15 files, D21 central documents),
//! shared so that backend can validate a central configuration document
//! against the service's own type.

pub mod backend;
pub mod dbwriter;
pub mod endpoint;
pub mod runner;
pub mod scheduler;
pub mod sourcesync;
