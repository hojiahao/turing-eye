//! R1 模型 SDK 兼容性实验，不负责评测调度或持久化。

pub mod probe;
mod rig_client;

pub use rig_client::{
    FailureKind, ModelClient, ModelConfig, ModelFailure, ModelOutput, ModelRequest, ModelTool,
    ModelToolCall, ModelToolResult, RemoteExecution, TokenUsage,
};

#[cfg(test)]
mod tests;
