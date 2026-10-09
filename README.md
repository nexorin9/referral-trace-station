# 转诊单留痕回放工作台

> 上传一份转诊单的版本快照 → 自动按 cl_id 单调串联成时间线 → 一键导出 sha256 链 zip 给医务科签收 →
> 出院复诊或医保核查时回放任意时段每一步修改者与字段变化。

## 适用场景 / 目标岗位

| 岗位 | 什么时候用 | 得到什么 |
|------|------------|----------|
| 医务科转诊窗口 | 上游医院发起转诊、对方多次补件后 | 一条按 cl_id 倒序的版本时间线，每行写明谁在何时改了哪个字段 |
| 病案室编码员 | 出院后被医保或审计问到「这份转诊单哪一步是对方改的」 | 三秒定位到对应版本号与 sha256 摘要 + diff 视图 |
| 医保办对账 | 跨机构转诊与检查互认出现争议 | 按时间线导出整份 zip（manifest + 全部版本快照 + 链摘要），人工签收备查 |
| 信息科 | HIS REST 与区域 drop 同时给到同一份转诊单，互相不一致 | 工具沿用静态 task graph 自动判断哪条流占主、哪条走降级路径 |

> **表格纪律**：本表只写**岗位口语 + 业务时刻 + 可核对产出**。HL7 ORU、FHIR JSON、sha256 链、`POST /v1/chat/completions` 等技术细节一律放到「命令 / API / 配置说明」。

## 能力要点

- **静态 task graph 把转诊单生命周期锁住**：四阶段（created → modified → received → reviewed）在 Rust 结构体里静态声明，避免运行期被人为改动。
- **cl_id 单调计数器守住版本顺序**：每份转诊单独占一个 u64 计数器，重启后仍能从落盘状态续读；reorder 的事件直接被拒收。
- **fsck 序列空洞检测**：连续 ID 中出现遗漏时，立即在顶部状态条显示首尾号 + 缺失原因；超 256 个空洞报 `truncated`。
- **四态连续性记录**：Manifest / Gap / RecoveryPoint / Finished，对接区域平台 drop 时断网 / 重发不臆造尾部。
- **sha256 链式摘要**：`prev_hash || canonical_json(this)`，导出 zip 时 `chain.txt` 末尾带 `end of chain` 标记，verify 子命令可重算比对。
- **OpenAI-compatible ModelGateway**：结构化 prompt 给出 `{summary, fields_added, fields_removed, semantic_class}`；超时 / 解析失败自动降级到字段 diff；调用写入 `logs/audit.jsonl`。
- **replay 重放**：`cargo run -- replay --referral X` 把整份存档渲染回文本时间线，链不完整时显式提示 `RecoveryPoint required`。

## 快速开始

### 依赖

- Rust **≥ 1.74**（edition 2021；用于 `cargo build`）
- 仅无网络可达时：mock harness 自包含，无需真实 HIS / OpenAI
- 凭据一律从 `.env` 读取（不要硬编码）

### 安装

```bash
cd referral-trace-station
cp .env.example .env   # 填入 HIS_TOKEN / OPENAI_API_KEY（可留空走降级）
cargo build --release
```

### 配置

`.env` 里的关键项（详见「命令 / API / 配置说明」）：

| 配置项 | 默认 | 说明 |
|--------|------|------|
| `HIS_BASE_URL` | 空 | HIS REST 根 URL，留空则走 drop 目录路径 |
| `HIS_TOKEN` | 空 | 凭据，从环境变量注入，不进代码 |
| `OPENAI_BASE_URL` | 空 | ModelGateway 端点，留空则 `AI: off` |
| `OPENAI_API_KEY` | 空 | 同上；未配置时仅用字段 diff |
| `OPENAI_TIMEOUT_MS` | 4000 | 单次模型调用超时 |
| `RTS_INBOX_DIR` | `./inbox` | drop zip 热加载目录 |
| `RTS_STATE_DIR` | `./state` | 落盘 cl_seq 与 sha256 链 |

### 构建与验证

```bash
cargo build --release --manifest-path referral-trace-station/Cargo.toml
cargo test  --manifest-path referral-trace-station/Cargo.toml --no-run
cargo test  --manifest-path referral-trace-station/Cargo.toml -- --skip perf:: --skip e2e::   # 快速回归
```

### 主命令速查

```bash
cargo run -- trace                                            # 启动 ratatui 三视图
cargo run -- replay --referral referral_001                   # 命令行文本时间线回放
cargo run -- export --referral referral_001 --out bundle.zip  # 导出签收包
cargo run -- verify --zip bundle.zip                          # 重算 sha256 链 + tamper 检测
```

## 命令 / API / 配置说明

### 关键配置项

| 配置项 | 默认值 | 说明 |
|--------|--------|------|
| `HIS_BASE_URL` | 空 | GET `/referrals/{id}/snapshot` 基址 |
| `HIS_TOKEN` | 空 | HIS 鉴权 token；缺则立即 `HisCredentialMissing` |
| `REFERRAL_BASE_URL` | 空 | 区域平台 drop 备用基址 |
| `OPENAI_BASE_URL` | 空 | ModelGateway 接入点；OpenAI-compatible |
| `OPENAI_API_KEY` | 空 | 模型凭据；缺则降级到字段 diff |
| `OPENAI_MODEL` | `gpt-4o-mini` | 模型名，写入每次 audit 行 |
| `OPENAI_TIMEOUT_MS` | `4000` | 超时；超时即触发降级 |
| `RTS_INBOX_DIR` | `./inbox` | notify-rs watcher 监听目录 |
| `RTS_STATE_DIR` | `./state` | cl_seq 与 chain 落盘目录 |
| `RTS_DEBOUNCE_MS` | `100` | watcher debounce |

### CLI 子命令 / 接口端点

| 命令或路径 | 用途 | 关键参数 |
|------------|------|----------|
| `cargo run -- trace` | 启动 ratatui 三视图常驻监听 | 无 |
| `cargo run -- replay --referral X` | 命令行回放时间线 | `--referral` 转诊单 ID |
| `cargo run -- export --referral X --out zip` | 导出签收包 | `--referral` `--out` |
| `cargo run -- verify --zip path` | 重算链 + tamper 检测 | `--zip` |
| `POST {HIS_BASE_URL}/referrals/{id}/snapshot` | HIS 拉快照 | `Authorization: Bearer ${HIS_TOKEN}` |
| `POST {OPENAI_BASE_URL}/v1/chat/completions` | ModelGateway 调用 | `Authorization: Bearer ${OPENAI_API_KEY}` |
| `GET  {REFERRAL_BASE_URL}/drop/{id}.zip` | drop 入站（被动接收） | 无，watcher 监听 `RTS_INBOX_DIR` |

### 错误与降级

| 场景 | 表现 | 处理方式 |
|------|------|----------|
| HIS 5xx 或 4s 超时 | StatusBar 标 `HIS: down` | 自动回退到 `RTS_INBOX_DIR` drop zip |
| HIS 凭据缺失 | `HisCredentialMissing` | 直接走 drop，不发请求 |
| OpenAI 不可达 | `AiUnavailable` 事件广播 | DiffView 上半部分仍展示字段 diff |
| OpenAI 返回 JSON 缺字段 | 校验失败 → 不写回 | 降级 diff + 写 `audit.jsonl: parse_failed` |
| drop zip 缺 `signature.txt` | `SignatureMissing` 拒绝入流 | 不推进 cl_seq，写入 `inbox/unparsed/` |
| 上游 cl_id 倒退 | `observe()` 返回 `Err` | 写入 `logs/audit.jsonl`，不污染状态 |
| fsck 超 256 空洞 | 顶部标 `truncated` | 仍真实上报，导出 zip 时同步带标记 |

### 对接现有系统

- **HIS**：`GET /referrals/{id}/snapshot`（REST / JSON），`Authorization: Bearer ${HIS_TOKEN}`，超时 4000ms；仅在他院 HIS 主动暴露该端点时启用，缺则直接走 drop 目录。
- **区域平台**：被动接收对方推入 `inbox/<date>/*.zip`；zip 三件套 `before.json` + `after.json` + `signature.txt`，signature 缺失即拒收。
- **HL7 ORU**：仅解析 MSH/PID/OBR/OBX 四段定长字段；解析失败的 `.hl7` 落入 `inbox/unparsed/` 并打 `.unparsed` tag。
- **FHIR JSON**：仅取 `referral.id` / `referral.modification.timestamp` / `referral.modification.actor` 三关键字段，缺字段返回 `FhirMissingField`。
- **本工具不直写 HIS**：所有产物（`bundle.zip` / `replay.txt`）落到本地或由医务科人工签收后上传。

## 典型场景

### 场景 1：医务科收件台收到上游医院补件

1. 上游医院把 `bundle_referral_001.zip` 推到 `inbox/`。
2. notify-rs watcher 100ms 内检测到新文件 → 解析 before/after → 写入 cl_seq → sha256 链 append 一节 → LiveLog 广播 `version_added`。
3. ratatui 顶部状态条 `Versions +1`，左侧 Timeline 增加一行；中间 DiffView 列出字段 diff。

```bash
cargo run -- trace
# 观察 Logs： inbox watcher → version_added (cl_id=6) → chain extended → digest ok
```

### 场景 2：医保核查发现一次「双方记录不一致」

1. 医保办要求复盘 referral_003 全过程。
2. 跑 `replay` 命令行回放整份时间线，命中 fsck 已检测到的第 4 步空洞。
3. `export` 出 `bundle.zip`，内含 `manifest.json` + `versions/cl_*.json` + `chain.txt`。

```bash
cargo run -- replay --referral referral_003 | tee replay.txt
cargo run -- export --referral referral_003 --out /tmp/bundle.zip
unzip -l /tmp/bundle.zip
```

### 场景 3：信息科验证导出包未被改动

1. 拿到医务科给的 `bundle.zip`，本地跑 `verify` 重算 sha256 链。
2. 输出 `VerifyReport { chain_ok: true, total_versions: 8, gaps: [] }`，确认未被篡改。
3. 若中间被人改过 `chain.txt` 或 `versions/*.json`，`verify` 报 `tamper detected` 并指出具体序号。

```bash
cargo run -- verify --zip /tmp/bundle.zip
# VerifyReport { chain_ok: true, total_versions: 8, gaps: [] }
```

## 输出样例

### ratatui Trace 主视图（终端会话）

| 三视图内容 | 说明 |
|------------|------|
| 顶部 StatusBar | referral_id · 总版本数 · fsck 空洞数 · `AI: on/off` · 当前选中 cl_id |
| 左 Timeline | 按 cl_id 倒序版本列表，每行 `cl_id · modified_at · actor · sha256[:12]` |
| 中 DiffView | 上半字段 diff（similar），下半 AI summary（可切 tab） |
| 右 MetaView | referral_id / actor / modified_at / payload_sha256[:12] |

（`docs/screenshots/trace-tui.png`：真实终端截图，`## 概念 / 实测对照` 小节引用）

### replay 输出（文本）

```
cl_id=8  2026-10-10T03:21:11Z  上游医院A  fields_changed=3  sha=3a91..f8
cl_id=7  2026-10-10T03:18:02Z  本院医师站  fields_changed=1  sha=7c2e..11
cl_id=6  2026-10-10T03:11:55Z  上游医院A  fields_changed=5  sha=e10a..bc
...
RecoveryPoint required? false
end of chain
```

### verify 输出

```
VerifyReport { chain_ok: true, total_versions: 8, gaps: [], truncated: false }
```

## 概念 / 实测对照

### 视觉目标

起始态：ratatui 黑底青字三视图，顶部 StatusBar 中部 `Holes 0` 绿色；左列空列表。
首个可用瞬间：第一个 drop zip 解析成功，左列出现一行 `cl_id=1`，中列显示 before/after 字段 diff，右列显示 `payload_sha256` 前 12 字符；AI 默认关闭，下半显示 `AI: off`。
成形态：8 步版本 × 5 份转诊单同时跑通，StatusBar 显示 `Holes 1 (GAP between 4..6)` 黄色；中列 DiffView 上半字段 diff + 下半 AI summary 标签页可来回切。

（`docs/concepts/` 中放结构化视觉目标卡；若运行环境具备图像能力可附加概念图，无图则只写结构化说明，不伪造。）

### 实测证据

`trace`（ratatui 三视图真实渲染）：

![trace-tui — referral_001 健康 5 步，AI: on，fsck holes: 0](docs/screenshots/trace-tui.png)

![trace-tui-with-hole — referral_003 故意缺 cl_id=4，fsck holes: 1 黄色告警](docs/screenshots/trace-tui-with-hole.png)

- `docs/screenshots/trace-tui.txt` / `trace-tui-with-hole.txt` —— 同款 frame 的纯文本 dump（120x20 cells；用于文档 diff / grep）
- `docs/screenshots/replay-output.txt` —— `cargo run -- replay --referral referral_001` 真实输出（exit 0）
- `docs/screenshots/export-bundle.txt` —— `cargo run -- export --referral referral_001 --out /tmp/bundle.zip` + `unzip -l` 真实输出
- `docs/screenshots/verify-output.txt` —— `cargo run -- verify --zip /tmp/bundle.zip` 真实输出（chain_ok: true, exit 0）
- `docs/screenshots/fsck-gap-report.txt` —— `cargo run --example fsck_demo -- --referral referral_003` 真实输出（fsck 报 SessionEnded 空洞 first_id=4/last_id=4）

> **诚实标注**：左 / 上为视觉目标（`docs/concepts/`），右 / 下为真实实测。PNG 由 ratatui TestBackend 120x20 cells → 文本文本 → `scripts/trace_text_to_png.py` 用 Menlo 14pt 1:1 渲染，**未做任何美化、滤镜或样式补充**。目标与实测在**配色优先级**上有微小差距（ratatui StatusBarSeverity 实际配色会按 Yellow/Red/Green 三档精调，PNG 渲染仅为绿/黄/蓝基础配色），且 ratatui 真实终端花色/反走样/字号与静态 PNG 不同。**不**声称两者完全一致，也不生成「事后效果图」来抹平差距。

> **诚实标注**：左 / 上为视觉目标，右 / 下为真实实测。目标与实现在视觉上**必然有差距**（配色、字号、布局依赖终端实际渲染），本节不声称两者完全一致，也不生成事后效果图。

## 架构与数据流

```
┌──────────────────┐    ┌──────────────────────┐    ┌────────────────────┐
│ 上游 HIS (REST)  │───▶│  HisClient (reqwest) │───▶│                    │
└──────────────────┘    └──────────────────────┘    │                    │
                                                     │   EventBus         │
┌──────────────────┐    ┌──────────────────────┐    │  (mpsc+broadcast)  │
│ 区域平台 HL7/    │───▶│ InboxWatcher(notify) │───▶│                    │
│ drop zip         │    │  + parse_zip/oru     │    └─────────┬──────────┘
└──────────────────┘    └──────────────────────┘              │
                                                               ▼
                                  ┌────────────────────────────────────┐
                                  │ ClIdStore (cl_id 单调)             │
                                  │ sha256 Chain (链式摘要)            │
                                  │ StreamContinuityRecord (四态)      │
                                  │ LiveLogRegistry (订阅广播)         │
                                  └─────────────────┬──────────────────┘
                                                    │
                       ┌────────────────────────────┼────────────────────┐
                       ▼                            ▼                    ▼
            ┌──────────────────┐         ┌──────────────────┐    ┌──────────────────┐
            │  ratatui Trace   │         │  ReplayRunner    │    │  BundleWriter    │
            │  (三视图 + 状态条) │         │  (渲染 timeline) │    │  (manifest+链)    │
            └──────────────────┘         └──────────────────┘    └──────────────────┘
```

### 关键不变量

- **cl_id 单调**：每份转诊单独占一个 u64 计数器，重启后从落盘续读；倒退事件被 `observe()` 拒收，不污染状态。
- **ContinuityRecord 四态闭合**：Gap 必须三字段齐全；RecoveryPoint 后尾部不臆造；Finished 不可再追加。
- **sha256 链单向**：仅 `Chain::append` 增加节点；任何修改 `payload` 或 `prev_hash` 都被 `verify_chain` 检测。
- **降级不静默写回**：HIS 5xx / AI 不可达时只走旁路 + 写 `audit.jsonl`；不推进 cl_seq、不写 chain 节点。
- **drop 缺签即拒**：`signature.txt` 缺失返回 `SignatureMissing`，原文件入 `inbox/unparsed/` 不入流。

## 安全与合规边界

本工具**不**直接写入 HIS / EMR / 任何院内业务库；所有产物落到本地或经医务科人工签收后流转。
涉及下游消费时由 HIS 信息科或医保办人员对 `bundle.zip` 做人工确认。本工具不输出诊断建议，不代签电子签名。
AI 摘要（`ai_role=assist`）仅作字段变更归因草稿，需人工复核；模型调用元数据（`input_hash` / `output` / `model` / `latency_ms` / `status_code`）一律写入 `logs/audit.jsonl` 留痕。

## 验收与自检

| 验收项 | 命令 | 期望结果 |
|--------|------|----------|
| 子目录与顶层 project 对齐 | `test -d referral-trace-station && grep -q 'referral-trace-station' task.json` | 通过 |
| README 完整骨架 | `grep -c '^## ' README.md ≥ 10`、`grep -c '^### ' README.md ≥ 8` | 通过 |
| qrcode 关注区块存在 | `grep -q '## 关注我们' README.md && grep -q 'qrcode.jpg' README.md` | 通过 |
| `cargo build` 通过 | `cargo build --manifest-path referral-trace-station/Cargo.toml` | Finished release |
| fsck 边界用例 | `cargo test --manifest-path referral-trace-station/Cargo.toml --test fsck_boundaries` | 6/6 passed |
| 性能验收 | `cargo test --release --test perf_100_referrals -- --nocapture` | elapsed ≤ 30s |
| 端到端 3 转诊单 | `cargo test --test integration_end_to_end` | referral_001/002 verify 通过，referral_003 报空洞 |
| 验收故事 5 步 | `cargo test --test acceptance_spec` | 5/5 PASS，写入 `logs/acceptance.log` |

## 项目结构

```
referral-trace-station/
├── Cargo.toml                      # 包元数据（name=referral-trace-station、edition=2021）
├── .env.example                    # 凭据位样例（与 .env 同形）
├── .gitignore                      # 排除 target/、.env、state/、logs/、score.json 等
├── README.md                       # 产品说明（本文件）
├── qrcode.jpg                      # 公众号二维码（关注区块引用）
├── LICENSE                         # MIT
├── docs/
│   ├── concepts/                   # 视觉目标（结构化卡片 / 概念图）
│   └── screenshots/                # 实测证据（trace / replay / export / verify / fsck 真实输出）
├── tests/
│   └── fixtures/                   # 转诊单 + HL7 + FHIR + inbox drop zip 脱敏样例
└── src/
    ├── main.rs                     # 入口、clap CLI 装配、tokio runtime
    ├── model/                      # Referral / ReferralVersion / ContinuityRecord
    ├── task_graph/                 # 静态 4 阶段 Rust 结构体表
    ├── copperlist/                 # cl_seq 单调 + fsck 空洞检测
    ├── continuity/                 # StreamContinuityRecord 四态适配
    ├── live_log/                   # LiveLogListener 注册/反注册/广播
    ├── sha256_chain/               # 链式 sha256 摘要
    ├── inbox/                      # drop zip 解析 + notify-rs watcher
    ├── his/                        # REST 拉快照 + FHIR 字段映射
    ├── hl7/                        # ORU^R01 简版解析 + unparsed fallback
    ├── ai/                         # ModelGateway + mock 协议替身
    ├── event_bus/                  # tokio mpsc + broadcast 双通道
    ├── ui/                         # ratatui StatusBar / DiffView / Timeline / App run loop
    ├── replay/                     # .copper 存档解析 + 链校验 + timeline 渲染
    ├── export/                     # zip manifest + versions/*.json + chain.txt
    └── verify/                     # 重算 chain + tamper 检测
```

## License

MIT

---

## 关注我们

欢迎扫码关注公众号，获取项目更新与交流加群：

![关注我们](qrcode.jpg)