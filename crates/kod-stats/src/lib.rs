//! Session statistics (borrow from oh-my-pi, delta §13.2).
//!
//! * [`behavioral`] — free quality signals from the user's own
//!   messages: a user who says "no, I meant…" is telling the agent it
//!   missed the ask, and the signal is already in the text.
//! * [`request`] — per-request analytics and their aggregates: error
//!   rate, cache hit rate, cache savings, time-to-first-token, tokens
//!   per second.
//!
//! Both are pure functions over data the engine already has.

pub mod behavioral;
pub mod request;
