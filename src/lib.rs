//! Page and archive handling for `comic-auto-resize`.
//!
//! The binary is a thin shell over this library, so the decode → resize → encode contract
//! and the archive pipeline can be exercised by integration tests against committed
//! fixtures rather than only through the finished tool.

pub mod page;
pub mod pipeline;
pub mod policy;
pub mod sink;
pub mod source;
