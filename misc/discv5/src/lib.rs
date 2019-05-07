//! The Discovery Version 5 protocol ([discv5](https://github.com/ethereum/devp2p/blob/master/discv5/discv5.md)).
//!
//! Discv5 is a UDP-based protocol for discovering nodes and their capabilities (topics) on a
//! peer-to-peer network.

// mod behaviour;
mod packet;

pub mod service;
