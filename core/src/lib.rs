//! Shared foundation for the regional search stack.
//!
//! Both binaries in this workspace depend on this crate, so the writer
//! (`bot`) and the reader (`mcp`) cannot drift apart on the document
//! schema or the index settings.

pub mod error;
pub mod id;
pub mod index;
pub mod meili;
pub mod model;
pub mod region;
pub mod submission;

pub use error::{Error, Result};
pub use model::{Article, Doc, Event, GeoPoint, GeoPrecision, Kind, Place};
pub use region::{BBox, City, RegionConfig};
pub use submission::{Request, Status, Submission};
