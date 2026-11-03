// src/handlers/mod.rs — request handlers split by resource domain.
// `common` holds the shared plumbing (state, query structs, link/etag
// helpers, patch utilities, ApiError); each resource module keeps only
// its own handlers. `pub use` keeps `use handlers::*` working for lib.rs;
// the modules are pub(crate) so docs.rs can reach the generated
// `__path_*` items for the OpenAPI registry.
pub(crate) mod catalogs;
pub(crate) mod collections;
pub mod common;
pub(crate) mod items;
pub(crate) mod metadata;
pub(crate) mod search;

pub use catalogs::*;
pub use collections::*;
pub use common::*;
pub use items::*;
pub use metadata::*;
pub use search::*;
