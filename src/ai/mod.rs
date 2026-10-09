//! ModelGateway — 单一网关 + OpenAI-compatible 协议 + 离线降级 + audit.jsonl。
//!
//! ## 模块列表
//!
//! - [`openai_compat`]：ModelGateway + ModelGatewayConfig + AiSummary + AiError
//! - [`mock_server`]：自写 mock OpenAI-compatible 服务（仅测试用）：
//!   Task 13 提供 `/ok`；Task 14 扩展 `/malformed` / `/timeout` / `/5xx`
//!
//! ## 与 spec.md / plan.md 的对应
//!
//! - spec.md §1「ai_role: assist」+ §4「AI 调用仅做摘要归并」+ §9「不替医师改字段」：
//!   `ModelGateway::summarize_diff` 是 AI 入口；失败（`AiError::*`）由 caller 走
//!   `crate::his::fhir_map::compute_diff` 字段级 diff 视图，**不**推进任何业务状态
//! - spec.md §5「AI 不可达 → diff 视图」：超时 / 解析失败 / 未配置 → `AiError`
//!   冒泡 → caller 自动降级
//! - spec.md §5 安全审计：「AI 调用写 audit.jsonl」：`with_audit_log` 注入路径后
//!   每次调用必追加一行（ts + model + input_hash + status_code + latency_ms +
//!   status: ok|network_error|upstream_error|parse_error|schema_error）
//!
//! ## 不照搬
//!
//! - copper-rs 不调用外部 LLM；本模块是「留痕工作台」的 AI 嵌入层，模仿的是
//!   copper-rs 风格「单一网关入口 + 显式降级路径」哲学
//! - 不引入具体厂商 SDK；只走 OpenAI-compatible `POST /v1/chat/completions` +
//!   Bearer auth，配置由 .env 注入（spec §8 凭据位全部走环境变量）

pub mod mock_server;
pub mod openai_compat;

pub use openai_compat::{
    AiError, AiSummary, ModelGateway, ModelGatewayConfig, DEFAULT_OPENAI_MODEL,
    DEFAULT_OPENAI_TIMEOUT_MS,
};
