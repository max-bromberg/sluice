//! sluice — series-aware update gating for rolling Linux distributions.
//!
//! Rolling distributions make one decision on your behalf that they should not:
//! they treat a bug-fix release and a new feature series as the same event. A
//! blanket package lock is the usual answer, and it is the wrong one — it
//! freezes you at the *least* mature point of a series, holding back exactly
//! the fixes you want while you wait to decide about the jump you do not.
//!
//! sluice separates the two. Fixes inside your current series flow
//! automatically. A new series is held, announced, and described — with its
//! upstream release lineage and your own machine's boot-health record — until
//! you promote it explicitly. Promotion is always a human decision; sluice only
//! ever offers evidence and a hint.
//!
//! The pieces:
//!
//! - [`config`] — every path, URL and threshold, with generic defaults.
//! - [`policy`] — the pure decision engine: installed + available + state → decision.
//! - [`backend`] — the package-manager seam ([`backend::zypper`] is the real one).
//! - [`gate`] — locks, and the guarantees that they are never left off.
//! - [`lineage`] — upstream release history, cached and offline-capable.
//! - [`health`] — boot evidence, including freezes that leave nothing in the log.
//! - [`vault`] — kept RPMs, so a rollback target outlives the repository.

pub mod app;
pub mod backend;
pub mod boot;
pub mod cli;
pub mod config;
pub mod exec;
pub mod gate;
pub mod health;
pub mod lineage;
pub mod notify;
pub mod policy;
pub mod privilege;
pub mod state;
pub mod tui;
pub mod vault;
pub mod version;

pub use config::Config;
pub use state::State;
