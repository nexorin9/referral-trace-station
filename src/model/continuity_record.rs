//! `ContinuityRecord`：源产品 `StreamContinuityRecord` 的 4 态适配。
//!
//! - 来源：`copper-rs/core/cu29_runtime/src/continuity.rs::StreamContinuityRecord`
//! - 适配：把 `record: Vec<u8>` 改为「字段级 `ContinuityKind` 枚举 + cl_id + ref_version_id」
//!   以同时服务入站判别和持久化回放校验
//! - 详细契约（validate_replay_continuity 的三条 contract）由
//!   [`crate::continuity::four_state`] 在 Task 4 引入；本文件只承担「形状」声明

use serde::{Deserialize, Serialize};

/// 源产品四态枚举的字段级镜像（Task 4 会在 `crate::continuity::four_state`
/// 引入 `ContinuityKind` 的同名 alias 与 contract 校验函数；这里保留 `kind`
/// 字段作为 ReferralVersion 字段类型引用）。
///
/// 各变体含义：
/// - `Manifest`：流首个 entry 或重新声明 stream schema 的 entry。
/// - `Gap{first_id, last_id, reason}`：明确的空洞声明，要求发送方给出可审计原因。
/// - `RecoveryPoint`：恢复点（receiver 解到此处可继续接受后续 entry，不必臆造尾巴）。
/// - `Finished`：receiver 明确终态，不存在 unobserved sender tail。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContinuityKind {
    Manifest,
    Gap {
        first_id: u64,
        last_id: u64,
        reason: SourceGapReason,
    },
    RecoveryPoint {
        copperlist_id: u64,
    },
    Finished {
        next_copperlist_id: u64,
    },
}

/// 与源 `SourceGapReason` 对齐（LateJoin / SessionEnded；ManualDrop 为本产品新增，
/// 表示「drop 目录写入时人为指定丢弃」）。
///
/// 严禁直接对 owner 端暴露「UnspecifiedGapReason」之类的兜底变体 —— 任何 Gap
/// 声明都必须有明确原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceGapReason {
    /// 接收端晚到 / 错过若干 entry（典型：服务刚启动未接早期事件）。
    LateJoin,
    /// 上游 session 显式结束（典型：上一班次结束未补 entry）。
    SessionEnded,
    /// 区域平台 drop 目录手动剔除坏数据并标记（运维可观察到 .unparsed tag）。
    ManualDrop,
}

/// 持久化形态的连续性记录（JSON Lines 一行一条）。
///
/// `ref_version_id` 是 `ReferralVersion` 的业务编号（`referral_id@vN`），
/// 方便 replay 不必先反序列化整个 ReferralVersion 也能定位段落。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ContinuityRecord {
    pub kind: ContinuityKind,
    pub cl_id: u64,
    pub ref_version_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continuity_kind_manifest_round_trip() {
        let k = ContinuityKind::Manifest;
        let s = serde_json::to_string(&k).unwrap();
        assert!(s.contains("\"kind\":\"manifest\""));
        let k2: ContinuityKind = serde_json::from_str(&s).unwrap();
        assert_eq!(k, k2);
    }

    #[test]
    fn continuity_kind_gap_round_trip() {
        let k = ContinuityKind::Gap {
            first_id: 3,
            last_id: 5,
            reason: SourceGapReason::LateJoin,
        };
        let s = serde_json::to_string(&k).unwrap();
        assert!(s.contains("\"first_id\":3"));
        assert!(s.contains("\"last_id\":5"));
        assert!(s.contains("\"reason\":\"late_join\""));
        let k2: ContinuityKind = serde_json::from_str(&s).unwrap();
        assert_eq!(k, k2);
    }

    #[test]
    fn continuity_record_serde_round_trip() {
        let r = ContinuityRecord {
            kind: ContinuityKind::Finished {
                next_copperlist_id: 9,
            },
            cl_id: 8,
            ref_version_id: "REF-001@v8".to_string(),
        };
        let s = serde_json::to_string(&r).unwrap();
        let r2: ContinuityRecord = serde_json::from_str(&s).unwrap();
        assert_eq!(r, r2);
    }

    #[test]
    fn source_gap_reason_three_variants_distinct() {
        let a = SourceGapReason::LateJoin;
        let b = SourceGapReason::SessionEnded;
        let c = SourceGapReason::ManualDrop;
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }
}
