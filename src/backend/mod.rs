pub mod fake;
pub mod guard;
pub mod mysql;
pub mod postgres;

use anyhow::Result;

use crate::config::{Kind, Target};
use crate::model::{ExplainTarget, Plan, Sample};

/// A database to monitor. Calls block, so they only ever run on the collector thread.
pub trait Backend: Send + 'static {
    fn label(&self) -> String;
    fn poll(&mut self) -> Result<Sample>;
    /// Must stay read-only: never `EXPLAIN ANALYZE`, which executes the statement.
    fn explain(&mut self, target: &ExplainTarget) -> Result<Plan>;
}

/// Connects to `target` now, so a wrong host or password fails before the screen is taken over.
pub fn open(target: &Target) -> Result<Box<dyn Backend>> {
    match target.kind {
        Kind::Mysql => Ok(Box::new(mysql::MySql::connect(target)?)),
        Kind::Postgres => Ok(Box::new(postgres::Postgres::connect(target)?)),
    }
}
