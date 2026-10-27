//! sprig: a small version control system built for a world where most
//! commits come from agents. Content-addressed and distributed like git,
//! with a server that exposes the repository as an API.

pub mod util;

pub mod cli;
pub mod codemerge;
pub mod diff;
pub mod git;
pub mod index;
pub mod merge;
pub mod pack;
pub mod project;
pub mod remote;
pub mod server;
pub mod sign;
pub mod store;
pub mod tree;
pub mod watch;
