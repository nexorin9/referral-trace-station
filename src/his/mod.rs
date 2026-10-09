//! HIS REST 拉取客户端 + FHIR JSON 字段映射模块。
//!
//! 把 HIS 系统的 `GET /referrals/{id}/snapshot` 拉到本地，作为 inbox drop 的
//! 二级入站（drop 是默认优先；HIS REST 仅作为补充）；FHIR JSON 经本模块做
//! 字段映射后落入 ReferralVersion。
//!
//! ## 与 spec.md / plan.md 的对应
//!
//! - spec.md §3 最小数据格式：HIS REST 拉到的 snapshot 是 JSON（fields 不限，
//!   本工具只取 `referral.id` / `modification.timestamp` / `modification.actor`
//!   关键字段），由 [`fhir_map::map_fhir_to_referral`] 完成
//! - spec.md §5 失败模式：「HIS REST 5xx → 走 drop 目录」由
//!   [`HisError::Unavailable`] 守护；「HIS REST 凭据缺 → 走 drop」由
//!   [`HisError::CredentialMissing`] 守护；「FHIR 缺关键字段 → 拒绝该
//!   referral_id」由 [`fhir_map::FhirError::MissingField`] 守护
//! - plan.md §5「HIS REST 凭据缺 / 网络隔离」风险缓解：本模块是该风险点的硬关
//!
//! ## 不照搬
//!
//! - copper-rs 没有「外部 HIS REST 入站」概念（它是进程内 CuLogEntry 流）
//! - 本模块填补「HIS FHIR/REST → 区域平台 drop 单调流」的桥
//!
//! ## 公开 API
//!
//! - [`HisClient`]：reqwest 异步客户端，绑 `HIS_BASE_URL` / `HIS_TOKEN` 走 .env
//! - [`HisClientConfig`]：构造配置（base_url / token / timeout_ms）
//! - [`HisError`]：5 类失败（CredentialMissing / Network / Unavailable /
//!   ClientError / ParseError）
//! - [`fhir_map::map_fhir_to_referral`]：FHIR JSON → ReferralVersion
//! - [`fhir_map::compute_diff`]：字段级 diff（基于 similar crate）

pub mod client;
pub mod fhir_map;
pub mod mock_server;

pub use client::{HisClient, HisClientConfig, HisError};
pub use fhir_map::{compute_diff, flatten_value_map, map_fhir_to_referral, DiffEntry, FhirError};
