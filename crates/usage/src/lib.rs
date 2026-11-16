//! Usage-limit pages: the allowances of Claude Max, ChatGPT Plus and OpenCode Go Plus, shown as
//! `meter` pages whose feed is the service and whose items are its allowance windows.
//!
//! The hub knows nothing of this: it stores and sends meter pages like any other. The desktop
//! manager and the PC helper use this crate to build those pages and to check figures before they
//! send them.

pub mod api;
pub mod model;
pub mod provider;

pub use api::{
    clean_figures, ManualFigures, ManualMetric, NewSubscription, SubscriptionChange, SubscriptionView,
    UsageReport,
};
pub use model::{
    clean_percent, to_subscription, usage_service, MetricValue, PageData, Service, Subscription, Window,
};
