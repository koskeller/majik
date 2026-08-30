//! Desktop integration that GPUI does not cover, behind one portable API per capability.
//!
//! Each module is a portable API with a `#[cfg(target_os)]` backend and a stub elsewhere, and states
//! what it can actually do per platform in `const bool`s the callers branch on, so a capability the
//! current OS lacks degrades instead of failing. Today that is [`clipboard`], which puts the files
//! themselves on the clipboard where GPUI can only put a bitmap.

pub mod clipboard;
