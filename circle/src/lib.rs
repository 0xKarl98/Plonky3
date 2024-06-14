//! A framework for operating over the unit circle of a finite field,
//! following the [Circle STARKs paper](https://eprint.iacr.org/2024/278) by Haböck, Levit and Papini.

// #![no_std]

extern crate alloc;

// mod cfft;
mod cfft;
mod deep_quotient;
mod domain;
// mod folding;
// mod pcs;
mod point;
// mod twiddles;
mod ordering;
mod util;
//
// pub use cfft::*;
pub use cfft::*;
pub use ordering::*;
// pub use pcs::*;
