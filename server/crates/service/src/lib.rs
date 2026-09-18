//! Turing Eye 的 HTTP 契约验证类型与外部依赖适配器。

pub mod contracts;

#[cfg(feature = "dependency-probes")]
pub mod infrastructure;
