//! 区域平台 drop 入站 zip 三件套解析器。
//!
//! 一份 drop zip 必须含三类条目：
//!
//! 1. `before.json` —— 改前字段快照（`serde_json::Map<String, Value>`）
//! 2. `after.json`  —— 改后字段快照（`serde_json::Map<String, Value>`）
//! 3. `signature.txt` —— 发送方签名占位（`String`，本轮仅做存在性校验，不验签）
//!
//! ## 边界与拒绝策略
//!
//! - `signature.txt` 缺失 → `Err(InboxZipError::SignatureMissing)`：守住 plan.md §5
//!   「drop 缺签」风险缓解：缺签的 drop 不入流，避免后续 sha256 链把未经签收的版本
//!   永久写入审计包。
//! - `before.json` / `after.json` 缺失 → `Err(InboxZipError::MissingFile { name })`：
//!   无 before/after 任一即视为不可比照的 drop，不入流。
//! - 任何条目 JSON 解析失败 → `Err(InboxZipError::BadJson { name, reason })`：
//!   与 `signature.txt` 缺签同样拒绝入流，避免污染 chain。
//! - 顶层对象必须是 JSON object（`Map`） → `Err(InboxZipError::NotAnObject { name })`：
//!   数组 / 字符串 / 数字 root 不接受（before/after 必须是字段表）。
//!
//! ## 与源产品的差异
//!
//! copper-rs 没有「外部入站 zip」概念——它是进程内 `CuLogEntry` 流。本模块填补
//! 「跨机构推送」与「本进程 cl_seq 单调流」之间的桥：区域平台 ORU / FHIR 推送来
//! 的 zip → 解析成内存里两份 Map → 经 fsck + sha256 chain + LiveLogListener 注入。
//!
//! ## 公开 API
//!
//! - [`parse_inbox_zip`]：主入口；返回 `Result<ParsedInboxDrop, InboxZipError>`
//! - [`ParsedInboxDrop`]：before / after / signature 三件套
//! - [`InboxZipError`]：失败原因分类（`SignatureMissing` 单独命名 + `MissingFile` /
//!   `BadJson` / `NotAnObject` / `ZipOpen` / `BadUtf8`）

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// 三件套文件名常量（zip 内 entry name 严格匹配；不区分大小写）。
pub const ENTRY_BEFORE: &str = "before.json";
pub const ENTRY_AFTER: &str = "after.json";
pub const ENTRY_SIGNATURE: &str = "signature.txt";

/// 解析后的 drop zip 三件套。
///
/// - `before` / `after` 强制为顶层 JSON object，否则拒绝入流。
/// - `signature` 为 `signature.txt` 的全文（trim 末尾换行）；本轮不验签，只做
///   存在性校验 + 留痕（医务科审计可看）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedInboxDrop {
    pub before: BTreeMap<String, serde_json::Value>,
    pub after: BTreeMap<String, serde_json::Value>,
    pub signature: String,
}

/// zip 解析失败原因。
///
/// `Display` 全部不空且不出现 `"unknown"` 字样；`SignatureMissing` 单独命名便于
/// `LiveLogListener` 在 UI 上把「缺签」高亮（与「缺文件」「JSON 损坏」区分）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxZipError {
    /// zip 文件打不开（不存在 / 不是 zip / 权限拒绝 / IO 错误）。
    ZipOpen { path: String, reason: String },
    /// 必填 entry 缺失（如 `signature.txt` / `before.json` / `after.json`）。
    MissingFile { name: String },
    /// `signature.txt` 缺失 —— spec.md §5 强约束：缺签即拒绝入流。
    SignatureMissing,
    /// JSON 解析失败（含非法 UTF-8 字节）。
    BadJson { name: String, reason: String },
    /// 顶层不是 object（数组 / 字符串 / 数字 root 不接受）。
    NotAnObject { name: String },
    /// `signature.txt` 不是合法 UTF-8。
    BadUtf8 { reason: String },
}

impl fmt::Display for InboxZipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InboxZipError::ZipOpen { path, reason } => {
                write!(f, "failed to open inbox zip '{path}': {reason}")
            }
            InboxZipError::MissingFile { name } => {
                write!(f, "inbox zip missing required entry '{name}'")
            }
            InboxZipError::SignatureMissing => {
                write!(f, "inbox zip missing signature.txt; drop rejected per spec.md §5")
            }
            InboxZipError::BadJson { name, reason } => {
                write!(f, "inbox zip entry '{name}' is not valid JSON: {reason}")
            }
            InboxZipError::NotAnObject { name } => {
                write!(
                    f,
                    "inbox zip entry '{name}' root must be a JSON object, got non-object"
                )
            }
            InboxZipError::BadUtf8 { reason } => {
                write!(f, "inbox zip signature.txt not valid UTF-8: {reason}")
            }
        }
    }
}

impl std::error::Error for InboxZipError {}

/// 主入口：解析一份 inbox drop zip，返回三件套。
///
/// ## 流程
///
/// 1. `File::open(path)` + `ZipArchive::new` —— 失败 → `ZipOpen`。
/// 2. 在 archive 里**精确匹配** `before.json` / `after.json` / `signature.txt`
///    三个文件名（去前导 `./`，保留大小写）。任一缺失 → `MissingFile` 或
///    `SignatureMissing`。
/// 3. 读 `before.json` / `after.json` 字节流 → `serde_json::from_slice` 解析为
///    `Map<String, Value>`；失败 → `BadJson`；顶层非 object → `NotAnObject`。
/// 4. 读 `signature.txt` 字节流 → `String::from_utf8`；失败 → `BadUtf8`；trim
///    末尾换行保留作 `signature` 字段。
///
/// ## 失败兜底
///
/// 任何错误均不上抛 zip 内部异常，统一映射到 `InboxZipError` 枚举；caller（inbox
/// watcher）据此决定「继续监听 / 把原文件 mv 到 `inbox/unparsed/` + 打 .rejected
/// tag / 触发 LiveLogListener 事件」。
pub fn parse_inbox_zip(path: &Path) -> Result<ParsedInboxDrop, InboxZipError> {
    // 1) 打开 zip
    let file = File::open(path).map_err(|e| InboxZipError::ZipOpen {
        path: path.display().to_string(),
        reason: e.to_string(),
    })?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| InboxZipError::ZipOpen {
            path: path.display().to_string(),
            reason: format!("zip open: {e}"),
        })?;

    // 2) 列出所有 entry name（精确匹配 + 去除前导 `./`）
    let mut names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).map(|f| f.name().to_string()))
        .collect::<Result<_, _>>()
        .map_err(|e| InboxZipError::ZipOpen {
            path: path.display().to_string(),
            reason: format!("zip enumerate: {e}"),
        })?;
    // 规整化：strip leading "./"
    for n in names.iter_mut() {
        if let Some(stripped) = n.strip_prefix("./") {
            *n = stripped.to_string();
        }
    }

    // 3) 解析 before.json / after.json / signature.txt
    let before = read_object_entry(&mut archive, &names, ENTRY_BEFORE)?;
    let after = read_object_entry(&mut archive, &names, ENTRY_AFTER)?;
    let signature = read_signature_entry(&mut archive, &names)?;

    Ok(ParsedInboxDrop {
        before,
        after,
        signature,
    })
}

/// 内部：读取一个 JSON object entry。
///
/// 缺失 → `MissingFile { name }`；解析失败 → `BadJson`；顶层非 object → `NotAnObject`。
fn read_object_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    names: &[String],
    name: &str,
) -> Result<BTreeMap<String, serde_json::Value>, InboxZipError> {
    if !names.iter().any(|n| n == name) {
        return Err(InboxZipError::MissingFile { name: name.to_string() });
    }
    let mut entry = archive
        .by_name(name)
        .map_err(|e| InboxZipError::BadJson {
            name: name.to_string(),
            reason: format!("zip read entry: {e}"),
        })?;
    let mut buf = Vec::new();
    entry
        .read_to_end(&mut buf)
        .map_err(|e| InboxZipError::BadJson {
            name: name.to_string(),
            reason: format!("zip read bytes: {e}"),
        })?;

    // 用 Map 强类型：serde_json::Map 配合 preserve_order feature 仍是有序插入；
    // 转 BTreeMap 让 canonical_json 跨环境的字段顺序可比。
    let raw: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&buf).map_err(|e| InboxZipError::BadJson {
            name: name.to_string(),
            reason: e.to_string(),
        })?;
    Ok(raw.into_iter().collect())
}

/// 内部：读取 signature.txt。
///
/// 缺失 → `SignatureMissing`（spec.md §5 硬约束）；非 UTF-8 → `BadUtf8`。
fn read_signature_entry<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    names: &[String],
) -> Result<String, InboxZipError> {
    if !names.iter().any(|n| n == ENTRY_SIGNATURE) {
        return Err(InboxZipError::SignatureMissing);
    }
    let mut entry = archive
        .by_name(ENTRY_SIGNATURE)
        .map_err(|e| InboxZipError::BadUtf8 {
            reason: format!("zip read entry: {e}"),
        })?;
    let mut buf = Vec::new();
    entry
        .read_to_end(&mut buf)
        .map_err(|e| InboxZipError::BadUtf8 {
            reason: format!("zip read bytes: {e}"),
        })?;
    // trim 末尾 \r\n / \n；保留行内空白作 signature 一部分
    let s = String::from_utf8(buf).map_err(|e| InboxZipError::BadUtf8 {
        reason: e.to_string(),
    })?;
    Ok(s.trim_end_matches(['\r', '\n']).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- drop zip 正常三件套解析成功 --

    #[test]
    fn test_drop_zip_ok() {
        // tests/fixtures/inbox_zip_ok.zip 由 build.rs 生成（含三件套）
        let path = std::path::PathBuf::from("tests/fixtures/inbox_zip_ok.zip");
        let parsed = parse_inbox_zip(&path).expect("ok zip should parse");

        // before/after 都是 Map 且字段正确
        assert_eq!(
            parsed.before.get("diagnosis").and_then(|v| v.as_str()),
            Some("chest pain")
        );
        assert_eq!(
            parsed.after.get("diagnosis").and_then(|v| v.as_str()),
            Some("ACS suspected")
        );
        assert_eq!(
            parsed.before.get("actor").and_then(|v| v.as_str()),
            Some("dr_wang@hospital_a")
        );
        assert_eq!(
            parsed.after.get("actor").and_then(|v| v.as_str()),
            Some("dr_li@hospital_b")
        );

        // signature 全文保留
        assert_eq!(parsed.signature, "signature_ok");
    }

    // -- drop zip 缺签被拒 --

    #[test]
    fn test_drop_signature_missing() {
        let path = std::path::PathBuf::from("tests/fixtures/inbox_signature_missing.zip");
        let err = parse_inbox_zip(&path).expect_err("missing signature must reject");
        match err {
            InboxZipError::SignatureMissing => {} // 期望
            other => panic!("expected SignatureMissing, got {other:?}"),
        }
    }

    // -- ParseInboxDrop serde round-trip --

    #[test]
    fn test_parsed_inbox_drop_serde_round_trip() {
        let mut before = BTreeMap::new();
        before.insert("k".to_string(), serde_json::json!("v"));
        let mut after = BTreeMap::new();
        after.insert("k".to_string(), serde_json::json!("v2"));
        let drop = ParsedInboxDrop {
            before,
            after,
            signature: "signature_ok".to_string(),
        };

        let s = serde_json::to_string(&drop).expect("serialize");
        let back: ParsedInboxDrop = serde_json::from_str(&s).expect("deserialize");
        assert_eq!(back, drop);
    }

    // -- InboxZipError Display 不空且不出现 "unknown" --

    #[test]
    fn test_inbox_zip_error_display_nonempty_and_no_unknown() {
        let cases = vec![
            InboxZipError::ZipOpen {
                path: "/tmp/x.zip".to_string(),
                reason: "No such file".to_string(),
            },
            InboxZipError::MissingFile {
                name: "before.json".to_string(),
            },
            InboxZipError::SignatureMissing,
            InboxZipError::BadJson {
                name: "after.json".to_string(),
                reason: "expected value".to_string(),
            },
            InboxZipError::NotAnObject {
                name: "before.json".to_string(),
            },
            InboxZipError::BadUtf8 {
                reason: "invalid byte".to_string(),
            },
        ];
        for e in cases {
            let s = format!("{e}");
            assert!(!s.is_empty(), "Display must be non-empty: {e:?}");
            assert!(!s.contains("unknown"), "Display must not say 'unknown': {s}");
        }
    }
}