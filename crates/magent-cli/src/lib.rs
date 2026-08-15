//! Magent's command-line surface.
//!
//! The binary is what Claude Code hooks invoke, so startup cost is part of the
//! contract: a hook fires on every tool call and a `PreCompact` handler has a
//! 100 ms budget. That is why this is a native binary with no async runtime
//! rather than a script.

pub mod hook;
pub mod import;
pub mod packet;
pub mod paths;
