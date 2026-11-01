//! Band plan and transmit-authorization policy for sdrtx.
//!
//! Nothing in this workspace can key a transmitter without a [`policy::TxPermit`],
//! and the only way to obtain one is [`policy::authorize`].

pub mod bandplan;
pub mod freq;
pub mod policy;
