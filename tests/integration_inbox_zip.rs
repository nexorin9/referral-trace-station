//! inbox drop zip 解析器集成测试（Task 23）
//!
//! ## 范围
//!
//! 与 `src/inbox/zip_parser.rs` 内嵌单测互补——此处作为外部 integration test
//! 验证「跨模块边界」的契约，覆盖 5 类边界：
//!
//! 1. `test_inbox_zip_normal_ok`：三件套齐全（before.json + after.json +
//!    signature.txt） → 解析成功，字段、签名、actor 全验；
//! 2. `test_inbox_zip_signature_missing`：仅 before/after，缺 signature.txt →
//!    `Err(InboxZipError::SignatureMissing)`（spec.md §5 硬约束「drop 缺签 →
//!    拒绝入流」）；
//! 3. `test_inbox_zip_corrupted_zip`：65 字节非 zip 字节流 →
//!    `Err(InboxZipError::ZipOpen { .. })`（不是合法 zip 文件）；
//! 4. `test_inbox_zip_empty_zip`：合法 zip 但 0 entries →
//!    `Err(InboxZipError::MissingFile { name: "before.json" })`（首个必填
//!    entry 缺失即拒，**不**误判为 SignatureMissing 或静默 OK）；
//! 5. `test_inbox_zip_wrong_filenames`：zip 内 entry 是 patch.json / change.json
//!    / sig.txt 而非 before/after/signature →
//!    `Err(InboxZipError::MissingFile { name: "before.json" })`（按文件名精确
//!    匹配，**不**「宽松识别」）。
//!
//! ## 与 src/inbox/zip_parser.rs 内嵌单测的区别
//!
//! - 内嵌单测写在 lib 模块内（`#[cfg(test)] mod tests`），粒度是「主路径 + 缺签 +
//!   serde round-trip + Display」；
//! - 本文件作为 integration test，链接到 `referral_trace_station::inbox::*`，
//!   从外部视角覆盖 **完整 5 类边界**，包括坏 zip / 空 zip / 文件名不符这三类
//!   内嵌测试未覆盖的边界。
//!
//! ## 解析器流程（与失败顺序相关）
//!
//! `parse_inbox_zip` 的失败顺序是**线性的**：
//!
//! 1. 文件 IO 失败 / zip 解析失败 → `ZipOpen`；
//! 2. `before.json` 缺失 → `MissingFile { name: "before.json" }`；
//! 3. `after.json` 缺失 → `MissingFile { name: "after.json" }`；
//! 4. `signature.txt` 缺失 → `SignatureMissing`；
//! 5. JSON 解析失败 / 非顶层 object → `BadJson` / `NotAnObject`。
//!
//! 这就意味着：**空 zip（含 0 entries）会先撞 before.json 缺失**，而不是
//! SignatureMissing。**文件名不符同理**。两类边界测试因此断言
//! `MissingFile { name: "before.json" }`，守住「按文件名精确匹配 + 先来先拒」。
//!
//! ## fixtures
//!
//! 测试数据位于 `tests/fixtures/`：
//!
//! - `inbox_zip_ok.zip`（既有）：before/after/signature 三件套齐全，
//!   signature="signature_ok"；
//! - `inbox_signature_missing.zip`（既有）：before/after 但无 signature.txt；
//! - `inbox_zip_corrupted.zip`（新增，65 字节）：开头为 "NOT_A_VALID_ZIP_FILE"
//!   + 末尾 binary blob；zip crate 解析失败；
//! - `inbox_zip_empty.zip`（新增，22 字节）：合法 zip 容器 + 0 entries；
//! - `inbox_zip_wrong_filenames.zip`（新增，338 字节）：zip 内是
//!   `patch.json` / `change.json` / `sig.txt`，无任何 expected entry 名。

use std::path::Path;

use referral_trace_station::inbox::{parse_inbox_zip, InboxZipError};

fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

/// 测试 1：正常三件套（before.json + after.json + signature.txt）→ 解析成功。
///
/// 验证：
/// - 不返回 `Err`；
/// - `before` / `after` 字段值与 fixtures 的 `inbox_zip_ok.zip` 内容一致；
/// - `signature` 内容 = "signature_ok"（trim 后）；
/// - `actor` 字段从 before 改成 after → 这是 drop zip 表达「谁修改了
///   转诊单字段」的关键证据。
#[test]
fn test_inbox_zip_normal_ok() {
    let path = fixtures_dir().join("inbox_zip_ok.zip");
    let parsed = parse_inbox_zip(&path).expect("normal three-piece zip must parse");

    // before / after 关键字段（与 fixtures/inbox_zip_ok.zip 内容对齐）
    assert_eq!(
        parsed.before.get("diagnosis").and_then(|v| v.as_str()),
        Some("chest pain"),
        "before.diagnosis mismatch"
    );
    assert_eq!(
        parsed.before.get("actor").and_then(|v| v.as_str()),
        Some("dr_wang@hospital_a"),
        "before.actor mismatch"
    );
    assert_eq!(
        parsed.after.get("diagnosis").and_then(|v| v.as_str()),
        Some("ACS suspected"),
        "after.diagnosis mismatch"
    );
    assert_eq!(
        parsed.after.get("actor").and_then(|v| v.as_str()),
        Some("dr_li@hospital_b"),
        "after.actor mismatch"
    );

    // signature trim 后全文保留
    assert_eq!(parsed.signature, "signature_ok", "signature trim mismatch");
}

/// 测试 2：仅 before/after，缺 `signature.txt` → `Err(SignatureMissing)`。
///
/// spec.md §5 硬约束：drop 缺签 → 拒绝入流，绝不静默继续；`SignatureMissing`
/// 是该硬约束的单一出口。
#[test]
fn test_inbox_zip_signature_missing() {
    let path = fixtures_dir().join("inbox_signature_missing.zip");
    let err = parse_inbox_zip(&path).expect_err("missing signature must reject");

    match err {
        InboxZipError::SignatureMissing => {
            // 期望：spec.md §5 硬约束由 SignatureMissing variant 单一路径守护
        }
        other => panic!("expected SignatureMissing, got {other:?}"),
    }
}

/// 测试 3：65 字节非 zip 字节流 → `Err(ZipOpen { .. })`。
///
/// 关键边界：parser 必须在 IO 阶段就拒绝，不能流传到「after.json 缺失」之类
/// 误导性错误；`ZipOpen` 的 reason 含 zip crate 的真实报错信息。
#[test]
fn test_inbox_zip_corrupted_zip() {
    let path = fixtures_dir().join("inbox_zip_corrupted.zip");
    let err = parse_inbox_zip(&path).expect_err("corrupted bytes must reject");

    match err {
        InboxZipError::ZipOpen { path: p, reason } => {
            assert!(p.contains("inbox_zip_corrupted.zip"), "path mismatch: {p}");
            assert!(!reason.is_empty(), "reason must be non-empty: {reason}");
            // 守护：reason 不能误称 'unknown'（spec 一贯要求 Display 非空
            // 且不出现 'unknown' 兜底字样）。
            assert!(
                !reason.to_lowercase().contains("unknown"),
                "zip open reason must not say 'unknown': {reason}"
            );
        }
        other => panic!("expected ZipOpen, got {other:?}"),
    }
}

/// 测试 4：合法 zip 但 0 entries → `Err(MissingFile { name: "before.json" })`。
///
/// 边界要点：解析器按线性顺序扫描（before → after → signature），0 entries
/// 时**先**撞 before.json 缺失。`SignatureMissing` **不会**被触发，因为它
/// 在最后一道检查。这保证「drop 缺签」契约不会被「drop 完全为空」混淆。
#[test]
fn test_inbox_zip_empty_zip() {
    let path = fixtures_dir().join("inbox_zip_empty.zip");
    let err = parse_inbox_zip(&path).expect_err("empty zip must reject");

    match err {
        InboxZipError::MissingFile { name } => {
            assert_eq!(
                name, "before.json",
                "empty zip must surface the first missing entry as before.json; got {name}"
            );
        }
        // 守一道反向断言：绝不接受 SignatureMissing；空 zip 的 before 缺失
        // 优先于 signature 缺失。
        InboxZipError::SignatureMissing => {
            panic!("empty zip must not surface as SignatureMissing (signature check is last)")
        }
        other => panic!("expected MissingFile {{ before.json }}, got {other:?}"),
    }
}

/// 测试 5：zip 内 entry 是 patch.json / change.json / sig.txt 而非
/// `before/after/signature.txt` → `Err(MissingFile { name: "before.json" })`。
///
/// 关键边界：entry 命名**严格匹配**三个 expected 文件名（不区分大小写但**不**做
/// 模糊匹配）。即便 zip 内文件数 == 3，命名错位即拒绝；不接受「宽松识别」。
#[test]
fn test_inbox_zip_wrong_filenames() {
    let path = fixtures_dir().join("inbox_zip_wrong_filenames.zip");
    let err = parse_inbox_zip(&path).expect_err("wrong-filenames zip must reject");

    match err {
        InboxZipError::MissingFile { name } => {
            assert_eq!(
                name, "before.json",
                "wrong-filenames zip must surface the first missing expected entry; got {name}"
            );
        }
        InboxZipError::SignatureMissing => {
            panic!("wrong-filenames zip must not surface as SignatureMissing (before check is first)")
        }
        other => panic!("expected MissingFile {{ before.json }}, got {other:?}"),
    }
}
