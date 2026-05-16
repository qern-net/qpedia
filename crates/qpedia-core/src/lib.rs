//! Qpedia domain types shared across all crates.
//! See `DESIGN.md` §2 (Data Model) for the authoritative definitions.

pub mod error;
pub mod ids;
pub mod source;
pub mod wiki;
pub mod job;
pub mod acl;
pub mod tenant;

pub use error::{Error, Result};
pub use ids::{SourceId, PageId, JobId};
pub use tenant::{Tenant, DEFAULT_TENANT};
