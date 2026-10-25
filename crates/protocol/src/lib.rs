//! Types shared between the Tinlink server and its clients.

pub mod id;
pub mod permissions;

pub use id::UserId;
pub use permissions::{Overwrite, Permissions, compute_channel_permissions};
