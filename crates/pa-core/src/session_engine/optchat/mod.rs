//! Optional `OptChat` session strategy: binary source-linked summaries and exact recall.
//! Lua owns policy and model orchestration; native code owns tree correctness,
//! layout, budget, canonical retrieval and the generation/branch publication fence.

mod service;
mod tree;
pub(crate) use service::OptChatServices;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod service_tests;

#[cfg(test)]
mod engine_tests;
