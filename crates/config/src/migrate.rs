//! Schema migration. Identity for V1 today; future V1→V2 upgrades slot in here.

use crate::schema::{v1::ConfigV1, ConfigFile};

pub fn migrate(file: ConfigFile) -> ConfigV1 {
    match file {
        ConfigFile::V1(cfg) => cfg,
    }
}
