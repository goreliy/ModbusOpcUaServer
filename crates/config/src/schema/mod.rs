//! On-disk configuration schema, versioned at the root.

pub mod v1;

use serde::{Deserialize, Serialize};

/// Versioned on-disk root. Adding V2 later is additive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "schema_version")]
pub enum ConfigFile {
    #[serde(rename = "1")]
    V1(v1::ConfigV1),
}
