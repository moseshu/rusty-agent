//! # `ra-runtime`
//!
//! Loop 内核。公开 API 必须能跑任何 agent，不含任何业务词汇。

pub mod agent;
pub mod budget;
pub mod capability;
pub mod circuit;
pub mod guard;
pub mod hook;
pub mod permission;
pub mod runner;
pub mod tool;
pub mod turn;
