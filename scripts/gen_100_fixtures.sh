#!/usr/bin/env bash
# gen_100_fixtures.sh — 为性能验收生成 100 份转诊单 .copper fixtures。
#
# 用法：
#   scripts/gen_100_fixtures.sh <output_state_dir> [count]
#
# 参数：
#   <output_state_dir>   必填，写入 <output_state_dir>/REF-NNN.copper（N=001..count）。
#   [count]              可选，默认 100；其中 referral_050 故意制造空洞（cl_id 序列 1,2,3,5,6）。
#
# 设计：
#   - 每份 referral 5 个版本（cl_id 1..5），modified_at 沿 cl_id 递增 1 秒。
#   - actor 按 cl_id % 2 在 hospital_a / hospital_b 间交替。
#   - diagnosis 按 cl_id 派生（diagnosis_stepN），便于人眼读。
#   - referral_050 删去 cl_id=4，模拟投递链路单条小跨度缺失 → SessionEnded reason。
#
# 本脚本只是"造数据"路径的辅助工具；性能验收的主基准由
# `tests/perf_100_referrals.rs` 负责（它用 tempdir 临时生成 100 份、不污染仓库）。
# 该脚本的用途是：当 audit / CI 想直接检查产物时，把 .copper 写到指定目录。

set -euo pipefail

OUT_DIR="${1:-}"
COUNT="${2:-100}"

if [ -z "$OUT_DIR" ]; then
    echo "usage: $0 <output_state_dir> [count]" >&2
    exit 2
fi

if ! command -v python3 >/dev/null 2>&1; then
    echo "python3 not found" >&2
    exit 3
fi

mkdir -p "$OUT_DIR"

python3 - <<PY
import hashlib, json, os, sys, datetime
out_dir = "${OUT_DIR}"
count = ${COUNT}
def sha256_hex(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()
def canonical_json(obj):
    # BTreeMap 字段排序 + 无空格：与 src/sha256_chain/chain.rs 的 canonical_json 对齐
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode("utf-8")

def make_version(referral_id, cl_id, actor, diagnosis):
    before = {"diagnosis": "chest pain"}
    after = {
        "diagnosis": diagnosis,
        "actor": actor,
        "modified_at": (datetime.datetime(2026,10,10,8,30,0) + datetime.timedelta(seconds=cl_id-1)).strftime("%Y-%m-%dT%H:%M:%SZ"),
    }
    payload_sha256 = sha256_hex(canonical_json(after))
    return {
        "cl_id": cl_id,
        "version_no": cl_id,
        "actor": actor,
        "modified_at": after["modified_at"],
        "before": before,
        "after": after,
        "continuity": {"Manifest": None},
        "payload_sha256": payload_sha256,
    }

def make_chain(versions):
    links = []
    prev = bytes(32)
    for v in versions:
        payload_bytes = canonical_json(v)
        this = sha256_hex(prev + payload_bytes)
        links.append({
            "prev_hash": prev.hex(),
            "payload": v,
            "this_hash": this,
        })
        prev = bytes.fromhex(this)
    return links

os.makedirs(out_dir, exist_ok=True)
for i in range(1, count + 1):
    n = f"{i:03d}"
    referral_id = f"REF-{n}"
    if i == 50:
        cl_ids = [1, 2, 3, 5, 6]
    else:
        cl_ids = [1, 2, 3, 4, 5]
    versions = []
    for cl_id in cl_ids:
        hospital = "b" if cl_id % 2 == 0 else "a"
        actor = f"dr_step{cl_id}@hospital_{hospital}"
        diagnosis = f"diagnosis_step{cl_id}"
        versions.append(make_version(referral_id, cl_id, actor, diagnosis))
    links = make_chain(versions)
    with open(os.path.join(out_dir, f"{referral_id}.copper"), "w") as f:
        for link in links:
            f.write(json.dumps(link, ensure_ascii=False))
            f.write("\n")
print(f"wrote {count} .copper fixtures to {out_dir}")
PY
