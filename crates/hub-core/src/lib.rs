//! Display Hub's domain model, with no I/O: subscriptions, data quality, freshness, providers and
//! building snapshots. Time always comes from a [`Clock`].

pub mod api;
pub mod build;
pub mod clock;
pub mod freshness;
pub mod model;
pub mod provider;

pub use build::build_snapshot;
pub use clock::{Clock, FakeClock, SystemClock};
pub use freshness::{short_duration, Freshness};
pub use model::{MetricValue, PageData, Service, Source, Subscription, Window};
