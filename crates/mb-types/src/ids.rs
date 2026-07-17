//! Dense id newtypes shared across the gateway.
//!
//! `TagId` is dense and contiguous from 0: it directly indexes the flat tag
//! cache. All ids are assigned by `gateway-config`'s resolve step.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct ChannelId(pub u16);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct DeviceId(pub u32);

/// Dense, contiguous from 0; indexes the flat cache.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd, Debug, Serialize, Deserialize)]
pub struct TagId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub struct PollGroupId(pub u16);
