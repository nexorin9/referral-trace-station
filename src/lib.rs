//! referral-trace-station: 跨机构转诊单留痕回放工作台
//!
//! 该工作台把 copper-rs 的「静态 task graph 声明 + 每事件入 CopperList + fsck 序列空洞检测 +
//! replay 重放 + sha256 链」主路径，搬到跨机构转诊单 / 检查互认文档的版本时间线接缝上。
//!
//! 模块组织：
//! - [`model`]：数据结构层（Referral / ReferralVersion / ContinuityRecord）
//! - [`task_graph`]：静态生命周期声明（编译期生效）
//! - [`copperlist`]：cl_seq 单调计数器 + 序列空洞检测（fsck）+ 持久化重排保护
//! - [`continuity`]：StreamContinuityRecord 四态适配 + 三条 contract 守护
//! - [`live_log`]：实时事件广播注册表（UI 即时刷新）
//! - [`event_bus`]：桌面端事件总线（mpsc + broadcast 双通道；背压「丢旧保新」+ dropped 计数）
//! - [`sha256_chain`]：链式 sha256 摘要（canonical_json + 整链 verify + tamper 检测）
//! - [`replay`]：`.copper` 存档解析 + 链校验 + 时间线渲染（`replay` 子命令主入口）
//! - [`hl7`]：HL7 ORU^R01 简版解析（MSH/PID/OBR/OBX 四段）+ 失败兜底 mv unparsed
//! - [`inbox`]：区域平台 drop zip 三件套解析（before.json + after.json + signature.txt）
//! - [`his`]：HIS REST 拉快照客户端（reqwest + .env 凭据 + 5xx → drop 入站）
//! - [`ai`]：ModelGateway（OpenAI-compatible + 结构化 JSON + 离线降级 + audit.jsonl）

pub mod ai;
pub mod continuity;
pub mod copperlist;
pub mod event_bus;
pub mod export;
pub mod his;
pub mod hl7;
pub mod inbox;
pub mod live_log;
pub mod model;
pub mod replay;
pub mod sha256_chain;
pub mod task_graph;
pub mod ui;
pub mod verify;