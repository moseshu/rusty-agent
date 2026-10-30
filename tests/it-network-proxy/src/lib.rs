//! Runs private upstream tests against copies of every product module.
#![allow(dead_code, unused_imports)]
mod dns_fixture;
mod home_fixture;
include!(concat!(env!("OUT_DIR"), "/proxy/test_root.rs"));
