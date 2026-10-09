//! 链式 sha256 摘要（核心模块 E）。
//!
//! 链式哈希结构 `prev_hash || canonical_json(payload) → sha256` —— 把转诊单每条
//! `ReferralVersion` 的不可篡改证据沿 cl_id 串成单向链。导出 zip 内 `chain.txt`
//! 由它生成；`verify` 子命令靠它反算硬核对。
//!
//! ## 与源产品的差异
//!
//! - 源 `copper-rs/core/cu29_logstream/src/capture.rs::DigestWriter` 用 **blake3**
//!   （用 `blake3::Hasher::new()` 增量喂字节）。本任务按 `spec.md` §6 改成 **sha256**
//!   —— 行业合规偏好 + OpenSSL 自带 + 医务科审计无需复制代码依赖。
//! - 源是 bincode `Writer` trait 增量写；本任务是「canonical_json(payload) + 链」
//!   —— 公开字段级 `prev_hash` / `payload` / `this_hash`，医务科人工核签能直接
//!   读 `chain.txt` 文字版而不必解码二进制。
//!
//! ## 不变量
//!
//! - 链首条：`prev_hash = [0u8; 32]`（零哈希是链 anchor，区别于「有效哈希」）。
//! - `this_hash = sha256(prev_hash || canonical_json(payload))`。
//! - `payload` 序列化为 **canonical JSON**（BTreeMap 字典序 + 无空格 + 无 BOM），
//!   `serde_json` 的 `preserve_order` feature 已被 Cargo.toml 锁入，但 `BTreeMap`
//!   自身已保证 key 排序；为防 `HashMap` 偶发渗入再加一道「先转 BTreeMap 再序列化」
//!   的兜底序列化路径。
//! - `verify_chain` 必逐条重算；任一条 hash 不匹配 / prev_hash 与上一条 this_hash
//!   不衔接 → `Err(ChainError::...)`，UI 顶部 + 报告导出都标「tamper detected」。
//!
//! ## 公开 API
//!
//! - [`Chain`]：append-only 链；提供 `append`、`verify`、`as_slice`、`head_hash`。
//! - [`ChainLink`]：单条链节；`prev_hash` / `payload` / `this_hash` 三段都序列化。
//! - [`canonical_json`]：把任意 `Serialize` 类型转为字段排序固定、无空格的字节流。
//! - [`verify_chain`]：对 `&[ChainLink]` 全链路重算 + tamper 检测。
//! - [`ChainError`]：三类失败原因 —— `EmptyChain` / `PrevHashMismatch` /
//!   `ThisHashMismatch`。
//!
//! 与源 `DigestWriter` 的「单条 payload digest」不同：本任务聚焦**整条链**的可
//! 移植、可读、可审计；导出 `chain.txt` 即直接落盘这一组 `ChainLink`。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;

/// sha256 输出字节数。
pub const SHA256_BYTES: usize = 32;

/// 链首条 prev_hash 占位：32 字节全 0。任何合法 `this_hash` 都是 sha256(...) 输出，
/// 不可能等于全 0（概率 2^-256），故 0 串可作为链 anchor 哨兵使用。
pub const ZERO_HASH: [u8; SHA256_BYTES] = [0u8; SHA256_BYTES];

/// canonical JSON：把任意 `Serialize` 类型序列化为「字段排序固定 + 无空白」的字节流。
///
/// 实现要点：
/// - 把输入先转 `serde_json::Value`，把所有 `Value::Object` 内的 key 排序为 `BTreeMap`。
///   `serde_json` 默认对 `serde_json::Map` 走 `BTreeMap`（preserve_order feature
///   在 Cargo.toml 已开启，仅在 `Value` 路径无副作用）；本兜底强制把 `Map` 重建为
///   排序后的 `BTreeMap`，再 `serde_json::to_vec` 输出。
/// - 不插入任何空格 / 换行 / BOM —— 这是 `chain.txt` 与 verify 重算跨环境可比的
///   必要条件。
///
/// 异常：序列化失败 → `Err(String)`（chain 模块不引 `anyhow`，仅 String 报错）。
pub fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let v = serde_json::to_value(value).map_err(|e| format!("canonical_json: to_value failed: {e}"))?;
    let sorted = sort_value(v);
    serde_json::to_vec(&sorted).map_err(|e| format!("canonical_json: to_vec failed: {e}"))
}

/// 内部：递归把 `Value::Object` 内的所有 key 排序为 `BTreeMap`（ASCII 升序）。
/// 其他变体（Null / Bool / Number / String / Array）保持原值不变。
fn sort_value(v: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            let mut sorted: BTreeMap<String, Value> = BTreeMap::new();
            for (k, vv) in map.into_iter() {
                sorted.insert(k, sort_value(vv));
            }
            // BTreeMap → Value::Object(serde_json::Map)，serde_json 默认对 Map 排序输出
            Value::Object(serde_json::Map::from_iter(sorted.into_iter()))
        }
        Value::Array(arr) => Value::Array(arr.into_iter().map(sort_value).collect()),
        other => other,
    }
}

/// 计算单条链节的 hash：`sha256(prev_hash || canonical_json(payload))`。
pub fn link_hash(prev_hash: &[u8], payload: &[u8]) -> [u8; SHA256_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(prev_hash);
    hasher.update(payload);
    let out = hasher.finalize();
    let mut buf = [0u8; SHA256_BYTES];
    buf.copy_from_slice(&out);
    buf
}

/// 链节：单条不可篡改证据。
///
/// - `prev_hash`：上一条链节的 `this_hash`；链首条 = [`ZERO_HASH`]。
/// - `payload`：业务对象（通常是 `ReferralVersion`）；canonical_json 后入 hash。
/// - `this_hash`：`sha256(prev_hash || canonical_json(payload))`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChainLink {
    pub prev_hash: String,
    pub payload: serde_json::Value,
    pub this_hash: String,
}

impl ChainLink {
    /// 取 16 进制字符串（lowercase）。长度固定 64。
    pub fn this_hash_hex(&self) -> &str {
        &self.this_hash
    }

    /// `this_hash` 取前 12 字符：UI 右上元数据视图显示用（与 source digest 截断同形）。
    pub fn this_hash_short(&self) -> &str {
        &self.this_hash[..12]
    }
}

/// 链错误。三类覆盖 verify 全部失败形态：
/// - `EmptyChain`：`&[]` 传入 verify（无意义）；
/// - `PrevHashMismatch`：第 i 条的 `prev_hash` 与第 i-1 条的 `this_hash` 不衔接；
/// - `ThisHashMismatch`：重算的 hash 与链上 `this_hash` 不一致（payload 篡改）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainError {
    EmptyChain,
    PrevHashMismatch {
        index: usize,
        expected_prev: String,
        actual_prev: String,
    },
    ThisHashMismatch {
        index: usize,
        expected_this: String,
        actual_this: String,
    },
}

impl fmt::Display for ChainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChainError::EmptyChain => write!(f, "chain is empty; nothing to verify"),
            ChainError::PrevHashMismatch { index, expected_prev, actual_prev } => write!(
                f,
                "chain link[{index}] prev_hash mismatch: expected={expected_prev} actual={actual_prev}"
            ),
            ChainError::ThisHashMismatch { index, expected_this, actual_this } => write!(
                f,
                "chain link[{index}] this_hash mismatch (payload tamper?): expected={expected_this} actual={actual_this}"
            ),
        }
    }
}

impl std::error::Error for ChainError {}

/// append-only 链。
///
/// `append(value)` 把 value canonical_json 化后算 sha256(prev_hash || payload)，
/// 产出新 `ChainLink` 并推入尾部。`head_hash()` 给最新一条的 `this_hash`，供下一条
/// `append` 作为 prev_hash 输入。
#[derive(Default, Debug, Clone)]
pub struct Chain {
    links: Vec<ChainLink>,
    head: [u8; SHA256_BYTES],
}

impl Chain {
    pub fn new() -> Self {
        Self {
            links: Vec::new(),
            head: ZERO_HASH,
        }
    }

    pub fn len(&self) -> usize {
        self.links.len()
    }

    pub fn is_empty(&self) -> bool {
        self.links.is_empty()
    }

    pub fn as_slice(&self) -> &[ChainLink] {
        &self.links
    }

    pub fn head_hash(&self) -> &[u8; SHA256_BYTES] {
        &self.head
    }

    /// 追加一条 payload，返回新链节（同时被 push 进内部 Vec）。
    ///
    /// 行为：
    /// 1. canonical_json(value) → bytes
    /// 2. this_hash = sha256(head || bytes)
    /// 3. ChainLink { prev_hash: hex(head), payload: value, this_hash: hex(this_hash) }
    /// 4. head = this_hash
    pub fn append<T: Serialize>(&mut self, value: &T) -> Result<ChainLink, String> {
        let bytes = canonical_json(value)?;
        let this_hash = link_hash(&self.head, &bytes);
        let link = ChainLink {
            prev_hash: hex_lower(&self.head),
            payload: serde_json::to_value(value)
                .map_err(|e| format!("chain.append: to_value failed: {e}"))?,
            this_hash: hex_lower(&this_hash),
        };
        self.head = this_hash;
        self.links.push(link.clone());
        Ok(link)
    }
}

/// 整条链硬核对：逐条重算 `sha256(prev_hash || canonical_json(payload))`，
/// 与链上 `this_hash` 比对；同时校验 `link[i].prev_hash == link[i-1].this_hash`。
///
/// 返回 `Ok(())` 表示整链无篡改；任一处不一致即 `Err(ChainError)`。
pub fn verify_chain(links: &[ChainLink]) -> Result<(), ChainError> {
    if links.is_empty() {
        return Err(ChainError::EmptyChain);
    }
    let mut prev_bytes = ZERO_HASH;
    for (i, link) in links.iter().enumerate() {
        // 1) prev_hash 衔接
        let prev_hex = hex_lower(&prev_bytes);
        if link.prev_hash != prev_hex {
            return Err(ChainError::PrevHashMismatch {
                index: i,
                expected_prev: prev_hex,
                actual_prev: link.prev_hash.clone(),
            });
        }
        // 2) 重算 this_hash
        let payload_bytes = canonical_json(&link.payload)
            .map_err(|e| format!("link[{i}] canonical_json failed: {e}"))
            // verify 路径不直接产 ChainError；包成 ThisHashMismatch 表达「payload 反序列化也失败 = 数据被改」
            .map_err(|_| ChainError::ThisHashMismatch {
                index: i,
                expected_this: link.this_hash.clone(),
                actual_this: "<<canonical_json failed>>".to_string(),
            })?;
        let this_bytes = link_hash(&prev_bytes, &payload_bytes);
        let this_hex = hex_lower(&this_bytes);
        if link.this_hash != this_hex {
            return Err(ChainError::ThisHashMismatch {
                index: i,
                expected_this: link.this_hash.clone(),
                actual_this: this_hex,
            });
        }
        prev_bytes = this_bytes;
    }
    Ok(())
}

/// hex 编码（小写、64 字符）—— sha256 输出标准格式。
pub fn hex_lower(bytes: &[u8; SHA256_BYTES]) -> String {
    hex::encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- 链 step1 — 链长度 3 校验通过 --

    #[test]
    fn test_sha256_link() {
        let mut chain = Chain::new();
        let link1 = chain.append(&json!({"cl_id": 1, "actor": "dr_wang@hospital_a"})).unwrap();
        let link2 = chain.append(&json!({"cl_id": 2, "actor": "dr_li@hospital_b"})).unwrap();
        let link3 = chain.append(&json!({"cl_id": 3, "actor": "dr_zhao@hospital_a"})).unwrap();

        assert_eq!(chain.len(), 3);
        assert_eq!(link1.prev_hash, hex_lower(&ZERO_HASH));
        assert_eq!(link1.this_hash.len(), 64);
        // link2.prev_hash 必须等于 link1.this_hash
        assert_eq!(link2.prev_hash, link1.this_hash);
        assert_eq!(link3.prev_hash, link2.this_hash);
        // head = link3.this_hash
        assert_eq!(hex_lower(chain.head_hash()), link3.this_hash);

        // 整链 verify 通过
        assert!(verify_chain(chain.as_slice()).is_ok());
    }

    // -- 链 step2 — 篡改中间一条 hash 不匹配 --

    #[test]
    fn test_chain_tamper_detected() {
        let mut chain = Chain::new();
        chain.append(&json!({"cl_id": 1})).unwrap();
        chain.append(&json!({"cl_id": 2})).unwrap();
        chain.append(&json!({"cl_id": 3})).unwrap();

        // 在内存里手动篡改 link[1].payload（不动 prev_hash / this_hash）
        let mut links = chain.as_slice().to_vec();
        links[1].payload = json!({"cl_id": 2, "tampered": true});

        let err = verify_chain(&links).unwrap_err();
        match err {
            ChainError::ThisHashMismatch { index, .. } => assert_eq!(index, 1),
            other => panic!("expected ThisHashMismatch, got {other:?}"),
        }
    }

    // -- 篡改 prev_hash（不动 payload）应被 prev_hash mismatch 拒 --

    #[test]
    fn test_chain_tamper_prev_hash_detected() {
        let mut chain = Chain::new();
        chain.append(&json!({"cl_id": 1})).unwrap();
        chain.append(&json!({"cl_id": 2})).unwrap();

        let mut links = chain.as_slice().to_vec();
        // 把 link[1] 的 prev_hash 改成全 0（与 link[0].this_hash 不衔接）
        links[1].prev_hash = hex_lower(&ZERO_HASH);

        let err = verify_chain(&links).unwrap_err();
        match err {
            ChainError::PrevHashMismatch { index, .. } => assert_eq!(index, 1),
            other => panic!("expected PrevHashMismatch, got {other:?}"),
        }
    }

    // -- 篡改 this_hash（不动 payload）应被 this_hash mismatch 拒 --

    #[test]
    fn test_chain_tamper_this_hash_field_detected() {
        let mut chain = Chain::new();
        chain.append(&json!({"cl_id": 1})).unwrap();
        chain.append(&json!({"cl_id": 2})).unwrap();

        let mut links = chain.as_slice().to_vec();
        // 直接覆盖 link[0].this_hash 为伪造值
        links[0].this_hash = hex_lower(&[0xAA; SHA256_BYTES]);

        let err = verify_chain(&links).unwrap_err();
        match err {
            ChainError::ThisHashMismatch { index, .. } => assert_eq!(index, 0),
            other => panic!("expected ThisHashMismatch, got {other:?}"),
        }
    }

    // -- canonical_json 字段顺序不敏感性（key 乱序不破坏） --

    #[test]
    fn test_canonical_json_field_order_invariant() {
        let a = json!({"a": 1, "b": 2, "c": 3});
        let b = json!({"c": 3, "a": 1, "b": 2});
        let bytes_a = canonical_json(&a).unwrap();
        let bytes_b = canonical_json(&b).unwrap();
        assert_eq!(bytes_a, bytes_b, "key order must not change canonical bytes");
    }

    #[test]
    fn test_canonical_json_no_whitespace() {
        let v = json!({"x": 1, "y": [1, 2, 3]});
        let bytes = canonical_json(&v).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert!(!s.contains(' '), "canonical_json must have no spaces");
        assert!(!s.contains('\n'), "canonical_json must have no newlines");
    }

    #[test]
    fn test_canonical_json_nested_sort() {
        // 嵌套对象的 key 也必须排序
        let v = json!({"outer": {"z": 1, "a": 2}});
        let bytes = canonical_json(&v).unwrap();
        let s = std::str::from_utf8(&bytes).unwrap();
        // "a" 必须出现在 "z" 之前
        let pos_a = s.find("\"a\"").unwrap();
        let pos_z = s.find("\"z\"").unwrap();
        assert!(pos_a < pos_z, "nested keys must be sorted");
    }

    // -- 链 verify 空链 Err --

    #[test]
    fn test_verify_empty_chain_returns_err() {
        let err = verify_chain(&[]).unwrap_err();
        assert_eq!(err, ChainError::EmptyChain);
    }

    // -- 链首条 prev_hash 必须是 ZERO_HASH --

    #[test]
    fn test_first_link_prev_hash_is_zero() {
        let mut chain = Chain::new();
        let link = chain.append(&json!({"hello": "world"})).unwrap();
        assert_eq!(link.prev_hash, hex_lower(&ZERO_HASH));
        assert_eq!(link.prev_hash.len(), 64);
    }

    // -- head_hash 与最新链节 this_hash 一致 --

    #[test]
    fn test_head_hash_tracks_latest_link() {
        let mut chain = Chain::new();
        assert_eq!(hex_lower(chain.head_hash()), hex_lower(&ZERO_HASH));

        let link = chain.append(&json!({"v": 1})).unwrap();
        assert_eq!(hex_lower(chain.head_hash()), link.this_hash);

        let link2 = chain.append(&json!({"v": 2})).unwrap();
        assert_eq!(hex_lower(chain.head_hash()), link2.this_hash);
    }

    // -- this_hash_short 取前 12 字符 --

    #[test]
    fn test_chain_link_this_hash_short() {
        let mut chain = Chain::new();
        let link = chain.append(&json!({"k": "v"})).unwrap();
        assert_eq!(link.this_hash_short().len(), 12);
        assert!(link.this_hash.starts_with(link.this_hash_short()));
    }

    // -- Display impl 不空 --

    #[test]
    fn test_error_display_nonempty() {
        let cases = [
            ChainError::EmptyChain,
            ChainError::PrevHashMismatch {
                index: 2,
                expected_prev: hex_lower(&ZERO_HASH),
                actual_prev: "deadbeef".repeat(8),
            },
            ChainError::ThisHashMismatch {
                index: 1,
                expected_this: hex_lower(&[0xAA; SHA256_BYTES]),
                actual_this: hex_lower(&[0xBB; SHA256_BYTES]),
            },
        ];
        for e in cases {
            let s = format!("{e}");
            assert!(!s.is_empty());
            assert!(!s.contains("unknown"));
        }
    }

    // -- serde 往返：链节字段均可序列化反序列化 --

    #[test]
    fn test_chain_link_serde_round_trip() {
        let mut chain = Chain::new();
        let link = chain.append(&json!({"cl_id": 1, "note": "测试中文"})).unwrap();

        let s = serde_json::to_string(&link).unwrap();
        let back: ChainLink = serde_json::from_str(&s).unwrap();
        assert_eq!(back, link);
    }

    // -- 真实数据形态：ReferralVersion 字段级（结构体而非裸 JSON）-- --

    #[test]
    fn test_chain_with_struct_payload() {
        use crate::model::continuity_record::ContinuityKind;
        use crate::model::referral_version::ReferralVersion;
        use chrono::{TimeZone, Utc};
        use std::collections::BTreeMap;

        let mut before: BTreeMap<String, String> = BTreeMap::new();
        before.insert("diagnosis".to_string(), "chest pain".to_string());
        let mut after: BTreeMap<String, String> = BTreeMap::new();
        after.insert("diagnosis".to_string(), "ACS suspected".to_string());

        let v1 = ReferralVersion {
            cl_id: 1,
            version_no: 1,
            actor: "dr_wang@hospital_a".to_string(),
            modified_at: Utc.with_ymd_and_hms(2026, 10, 10, 0, 0, 0).unwrap(),
            before: before.clone(),
            after: after.clone(),
            continuity: ContinuityKind::Manifest,
            payload_sha256: "0".repeat(64),
        };

        let mut chain = Chain::new();
        let link1 = chain.append(&v1).expect("append v1");
        assert_eq!(link1.prev_hash, hex_lower(&ZERO_HASH));
        assert!(verify_chain(chain.as_slice()).is_ok());
    }
}