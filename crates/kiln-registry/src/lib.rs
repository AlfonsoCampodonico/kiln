//! A minimal OCI distribution client for kiln (spec §4.2 `kiln-registry`).
//!
//! - Blobs enter the store only through [`kiln_store::Store::put_verified`] (T1).
//! - Only the registry is contacted: descriptor `urls` are never fetched, and
//!   redirects, token realms and upload locations must be https and must not lead
//!   to loopback, link-local, private, CGNAT or unspecified addresses unless the
//!   registry itself is in that class (T2).
//! - Errors carry redacted URLs and never credentials or tokens (spec §13).
#![forbid(unsafe_code)]

mod auth;
mod client;
mod error;
mod policy;
mod reference;
#[doc(hidden)]
pub mod testregistry;

pub use auth::{Credential, DockerConfig};
pub use client::{
    Client, DOCKER_MANIFEST, DOCKER_MANIFEST_LIST, MANIFEST_TYPES, MAX_MANIFEST, MAX_REDIRECTS, MAX_TOKEN_RESPONSE,
    Manifest, ManifestHead, OCI_INDEX, OCI_MANIFEST,
};
pub use error::{RegistryError, Result, redact};
pub use policy::AddrClass;
pub use reference::{DOCKER_HUB, Reference, api_host};
