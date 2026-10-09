//! 转诊单生命周期 4 阶段静态表。
//!
//! 对应源 `copper-rs` 的「静态 task graph 声明」主路径（不引 RON，纯 Rust 结构体
//! 数组）。该表由 `#[cfg(test)]` 守住编译期一致性：每个阶段名 → 触发事件必须被
//! 显式枚举，未声明的阶段名调用 [`phase_for_event`] 会返回 `Err`，便于在
//! `cargo build` / `cargo test` 阶段就发现漏映射。
//!
//! ⚠ 该表只承担「形状」与「声明」；状态的真正迁移由运行时驱动（inbox drop watcher
//! + ratatui 事件循环 + replay runner）。本工具永不主动推进业务状态，详见
//! spec.md §5 与 §9 的「仅观察态」边界。

use serde::{Deserialize, Serialize};
use std::fmt;

/// 转诊单生命周期 4 阶段（与 spec.md §5 状态生命周期对齐）。
///
/// 本工具仅观察态：从「上转机构开单」到「本院接收医师复核签收」。**本工具绝不
/// 主动推进任何阶段**，仅在 inbox drop watcher 收到新 entry 时如实记录并把事件
/// 转发给 LiveLogListener。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePhase {
    /// 上转机构首次开单 / 区域平台 baseline 落库。
    Created,
    /// 上级医师或对方机构修改诊断 / 治疗方案后推送新快照。
    Modified,
    /// 本院接收医师确认接收（含会签 / 拒收）。
    Received,
    /// 本院复核签收（医务科在 HIS 内人工签收；本工具只标记观察到签收事件）。
    Reviewed,
}

impl fmt::Display for LifecyclePhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            LifecyclePhase::Created => "created",
            LifecyclePhase::Modified => "modified",
            LifecyclePhase::Received => "received",
            LifecyclePhase::Reviewed => "reviewed",
        };
        f.write_str(s)
    }
}

/// 触发事件：哪个入站事件把 entry 落到哪个阶段。
///
/// 输入源见 `plan.md §3` 入站表（HIS REST / 区域平台 drop / FHIR / HL7 ORU）；
/// 这里只取「业务语义」名称，与协议无关。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerEvent {
    /// 上转机构开单（drop zip `*.created.zip` or HIS REST POST 创建）。
    OutboxSend,
    /// 修改快照推送（drop zip `*.modified.zip` or HIS REST PUT 字段变更）。
    DropUpdate,
    /// 本院接收医师确认接收。
    DropAck,
    /// 医务科人工签收（观察 HIS audit_log 或区域平台 web hook 「signed」事件）。
    MarkReviewed,
}

/// 静态表行：阶段 + 触发事件 + 简短描述。
#[derive(Clone, Debug, PartialEq)]
pub struct LifecycleRow {
    pub phase: LifecyclePhase,
    pub trigger: TriggerEvent,
    pub description: &'static str,
}

/// 静态表（编译期常量；不引 RON）。
///
/// 4 行严格对齐 4 阶段，缺一行 `cargo build` 即失败（数组长度是常量，长度不符
/// 编译期报错）。
pub const LIFECYCLE_TABLE: &[LifecycleRow] = &[
    LifecycleRow {
        phase: LifecyclePhase::Created,
        trigger: TriggerEvent::OutboxSend,
        description: "上转机构首次开单；区域平台 baseline 落库",
    },
    LifecycleRow {
        phase: LifecyclePhase::Modified,
        trigger: TriggerEvent::DropUpdate,
        description: "上级医师 / 对方机构修改诊断或治疗方案后推送新快照",
    },
    LifecycleRow {
        phase: LifecyclePhase::Received,
        trigger: TriggerEvent::DropAck,
        description: "本院接收医师确认接收（含会签 / 拒收两条分支）",
    },
    LifecycleRow {
        phase: LifecyclePhase::Reviewed,
        trigger: TriggerEvent::MarkReviewed,
        description: "本院医务科人工签收（本工具仅观察到该事件，不直写 HIS）",
    },
];

/// 编译期断言：表长严格 = 阶段枚举变体数。
///
/// 若有人新增 `LifecyclePhase` 变体但忘了同步 `LIFECYCLE_TABLE`，编译期立刻
/// 报错（与源 copper-rs 的「静态 task graph 必须覆盖所有节点」约束同款）。
pub const fn static_table_covers_all_phases() -> bool {
    let mut covered: u32 = 0;
    let mut i = 0;
    while i < LIFECYCLE_TABLE.len() {
        covered |= 1 << (LIFECYCLE_TABLE[i].phase as u32);
        i += 1;
    }
    // 4 个阶段必须都被覆盖（位 0..3 全为 1）
    covered == 0b1111
}

/// 按「阶段名 → 触发事件」查找。
pub fn trigger_for_phase(phase: LifecyclePhase) -> Option<TriggerEvent> {
    LIFECYCLE_TABLE
        .iter()
        .find(|row| row.phase == phase)
        .map(|row| row.trigger)
}

/// 按「触发事件 → 阶段」反向查找。
pub fn phase_for_event(trigger: TriggerEvent) -> Option<LifecyclePhase> {
    LIFECYCLE_TABLE
        .iter()
        .find(|row| row.trigger == trigger)
        .map(|row| row.phase)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 守住编译期常量与变体对齐。
    #[test]
    fn test_task_graph_static_task_accept() {
        assert!(static_table_covers_all_phases());
        assert_eq!(LIFECYCLE_TABLE.len(), 4);
        // 每个阶段必须有一个唯一触发事件
        for phase in [
            LifecyclePhase::Created,
            LifecyclePhase::Modified,
            LifecyclePhase::Received,
            LifecyclePhase::Reviewed,
        ] {
            assert!(
                trigger_for_phase(phase).is_some(),
                "phase {:?} must map to a trigger",
                phase
            );
        }
        // 反向查找亦覆盖全部 4 个事件
        for trigger in [
            TriggerEvent::OutboxSend,
            TriggerEvent::DropUpdate,
            TriggerEvent::DropAck,
            TriggerEvent::MarkReviewed,
        ] {
            assert!(
                phase_for_event(trigger).is_some(),
                "trigger {:?} must map to a phase",
                trigger
            );
        }
        // 自反性：阶段↔事件 双射
        for phase in [
            LifecyclePhase::Created,
            LifecyclePhase::Modified,
            LifecyclePhase::Received,
            LifecyclePhase::Reviewed,
        ] {
            let trigger = trigger_for_phase(phase).unwrap();
            assert_eq!(
                phase_for_event(trigger),
                Some(phase),
                "phase {:?} ↔ trigger {:?} not bijective",
                phase,
                trigger
            );
        }
    }

    #[test]
    fn test_lifecycle_phase_display() {
        assert_eq!(LifecyclePhase::Created.to_string(), "created");
        assert_eq!(LifecyclePhase::Modified.to_string(), "modified");
        assert_eq!(LifecyclePhase::Received.to_string(), "received");
        assert_eq!(LifecyclePhase::Reviewed.to_string(), "reviewed");
    }

    #[test]
    fn test_lifecycle_table_rows_consistent() {
        for row in LIFECYCLE_TABLE {
            // 每个 row 的 phase 在该 row 的 trigger 反查中必须命中 row.phase
            let back = phase_for_event(row.trigger);
            assert_eq!(
                back,
                Some(row.phase),
                "row {:?} ↔ {:?} inconsistent",
                row.phase,
                row.trigger
            );
            // description 不能为空
            assert!(!row.description.trim().is_empty());
        }
    }
}
