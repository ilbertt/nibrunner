//! The edge. One route per hostname to the app's loopback port, in this process rather than in a
//! proxy beside it: the process that knows what is running is the one that decides where a
//! request goes, which is what leaves no room for the two to disagree.

pub mod activator;
pub mod forward;
pub mod router;

pub use router::{RouteTable, Router};
