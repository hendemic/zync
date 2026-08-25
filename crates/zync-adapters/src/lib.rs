//! Everything outside the process that is not the screen: the light network and
//! the files on disk. Depends on `zync-core`; nothing in `zync-core` depends on
//! this. Screen capture lives in `zync-capture`, which is the one crate allowed
//! to touch native APIs.

#![forbid(unsafe_code)]

pub mod config;
pub mod lights;
pub mod mqtt;
