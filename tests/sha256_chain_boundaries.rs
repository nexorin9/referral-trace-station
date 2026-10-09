//! sha256 链校验单元测试（Task 25）
//!
//! ## 范围
//!
//! 与 `src/sha256_chain/chain.rs` 内嵌单测互补 —— 此处作为外部 integration
//! test 覆盖「关键边界」七类：
//!
//! 1. `test_chain_length_1`：链长度 = 1（边界下沿）—— 验证单链节 prev_hash
//!    = ZERO_HASH + head_hash = link.this_hash + verify_chain 通过。
//! 2. `test_chain_length_100`：链长度 = 100（中等规模）—— 验证长链所有 hash
//!    唯一且全部衔接 verify_chain 通过 + head_hash = 最后一条 this_hash。
//! 3. `test_chain_tamper_middle`：篡改中间链节 hash —— verify_chain 必须
//!    报 ThisHashMismatch 并指向正确 index。
//! 4. `test_chain_tamper_payload`：篡改 payload 但保持 prev_hash /
//!    this_hash —— verify_chain 应报 ThisHashMismatch（payload 改导致
//!    重算 hash 与链上不一致）。
//! 5. `test_chain_tamper_prev_hash`：篡改 prev_hash 但 payload / this_hash
//!    不变 —— verify_chain 应报 PrevHashMismatch（衔接收尾失败）。
//! 6. `test_chain_canonical_field_order_invariant`：canonical_json 在 key
//!    顺序不同的输入下输出字节一致 —— 保证 verify 在不同序列化器实现间
//!    跨环境可比。
//! 7. `test_chain_recovery_point_recompute`：从 RecoveryPoint（链中某条
//!    已验证链节的 head_hash）截断后，新链节仍可衔接 head_hash 继续追加
//!    并通过 verify；并守住「RecoveryPoint 后接收方对未观察发送端尾部
//!    不臆造」语义（truncated chain verify 必显式失败）。
//!
//! ## 与 src/sha256_chain/chain.rs 内嵌单测的区别
//!
//! - 内嵌单测在 lib 模块内（`#[cfg(test)] mod tests`），粒度是「链 step1+step2
//!   + canonical_json 字段顺序 + 链首条 + head_hash 跟踪 + Display +
//!   serde 往返 + 结构体 payload」；
//! - 本文件作为 integration test，从外部视角覆盖**完整 7 类边界**，重点在
//!   链长度下沿 / 中等规模 / 三类篡改变体 / 跨序列化器一致性 / RecoveryPoint
//!   衔接 + 不臆造尾部。

use referral_trace_station::sha256_chain::{
    canonical_json, hex_lower, link_hash, verify_chain, Chain, ChainError, ChainLink, ZERO_HASH,
};
use serde_json::json;

// -- 1. 链长度 = 1（边界下沿）--

/// 测试 1：链长度 = 1（边界下沿）。
///
/// 验证：
/// - `chain.len() == 1`
/// - `link[0].prev_hash == hex_lower(&ZERO_HASH)`（链首条 anchor）
/// - `hex_lower(chain.head_hash()) == link[0].this_hash`
/// - `verify_chain` 通过（单链节链路本身就是健康路径）
/// - `link[0].this_hash.len() == 64`（sha256 输出固定 64 字符 hex）
/// - `link[0].this_hash != hex_lower(&ZERO_HASH)`（链 anchor ≠ 实际 hash，
///   概率 2^-256 排除碰撞）
///
/// 边界意义：守住链长度下沿 —— 单链节既是「链首条」也是「链尾条」，
/// prev_hash = ZERO_HASH 且 head_hash = this_hash 是链结构的不变量。
#[test]
fn test_chain_length_1() {
    let mut chain = Chain::new();
    let link1 = chain
        .append(&json!({"cl_id": 1, "actor": "dr_wang@hospital_a", "diagnosis": "chest pain"}))
        .expect("append must succeed");

    assert_eq!(chain.len(), 1, "single-link chain must have len=1");
    assert_eq!(chain.as_slice().len(), 1);

    // 链首条 prev_hash 必须等于 ZERO_HASH
    assert_eq!(
        link1.prev_hash,
        hex_lower(&ZERO_HASH),
        "first link prev_hash must be ZERO_HASH anchor"
    );
    assert_eq!(link1.prev_hash.len(), 64, "sha256 hex must be 64 chars");

    // head_hash 必须等于 link[0].this_hash
    assert_eq!(
        hex_lower(chain.head_hash()),
        link1.this_hash,
        "head_hash must track latest link's this_hash"
    );

    // this_hash 长度 + 与 ZERO_HASH 区分
    assert_eq!(link1.this_hash.len(), 64);
    assert_ne!(
        link1.this_hash,
        hex_lower(&ZERO_HASH),
        "actual hash must not collide with ZERO_HASH anchor"
    );

    // verify_chain 通过
    assert!(
        verify_chain(chain.as_slice()).is_ok(),
        "single-link chain must verify clean"
    );
}

// -- 2. 链长度 = 100（中等规模）--

/// 测试 2：链长度 = 100（中等规模）。
///
/// 验证：
/// - `chain.len() == 100`
/// - 100 条 hash 全部 64 字符 hex 且两两不等（任何链节碰撞概率 2^-256）
/// - `link[i+1].prev_hash == link[i].this_hash`（逐条衔接）
/// - `link[0].prev_hash == ZERO_HASH`（链 anchor）
/// - `head_hash == link[99].this_hash`（head 跟踪最新链节）
/// - `verify_chain` 通过（中等规模健康链路）
/// - 采样第 0 / 49 / 99 条 hash 全不等（覆盖首 / 中 / 尾）
///
/// 边界意义：守住中等规模链的稳定性 —— verify 在 100 条量级必须仍能一次
/// 跑通；本规模也是 Task 29 性能验收（100 转诊单 × 5 版本 = 500 条链节）
/// 的单 referral 子集。
#[test]
fn test_chain_length_100() {
    let mut chain = Chain::new();
    for i in 1u64..=100 {
        chain
            .append(&json!({
                "cl_id": i,
                "actor": format!("dr_{i}@hospital_x"),
                "modified_at": format!("2026-10-10T08:{:02}:00Z", i % 60),
            }))
            .expect("append must succeed");
    }

    assert_eq!(chain.len(), 100, "100-link chain must have len=100");

    let links = chain.as_slice();
    assert_eq!(links.len(), 100);

    // 链首条 prev_hash 必须等于 ZERO_HASH
    assert_eq!(links[0].prev_hash, hex_lower(&ZERO_HASH));

    // 逐条衔接：link[i+1].prev_hash == link[i].this_hash
    for i in 0..links.len() - 1 {
        assert_eq!(
            links[i + 1].prev_hash, links[i].this_hash,
            "link[{next}].prev_hash must equal link[{cur}].this_hash",
            cur = i,
            next = i + 1
        );
    }

    // head_hash 必须等于 link[99].this_hash
    assert_eq!(
        hex_lower(chain.head_hash()),
        links[99].this_hash,
        "head_hash must equal last link's this_hash"
    );

    // 全部 hash 64 字符且两两不等（用 HashSet 去重）
    let mut seen = std::collections::HashSet::new();
    for link in links {
        assert_eq!(link.this_hash.len(), 64, "every this_hash must be 64 chars");
        assert!(
            seen.insert(link.this_hash.clone()),
            "this_hash collision detected (probability ~2^-256): {}",
            link.this_hash
        );
    }
    assert_eq!(seen.len(), 100, "all 100 this_hashes must be distinct");

    // 采样首 / 中 / 尾三条 hash 全不等
    assert_ne!(links[0].this_hash, links[49].this_hash);
    assert_ne!(links[49].this_hash, links[99].this_hash);
    assert_ne!(links[0].this_hash, links[99].this_hash);

    // verify_chain 一次跑通
    assert!(
        verify_chain(links).is_ok(),
        "100-link healthy chain must verify clean"
    );
}

// -- 3. 篡改中间链节 hash --

/// 测试 3：篡改中间链节的 `this_hash` 字段（不动 payload / prev_hash）——
/// verify_chain 应报 `ThisHashMismatch { index = 1 }`。
///
/// 构造：3 条链 → 直接覆写 link[1].this_hash 为伪造值。
///
/// 边界意义：守住「链上 this_hash 字段直接被改」一类攻击 —— 即使 payload /
/// prev_hash 都没动，重算 hash 与链上不一致即报 tamper。
#[test]
fn test_chain_tamper_middle() {
    let mut chain = Chain::new();
    chain.append(&json!({"cl_id": 1, "note": "original v1"})).unwrap();
    chain.append(&json!({"cl_id": 2, "note": "original v2"})).unwrap();
    chain.append(&json!({"cl_id": 3, "note": "original v3"})).unwrap();

    // 在内存里篡改 link[1].this_hash（不动 prev_hash / payload）
    let mut links: Vec<ChainLink> = chain.as_slice().to_vec();
    let original_link1_this_hash = links[1].this_hash.clone();
    let forged_hash = hex_lower(&[0xAA; 32]);
    links[1].this_hash = forged_hash.clone();

    // verify_chain 应报 ThisHashMismatch { index = 1 }
    let err = verify_chain(&links).unwrap_err();
    match err {
        ChainError::ThisHashMismatch { index, expected_this, actual_this } => {
            assert_eq!(index, 1, "tamper detected at the modified index");
            assert_eq!(expected_this, forged_hash, "expected_this must be the forged value");
            assert_eq!(
                actual_this, original_link1_this_hash,
                "actual_this must be the recomputed original hash"
            );
        }
        other => panic!("expected ThisHashMismatch at index=1, got {other:?}"),
    }
}

// -- 4. 篡改 payload 但保持 prev_hash / this_hash --

/// 测试 4：篡改 payload 但保持 prev_hash / this_hash 不变 —— verify_chain 应
/// 报 `ThisHashMismatch`（payload 改了，重算 sha256(prev_hash || canonical_json(new_payload))
/// 与链上保留的旧 this_hash 不一致）。
///
/// 构造：3 条链 → 篡改 link[1].payload 为带新字段的对象。
///
/// 边界意义：守住「只动 payload 不动 hash」一类攻击 —— 这类篡改比改 hash
/// 更隐蔽（攻击者以为保留 hash 即可绕过），实际 verify_chain 重算必然暴露。
#[test]
fn test_chain_tamper_payload() {
    let mut chain = Chain::new();
    chain.append(&json!({"cl_id": 1, "diagnosis": "chest pain"})).unwrap();
    chain.append(&json!({"cl_id": 2, "diagnosis": "ACS suspected"})).unwrap();
    chain.append(&json!({"cl_id": 3, "diagnosis": "STEMI confirmed"})).unwrap();

    // 在内存里篡改 link[1].payload（不动 prev_hash / this_hash）
    let mut links: Vec<ChainLink> = chain.as_slice().to_vec();
    let tampered_payload = json!({
        "cl_id": 2,
        "diagnosis": "ACS suspected",
        "tampered_field": "attacker_payload"
    });
    links[1].payload = tampered_payload;

    // verify_chain 应报 ThisHashMismatch { index = 1 } —— payload 改了，重算 hash 与链上 this_hash 不一致
    let err = verify_chain(&links).unwrap_err();
    match err {
        ChainError::ThisHashMismatch { index, expected_this, actual_this } => {
            assert_eq!(
                index, 1,
                "payload tamper must be detected at the modified link"
            );
            // expected_this = 链上保留的旧 this_hash
            // actual_this = 重算后的 hash（基于被篡改的 payload）
            assert_eq!(expected_this.len(), 64);
            assert_eq!(actual_this.len(), 64);
            assert_ne!(
                expected_this, actual_this,
                "expected_this must differ from recomputed actual_this"
            );
        }
        other => panic!("expected ThisHashMismatch at index=1, got {other:?}"),
    }

    // 反向断言：link[0] / link[2] 不受篡改影响（hash 应一致）
    let expected_link0 = chain.as_slice()[0].this_hash.clone();
    let expected_link2 = chain.as_slice()[2].this_hash.clone();
    assert_eq!(links[0].this_hash, expected_link0);
    assert_eq!(links[2].this_hash, expected_link2);
}

// -- 5. 篡改 prev_hash（payload / this_hash 不变）--

/// 测试 5：篡改 prev_hash 但 payload / this_hash 不变 —— verify_chain 应
/// 报 `PrevHashMismatch`（衔接收尾失败：link[i].prev_hash 应等于
/// link[i-1].this_hash，但实际不等）。
///
/// 构造：3 条链 → 把 link[1].prev_hash 改成「deadbeef」×8（不是 link[0].this_hash）。
///
/// 边界意义：守住「中间链节指向错误 prev」一类篡改 —— 链节内部三字段任何
/// 一个被改都应被 detect；本测试与 test_chain_tamper_middle
/// （改 this_hash）和 test_chain_tamper_payload（改 payload）形成三类篡改
/// 闭环。
#[test]
fn test_chain_tamper_prev_hash() {
    let mut chain = Chain::new();
    chain.append(&json!({"cl_id": 1})).unwrap();
    chain.append(&json!({"cl_id": 2})).unwrap();
    chain.append(&json!({"cl_id": 3})).unwrap();

    // 篡改 link[1].prev_hash 为伪造值（不动 payload / this_hash）
    let mut links: Vec<ChainLink> = chain.as_slice().to_vec();
    let forged_prev = "deadbeef".repeat(8); // 64 字符 hex 伪造值
    let original_link1_prev = links[1].prev_hash.clone();
    links[1].prev_hash = forged_prev.clone();

    // verify_chain 应报 PrevHashMismatch { index = 1 }
    let err = verify_chain(&links).unwrap_err();
    match err {
        ChainError::PrevHashMismatch { index, expected_prev, actual_prev } => {
            assert_eq!(index, 1, "prev_hash tamper detected at the modified link");
            // expected_prev = link[0].this_hash（即「应该接上的 prev」）
            // actual_prev = 篡改后的 forged_prev
            assert_eq!(
                actual_prev, forged_prev,
                "actual_prev must be the forged value"
            );
            assert_eq!(
                expected_prev,
                chain.as_slice()[0].this_hash.clone(),
                "expected_prev must equal link[0].this_hash"
            );
            assert_ne!(
                expected_prev, actual_prev,
                "expected_prev must differ from forged actual_prev"
            );
        }
        other => panic!("expected PrevHashMismatch at index=1, got {other:?}"),
    }

    // 反向断言：原始 prev_hash 与 forged 不同
    assert_ne!(original_link1_prev, forged_prev);

    // 反向断言：篡改后单链节 link[1] 的 payload + this_hash 没被本测试动过
    // （与原始 chain 一致）
    assert_eq!(
        links[1].payload,
        chain.as_slice()[1].payload,
        "tamper test must not modify payload"
    );
    assert_eq!(
        links[1].this_hash,
        chain.as_slice()[1].this_hash.clone(),
        "tamper test must not modify this_hash"
    );
}

// -- 6. canonical_json 字段顺序不敏感性 --

/// 测试 6：canonical_json 字段顺序不敏感性 —— key 顺序不同（裸 JSON 字面量
/// / HashMap-like / 嵌套对象）的输入必须输出相同字节。
///
/// 验证：
/// - 顶层 key 乱序 → 字节相同
/// - 嵌套对象 key 乱序 → 字节相同
/// - 数组元素顺序敏感（不同数组顺序 → 字节不同）—— canonical_json **不**
///   对数组重排（数组是有序语义）
/// - 输出无空格 / 无换行
///
/// 边界意义：保证 verify 在不同序列化器（serde_json preserve_order 开/关、
/// hashbrown HashMap 偶然渗入、不同 OS / libc）下跨环境可比；输出仅依赖
/// 字段**集合**而非字段**顺序**。
#[test]
fn test_chain_canonical_field_order_invariant() {
    // 顶层 key 乱序
    let a = json!({"a": 1, "b": 2, "c": 3});
    let b = json!({"c": 3, "a": 1, "b": 2});
    let c = json!({"b": 2, "c": 3, "a": 1});

    let bytes_a = canonical_json(&a).expect("canonical_json a");
    let bytes_b = canonical_json(&b).expect("canonical_json b");
    let bytes_c = canonical_json(&c).expect("canonical_json c");

    assert_eq!(
        bytes_a, bytes_b,
        "top-level key reordering must yield identical canonical bytes"
    );
    assert_eq!(
        bytes_a, bytes_c,
        "all permutations of top-level keys must be identical"
    );
    assert_eq!(bytes_a, b"{\"a\":1,\"b\":2,\"c\":3}");

    // 嵌套对象 key 乱序
    let nested_a = json!({"outer": {"z": 1, "a": 2}, "tag": "v1"});
    let nested_b = json!({"tag": "v1", "outer": {"a": 2, "z": 1}});
    let nested_c = json!({"outer": {"a": 2, "z": 1}, "tag": "v1"});

    let bytes_nested_a = canonical_json(&nested_a).expect("canonical_json nested_a");
    let bytes_nested_b = canonical_json(&nested_b).expect("canonical_json nested_b");
    let bytes_nested_c = canonical_json(&nested_c).expect("canonical_json nested_c");

    assert_eq!(
        bytes_nested_a, bytes_nested_b,
        "nested object key reordering must yield identical bytes"
    );
    assert_eq!(
        bytes_nested_a, bytes_nested_c,
        "all permutations of nested keys must be identical"
    );

    // 输出无空格 / 无换行
    let s = std::str::from_utf8(&bytes_a).expect("canonical_json must be valid UTF-8");
    assert!(!s.contains(' '), "canonical_json must contain no spaces");
    assert!(!s.contains('\n'), "canonical_json must contain no newlines");
    assert!(!s.contains('\t'), "canonical_json must contain no tabs");

    // 数组顺序敏感（数组是有序语义）
    let arr_a = json!({"items": [1, 2, 3]});
    let arr_b = json!({"items": [3, 2, 1]});
    let bytes_arr_a = canonical_json(&arr_a).expect("canonical_json arr_a");
    let bytes_arr_b = canonical_json(&arr_b).expect("canonical_json arr_b");
    assert_ne!(
        bytes_arr_a, bytes_arr_b,
        "array element order is semantically significant; canonical_json must preserve it"
    );

    // 实际链场景：将两条 payload（key 顺序不同）喂给 Chain.append，结果
    // 必须完全相同 —— 守住「链 hash 跨序列化器一致」硬约束
    let mut chain1 = Chain::new();
    chain1.append(&json!({"cl_id": 1, "actor": "dr_a", "note": "v1"})).unwrap();
    let link1_v1 = chain1.as_slice()[0].this_hash.clone();

    let mut chain2 = Chain::new();
    chain2
        .append(&json!({"note": "v1", "actor": "dr_a", "cl_id": 1}))
        .unwrap();
    let link1_v2 = chain2.as_slice()[0].this_hash.clone();

    assert_eq!(
        link1_v1, link1_v2,
        "Chain.append must produce identical hash regardless of input key order"
    );
}

// -- 7. RecoveryPoint 截断重算 + 不臆造尾部 --

/// 测试 7：RecoveryPoint 截断重算 + 不臆造尾部 —— 验证源
/// `copper-rs` StreamContinuityRecord::RecoveryPoint 契约的「接收方不臆造
/// 未观察的发送端尾部」语义。
///
/// 本模块 `verify_chain` 始终从 ZERO_HASH 起步（无自定义 prev 输入），所以
/// RecoveryPoint 衔接的真正语义落在三处：
///
/// 1. **`Chain::append` 自动衔接**：阶段 2 从阶段 1 末尾继续 append 时，
///    新链节的 `prev_hash` 由 Chain 内部 head 状态自动设置 —— 这等价于
///    「从 RP head 重新衔接」。本测试守住
///    `link_after_rp_1.prev_hash == RP head`。
///
/// 2. **手工重建一致性**：若接收方从 RP 起**手工**构造 ChainLink（不通过
///    `Chain::append`），其 `sha256(RP_head || canonical_json(payload))` 必须
///    与 `Chain::append` 产出的 `this_hash` **完全相等** —— 否则链节不可
///    跨实现拼接。
///
/// 3. **「不臆造尾部」硬约束**：若接收方拿到的链是从 RP 之后截断的（少了
///    RP 之前的所有链节），`verify_chain` 必显式报 PrevHashMismatch —— 这
///    是 RecoveryPoint 语义的真实表达（不允许静默通过）。
///
/// 边界意义：守住本模块在 RP 截断场景下的「衔接自动 / 重建一致 / 截断必败」
/// 三条契约。
#[test]
fn test_chain_recovery_point_recompute() {
    // 阶段 1：构造初始链 3 条（RecoveryPoint 之前的链）
    let mut chain_a = Chain::new();
    chain_a.append(&json!({"cl_id": 1, "actor": "dr_a@hosp_a"})).unwrap();
    chain_a.append(&json!({"cl_id": 2, "actor": "dr_b@hosp_b"})).unwrap();
    chain_a.append(&json!({"cl_id": 3, "actor": "dr_c@hosp_c"})).unwrap();

    // RecoveryPoint：捕获阶段 1 的 head_hash 作为锚点
    let rp_head_hex = hex_lower(chain_a.head_hash());
    let rp_head_bytes = hex_to_bytes(&rp_head_hex);
    assert_eq!(rp_head_hex.len(), 64);
    assert_ne!(
        rp_head_hex,
        hex_lower(&ZERO_HASH),
        "RP head must not be ZERO_HASH (3 links => non-trivial hash)"
    );

    // 阶段 1 整链 verify 通过
    assert!(verify_chain(chain_a.as_slice()).is_ok());

    // 阶段 2：从 chain_a 末尾继续 append 2 条 —— Chain 内部 head 自然衔接 RP
    let link_after_rp_1 = chain_a.append(&json!({"cl_id": 4, "actor": "dr_d@hosp_d"})).unwrap();
    let link_after_rp_2 = chain_a.append(&json!({"cl_id": 5, "actor": "dr_e@hosp_e"})).unwrap();

    // 关键断言 1：衔接后第一条 prev_hash == RP head
    assert_eq!(
        link_after_rp_1.prev_hash, rp_head_hex,
        "first post-recovery link's prev_hash must equal RecoveryPoint head"
    );

    // 关键断言 2：衔接后第二条 prev_hash == 第一条 this_hash
    assert_eq!(
        link_after_rp_2.prev_hash, link_after_rp_1.this_hash,
        "subsequent post-recovery links must continue chaining"
    );

    // 关键断言 3：head_hash 跟踪到最新链节
    assert_eq!(
        hex_lower(chain_a.head_hash()),
        link_after_rp_2.this_hash,
        "head_hash must track latest link after RP continuation"
    );

    // 关键断言 4：阶段 2 衔接后整链 5 条 verify 仍通过
    assert!(
        verify_chain(chain_a.as_slice()).is_ok(),
        "5-link chain after RP continuation must verify clean"
    );

    // 关键断言 5：从 RP 起手工构造 ChainLink 的 hash 与 Chain::append 一致
    //   这是「跨实现重建一致性」 —— 否则不同 sender/receiver 实现拼接会失败
    let manual_payload = json!({"cl_id": 4, "actor": "dr_d@hosp_d"});
    let manual_payload_bytes = canonical_json(&manual_payload).expect("canonical_json");
    let manual_hash_bytes = link_hash(&rp_head_bytes, &manual_payload_bytes);
    let manual_hash_hex = hex_lower(&manual_hash_bytes);

    assert_eq!(
        manual_hash_hex, link_after_rp_1.this_hash,
        "manual sha256(RP_head || canonical_json(payload)) must equal Chain::append's this_hash"
    );

    // 关键断言 6：手工 ChainLink 的 prev_hash 必须等于 RP head
    let manually_constructed = ChainLink {
        prev_hash: rp_head_hex.clone(),
        payload: manual_payload.clone(),
        this_hash: manual_hash_hex.clone(),
    };
    assert_eq!(
        manually_constructed.prev_hash, link_after_rp_1.prev_hash,
        "manual ChainLink's prev_hash must equal RP head"
    );
    assert_eq!(
        manually_constructed.this_hash, link_after_rp_1.this_hash,
        "manual ChainLink's this_hash must equal Chain::append output"
    );

    // 关键断言 7：「不臆造尾部」 —— 仅 post-RP 链节（缺 RP 之前）的 verify 必显式失败
    //   场景：接收方拿到的链只有 link_after_rp_1 + link_after_rp_2（缺 RP 之前的 link[0..2]）
    //   期望：verify_chain 报 PrevHashMismatch at index 0 —— 因为 link_after_rp_1.prev_hash
    //     = RP head，verify 期望 prev = ZERO_HASH → mismatch
    //   边界意义：本模块 verify 路径必须显式拒绝「无声通过」，避免出现「少几条
    //   链节还能 verify 通过」导致审计窗口漏算
    let post_rp_only: Vec<ChainLink> = vec![link_after_rp_1.clone(), link_after_rp_2.clone()];
    assert_eq!(post_rp_only.len(), 2, "post-RP-only chain must contain 2 links");
    assert_eq!(
        post_rp_only[0].prev_hash, rp_head_hex,
        "post_rp_only[0].prev_hash must equal RP head (since chain_a was continued from RP)"
    );
    assert_ne!(
        post_rp_only[0].prev_hash,
        hex_lower(&ZERO_HASH),
        "post_rp_only[0].prev_hash must NOT be ZERO_HASH (otherwise verify would silently pass)"
    );

    let err = verify_chain(&post_rp_only).unwrap_err();
    match err {
        ChainError::PrevHashMismatch { index, expected_prev, actual_prev } => {
            assert_eq!(
                index, 0,
                "post-RP-only chain must fail at index 0 (first link's prev_hash != ZERO_HASH)"
            );
            assert_eq!(
                expected_prev,
                hex_lower(&ZERO_HASH),
                "expected_prev must be ZERO_HASH (verify_chain's fixed starting state)"
            );
            assert_eq!(
                actual_prev, rp_head_hex,
                "actual_prev must equal RP head (the truncated chain's first prev_hash)"
            );
        }
        other => panic!("expected PrevHashMismatch at index=0, got {other:?}"),
    }

    // 关键断言 8：「不臆造尾部」 —— 缺中间链节（不是首尾）的 verify 必失败
    //   场景：原 5 条链删掉 link[2]（中间一条）后剩 [link0, link1, link3, link4]：
    //     - link[0] 正常（prev=ZERO_HASH）
    //     - link[1] 正常（prev=link0.this_hash）
    //     - link[3]（即 truncated[2]）prev_hash = link[2].this_hash，但 truncated[1] = link[1]
    //       所以 truncated[2].prev_hash != truncated[1].this_hash → PrevHashMismatch at index 2
    //   边界意义：守「链结构任何中间链节不可缺失」的硬约束 —— 「不臆造尾部」
    //     是更广义的「不臆造任何缺失链节」。
    let mut middle_missing: Vec<ChainLink> = chain_a.as_slice().to_vec();
    middle_missing.remove(2); // 删 link[2]
    assert_eq!(middle_missing.len(), 4);
    let err = verify_chain(&middle_missing).unwrap_err();
    match err {
        ChainError::PrevHashMismatch { index, .. } => {
            assert_eq!(index, 2, "missing middle link[2] must fail at index 2");
        }
        other => panic!("expected PrevHashMismatch at index=2, got {other:?}"),
    }

    // 反向断言：删掉「最后一条」实际可通过 verify（前缀仍合法）
    //   边界意义：明示 verify 链不能被用来「截断尾部」 —— 它只检查首条是否对齐
    //     ZERO_HASH + 后续衔接收尾。所以「少尾部」不报错（语义合理 —— 这是 verify
    //     路径上「不臆造尾部」的另一面：删最后 N 条，前缀照样过）。
    let tail_truncated: Vec<ChainLink> = chain_a.as_slice()[..4].to_vec();
    assert_eq!(tail_truncated.len(), 4);
    assert!(
        verify_chain(&tail_truncated).is_ok(),
        "truncating last link still produces a valid prefix; this is the symmetric property of verify_chain"
    );
}

/// 把 64 字符 hex 字符串解析为 32 字节数组（test_chain_recovery_point_recompute
/// 内部 helper）。
fn hex_to_bytes(hex_str: &str) -> [u8; 32] {
    let bytes = hex::decode(hex_str).expect("hex decode must succeed for valid 64-char input");
    assert_eq!(bytes.len(), 32, "hex decode must produce 32 bytes");
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    out
}