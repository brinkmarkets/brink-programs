//! Instruction handlers, `subject_verb_object`. Each file validates accounts with constraints, pushes complex
//! checks into named functions with their own error codes, and ends by asserting pool invariants.
pub mod admin;
pub mod basis;
pub mod fees;
pub mod forward;
pub mod hooks;
pub mod liquidity;
pub mod math;
pub mod queue;
pub mod reserve;
pub mod swap;

pub use admin::*;
pub use basis::*;
pub use fees::*;
pub use forward::*;
pub use liquidity::*;
pub use queue::*;
pub use reserve::*;
pub use swap::*;
#[cfg(test)]
mod properties;
