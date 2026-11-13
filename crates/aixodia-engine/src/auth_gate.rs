//! The engine's side of the email gate.
//!
//! Issuing, confirming and expiring tokens is the auth Worker's job: it serves
//! `/auth/request`, `/auth/confirm` and `/auth/status` from its own origin, so
//! sign-in works whether or not this process is running. The allowlist lives there
//! too, because it decides who may sign in and that is not the engine's call.
//!
//! What is left here is the verdict type, which is how the engine asks the Worker
//! about a token a caller presented.

pub enum GateVerdict {
    /// Confirmed and unexpired.
    Active,
    /// Issued, but the user has not pressed Confirm.
    Pending,
    /// Ran out of time.
    Expired,
    /// The authority has no record of it.
    Unknown,
    /// The authority could not be reached.
    Unreachable,
}
