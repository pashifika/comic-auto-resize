//! Page-level image handling for `comic-auto-resize`.
//!
//! The binary is a thin shell over this library, so the decode → resize → encode
//! contract can be exercised by integration tests against a committed fixture rather
//! than only through the finished tool. Archive handling and the streaming pipeline
//! arrive with the next change; nothing here opens a file or a directory.

pub mod page;
