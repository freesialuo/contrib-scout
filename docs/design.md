# contrib-scout —— 面向多 Agent 的开源贡献发现工具（设计）

名称：`contrib-scout`（GitHub 公开仓库 `freesialuo/contrib-scout`；crates.io 上未被占用；npm 包为 `@freesialuo/contrib-scout`）。

## Context

你想让多个 Agent 去找"愿意接受外部 PR 的开源项目"，但不希望每个 Agent 开工前都各自高频访问 GitHub API。

dwave（`freesialuo/dwave`）已经验证过一套做法：本地 SQLite 共享缓存、TTL 新鲜度、ETag、请求预算、跨进程锁、JSON 契约加 skill。但它绑定 Drips Wave，而且限流只在单个进程内生效。

已确认的决定：
- 复制 dwave 的模式，另起新工具，**不与 dwave 共享 crate**。
- **首要信号是"不接受 AI 贡献"**，这类结论的 TTL 要长。
- 发现阶段可以用缓存；**开工前和提 PR 前必须看 GitHub 上的原始实时数据**。
- 复用 `gh`。

状态：设计已确认（2026-11-25），尚未实现。

目标是做到四点：发现阶段尽量不访问网络；关卡（gate）阶段一定访问网络，并且失败即拒绝（fail closed）；多个 Agent 共用同一份数据、同一份配额和同一张认领表；不另起服务。

## 1. 目标 / 非目标

目标
- **AI 政策标记**：由 Agent 读完原文后写入，按仓库或组织保存，附证据（文件、链接、原文摘录）。以后任何命令访问到这个仓库时都附带提醒。
- **仓库画像**：元数据、可贡献性指标（外部 PR 合并率、维护者首次回复的中位时间、最近活跃时间）。
- **issue 索引**：增量同步，候选 issue 在本地筛选。
- **issue 标记**：Agent 读过某个 issue 后，可以把它标为"不考虑"并写明原因；以后的发现和候选列表默认跳过它，不必再读一遍。
- **两道实时关卡**：`gate start` 和 `gate pr`，输出原始 GitHub 数据，供 Agent 自己阅读。
- **多 Agent 协调**：本机只有一个数据库；按 key 只拉一次；配额跨进程共享；认领带租约。
- 稳定的 `--json` 契约加 skill。核心库的 API 设计成以后能加一层很薄的 MCP。

非目标
- 不做常驻服务或守护进程，不做托管缓存。定时刷新交给 Windows 计划任务或 cron。
- 不对 GitHub 写入：不评论、不开 PR、不认领远端 issue。只读。
- 不镜像整个 GitHub，也不全量扫描大仓库。
- 现在不做 MCP，也不做 Web 界面或 TUI。

反目标（即使能跑通也算失败）
- CLI 自己去判定一个仓库是否接受 AI 贡献。判定由 Agent 读完原文后写入，CLI 只负责提供原文、保存标记和提醒。
- 已有标记的仓库再次被访问时没有提醒。
- 关卡阶段用缓存冒充实时数据。
- 多个 Agent 加起来触发 GitHub 的次级限流。

## 2. 已核实的外部事实（2026-11-25）

| 事实 | 设计后果 |
|---|---|
| 带认证的 304 不消耗主配额（每小时 5000 次），但仍计入次级限流 | 关卡用条件请求，304 视为"已实时确认"。全局并发和频率仍需限制 |
| `gh api -i` 输出状态行和响应头；**遇到 304 时退出码为 1**，并在 stderr 打印 `gh: HTTP 304` | gh 传输层必须解析状态行，不能依赖退出码。弱 ETag（`W/"…"`）要原样回传 |
| `gh api --cache` 只按 TTL 缓存，缓存放在 `$TMPDIR/gh-cli-cache`，不会主动清理 | 不使用。缓存由我们自己的存储负责 |
| 搜索 API 每分钟 30 次，单个查询最多返回 1000 条 | 搜索只用于发现，单独限速，结果缓存 24 小时 |
| AI 政策出现的位置：`AI_POLICY.md`、`LLM_POLICY.md`、`CONTRIBUTING.md`（根目录、`.github/`、`docs/`）、`AGENTS.md`、`CLAUDE.md`、`.github/copilot-instructions.md`、README 章节、PR 模板 | 政策探测要覆盖这些路径 |
| **政策可能定义在组织级的 `{org}/.github` 仓库**，例如 astral-sh 的政策覆盖 uv、ruff 等所有仓库 | 先查组织，再查仓库。组织结论可以被仓库内的政策覆盖 |
| 有些政策只限制特定 issue 类型：LLVM 和 polars 禁止用 AI 做 good first issue；clap 要求先在 issue 里获得维护者许可 | 立场不能只是一个枚举值，还要记录"范围限制"，在关卡阶段按 issue 的标签判定 |
| 常见限制：PR 描述和回复必须由人亲手写；禁止自主 Agent；必须声明 AI 使用（如 `Assisted-by:` 提交尾注） | `gate pr` 输出一份必须遵守的要求清单 |
| 有些政策在仓库之外，例如 Zig 官网、Gentoo wiki | 仓库内找不到不代表没有政策。可以导入外部清单作为种子（可选，见 §11），也支持手动记录 |
| 关键词匹配的误判率高：一项 703 仓库的调查中，18 个命中禁止性措辞的仓库，只有 7 个确实与 AI 有关 | CLI 不做分类。只把候选政策文件的原文交给 Agent，由 Agent 判断后写回标记 |

## 3. 架构

```
contrib-scout CLI ─► Planner ─► Store (SQLite, WAL) ◄─ Profiler / Indexer / Gate
                                  ▲                     │
                         Coordinator (锁, 配额, 认领)    ▼
                                  └────────── Gateway (预算, 限速, 重试)
                                                      │
                                       Transport trait ┼─ GhTransport   (`gh api -i` 子进程, 默认)
                                                      └─ HttpTransport (reqwest + `gh auth token`, 备选)
                                                      └─ FixtureTransport (测试)
```

Cargo workspace：
```
contrib-scout/
  crates/contrib-scout-core/ # 库：除参数解析和渲染外的全部逻辑
  crates/contrib-scout/  # 二进制：clap、渲染
  skills/contrib-scout/      # Claude skill
```

从 dwave 复制并改写的部分（不共享依赖）：
- `wave-core/src/gateway.rs`：主机策略、令牌桶、重试、单次调用预算、各项计数
- `wave-core/src/store.rs`：WAL、`busy_timeout`、基于 `fs2` 的 `lock_scope` 文件锁、只向前的迁移
- `wave-core/src/github.rs`：ETag 缓存页、`Link` 分页、同源校验（`safe_api_url`）、timeline 里的 PR 事件解析（`apply_pr_event`）、token 获取（`github_token`）
- `wave-core/src/config.rs`：TOML 配置和平台目录
- `dwave/src/render.rs`：版本化的 JSON 信封
- `scripts/`、`.github/workflows/`：npm 原生二进制发布和 CI

## 4. 核心抽象（签名仅示意）

```rust
/// 唯一的出网通道。默认实现：gh 子进程。
trait Transport {
    fn get(&self, req: &ApiRequest) -> Result<ApiResponse>;   // status, headers, body
    fn graphql(&self, query: &str, vars: &Value) -> Result<ApiResponse>;
}

/// 对所有资源统一做：预算 → 全局配额 → 令牌桶 → 发送 → 按状态码归类（2xx/304/可重试/致命）→ 重试。
struct Gateway { transport, policies: HostPolicies, budget, metrics, quota: SharedQuota }

/// 跨进程协调，全部落在 SQLite 加文件锁上。
trait Coordinator {
    fn single_flight(&self, key: &CacheKey, wait: Duration) -> Result<Option<FlightGuard>>;
    fn reserve(&self, resource: Resource, n: u32) -> Result<Reservation>; // 共享配额
    fn claim(&self, issue: &IssueRef, agent: &AgentId, lease: Duration) -> Result<ClaimOutcome>;
    fn release(&self, issue: &IssueRef, agent: &AgentId) -> Result<()>;
}

trait PolicyFiles {   // 只取原文，不做判断
    fn fetch(&self, repo: &RepoName, mode: Freshness) -> Result<Vec<PolicyFile>>; // path, url, text
}
trait PolicyMarks {   // Agent 写入的标记
    fn get(&self, repo: &RepoName) -> Result<Option<PolicyMark>>;   // 先查仓库，再查组织
    fn set(&self, mark: &PolicyMark) -> Result<()>;
    fn unset(&self, repo: &RepoName) -> Result<()>;
}
trait RepoProfiler { fn profile(&self, repo: &RepoName, mode: Freshness) -> Result<RepoProfile>; }
trait IssueIndexer { fn sync(&self, repo: &RepoName, mode: Freshness) -> Result<SyncReport>; }
trait Gatekeeper {
    fn start(&self, issue: &IssueRef, agent: &AgentId) -> Result<GateReport>;
    fn pr(&self, issue: &IssueRef, agent: &AgentId) -> Result<GateReport>;
}
```

`Freshness` 有四档：`Offline`（只读缓存）、`Default`（按 TTL 决定）、`Fresh`（忽略 TTL）、**`Live`**（必须收到网络响应才算数，304 也算；网络不可用时报错，不回退到缓存）。`Gatekeeper` 只能使用 `Live`，这一点由类型系统保证。

## 5. AI 政策标记（最重要；CLI 只存储和提醒，不做判断）

判断权在 Agent：Agent 读完原文后写入标记。之后任何 Agent、任何命令访问到这个仓库时，CLI 都把标记作为提醒附在输出里。CLI 只做两件事：**提供原文、保存并提醒标记**。

原文是否变化，CLI 不检测。真正的门禁是 Agent 提交 PR 前一定会阅读仓库的贡献规则（CONTRIBUTING、PR 模板、AI 政策）。`gate pr` 实时返回这些原文；如果规则变了，Agent 读的时候自己会发现，然后停下，或更新标记。

```rust
enum Stance { Bans, Restricts, Disclose, Allows, Welcomes, NoneFound } // 由 Agent 选定

struct PolicyMark {
    target: MarkTarget,              // Repo(owner/name) | Org(owner)，组织标记对其下所有仓库生效
    stance: Stance,
    scope: PolicyScope,              // 可选，Agent 填写：哪些环节受限
    requirements: Vec<Requirement>,  // 可选，Agent 填写：提 PR 时必须做到的事
    evidence: Vec<Evidence>,         // 至少一条；NoneFound 时记录"看过哪些路径"
    note: String,                    // Agent 的一句话总结，提醒时原样展示
    marked_by: AgentId, marked_at, expires_at,
}
struct PolicyScope {
    code: Restriction, pr_text: Restriction, comments: Restriction,
    autonomous_agents: Restriction,
    forbidden_labels: Vec<String>,   // 例如 ["good first issue"]
    needs_prior_approval: bool,
}
enum Requirement { DiscloseInPr, AssistedByTrailer, HumanWrittenDescription,
                   HumanWrittenReplies, MaintainerApprovalOnIssue, LabelPr(String) }
struct Evidence { path: String, url: String, excerpt: String /* Agent 摘录的原文，≤ 3 行 */ }
```

**流程**
1. 任何命令涉及某个仓库时，先查它的仓库标记，再查组织标记。
2. **有标记**：在 `meta.reminders` 中附上 `{stance, note, marked_at, marked_by, evidence_urls, review_due}`。人类可读输出里加醒目的一行，例如 `⚠ owner/repo: Bans AI — "<note>" (marked 2026-11-20)`。`discover` 和 `candidates` 默认把标记为 `Bans` 的仓库排除；结果中加一条"已排除 N 个"的说明；`--include-marked` 可以关闭排除。
3. **无标记**：输出 `reminders: [{kind: "unmarked", hint: "运行 contrib-scout policy files <repo> 阅读原文后，用 contrib-scout policy set 标记"}]`。`gate start` 遇到未标记的仓库会给出 `blocked: policy_unmarked`，让 Agent 在投入工作之前先读、先标。这是"禁止 AI 的仓库不要白干"的主要防线。
4. `contrib-scout policy files <repo>`：用条件请求取组织 `{org}/.github` 和仓库内的候选路径（见 §2），返回每个文件的路径、永久链接和原文（可截断，附截断标记）。不做任何解读。
5. `contrib-scout policy set`：Agent 写入或覆盖标记。至少要有一条证据；CLI 只校验格式。每次写入都追加一条历史记录。

**TTL（可配置，默认值偏长）**：Bans 90 天；Restricts / Disclose 30 天；Allows / Welcomes / NoneFound 14 天。过期后标记不会被删除，也不会被改写。提醒照常显示，只是多一个 `review_due: true`，提示 Agent 下次读原文时顺便重新标记。

## 5b. issue 标记（Agent 写入，避免重复阅读）

与仓库政策标记同一个思路，粒度是单个 issue。CLI 不判断，只存储和提醒。

```rust
enum IssueVerdict { Skip, Shortlist }          // Skip：不考虑；Shortlist：值得做，留给之后的 Agent
enum SkipReason { TooLarge, Unclear, NeedsSpecialEnv, OutOfScope, Duplicate,
                  MaintainerUnresponsive, AlreadyAddressed, Other }  // 开放枚举
struct IssueMark {
    issue: IssueRef, verdict: IssueVerdict, reason: Option<SkipReason>,
    note: String,                 // 一句话说明，提醒时原样展示
    seen_updated_at, seen_comments: u32,   // 标记时 issue 的状态快照，取自本地索引
    marked_by: AgentId, marked_at, expires_at: Option<Timestamp>,  // 默认不过期
}
```

- `issue mark <ref> --verdict skip|shortlist [--reason R] --note T [--until D]`：写入或覆盖；`issue unmark <ref>`。每次写入追加历史。
- `candidates` 和 `discover` 默认排除 `Skip` 的 issue，并在结果里注明"已排除 N 个"；`--include-marked` 可以显示它们。`Shortlist` 的 issue 排序靠前，并附上标记说明。
- **变化提示（不额外请求）**：索引同步后，如果 issue 的 `updated_at` 或评论数与标记时的快照不同，在提醒里加 `changed_since_mark: true`。被标为 `Skip` 的 issue 仍然默认排除，只在 `--include-marked` 和 `issue show` 里显示这个提示。是否重读由 Agent 决定。
- 已关闭、已指派、已有 PR 这些情况由索引自动识别，不需要手动标记。
- `gate start` 遇到被标为 `Skip` 的 issue，给出 `blocked: issue_skipped`，并附上标记原因。Agent 要先 `issue unmark` 才能继续。
- 标记只在本机生效，不写到 GitHub。

## 6. 数据模型（SQLite，按逻辑划分的表）

- `repos`：名称、归档状态、是否 fork、许可证、默认分支、`pushed_at`、star 数、topics、`raw_json`、`etag`、`fetched_at`
- `policy_marks`：即 `PolicyMark`，主键为 target（仓库或组织）；scope、requirements、evidence 存为 JSON 列；`stance` 和 `expires_at` 建索引
- `policy_mark_history`：每次 set、unset 都追加一行，用于审计"谁在何时依据什么改了判断"
- `issue_marks`：即 `IssueMark`，主键为 issue_ref；`verdict` 建索引
- `issue_mark_history`：每次 mark、unmark 追加一行
- `repo_activity`：外部 PR 合并率、首次回复中位时间、维护者最近活跃时间、样本量、`computed_at`
- `issues`：仓库、编号、状态、标签、assignee、评论数、`updated_at`、`raw_json`、`fetched_at`（只存索引字段，正文按需拉取）
- `sync_state`：每个仓库的 `since` 水位线、上次全量同步时间
- `http_cache`：URL 键、etag、last_modified、状态码、正文、`fetched_at`（只缓存实际读过的内容）
- `quota`：资源（core、search、graphql）、上限、剩余、重置时间、观测时间
- `request_log`：时间、资源、agent_id，用于计算全局时间窗口预算，定期裁剪
- `claims`：issue_ref、agent_id、claimed_at、lease_until、状态（active、released、expired、submitted）
- `gate_log`：关卡类型、issue_ref、agent_id、结论、原因列表、证据快照、`as_of`
- `discover_runs`：查询、参数、结果中的仓库列表、`fetched_at`，TTL 24 小时

数据库放在平台数据目录，可以用 `--db` 或 `CONTRIB_SCOUT_DB` 覆盖。所有 Agent 默认共用同一个文件。

## 7. 新鲜度与成本分层

| 数据 | 默认 TTL | 刷新方式 |
|---|---|---|
| AI 政策标记 | 14 到 90 天（见 §5） | 只在本地存储，不发请求；过期只标 `review_due`，不会改写标记 |
| 政策原文（`policy files`） | 7 天（`Default`） | 条件请求 |
| 仓库元数据 | 7 天 | 条件请求 |
| 仓库活跃度指标 | 7 天 | 用到搜索和 GraphQL，成本最高，只为通过了政策检查的仓库计算 |
| issue 索引 | 1 小时（`Default`），可配置 | `since` 加 `sort=updated`，带重叠窗口 |
| issue 详情 / timeline | 30 分钟（发现阶段） | 条件请求 |
| 搜索结果 | 24 小时 | 只用于发现 |
| **关卡读取** | **不使用 TTL（`Live`）** | 条件请求，304 视为实时确认 |

**发现流水线（从便宜到贵，一旦淘汰就停止）**：
搜索 → **查本地政策标记（零请求；标记为 Bans 的仓库直接排除，并在结果里注明）** → 仓库元数据（1 次请求，排除归档仓库和 fork）→ 活跃度指标 → issue 索引 → 本地打分。未标记的仓库照常列出，同时附 `unmarked` 提醒。

发现阶段不拉政策原文。只有 Agent 真正对某个仓库感兴趣时，才执行 `policy files`，这样成本只花在候选仓库上。

## 8. 关卡（总是实时，失败即拒绝，输出原始数据）

`gate start <ref>` 和 `gate pr <ref>` 实时检查以下各项：

| 检查项 | start | pr |
|---|---|---|
| issue 仍是 open | ✓ | ✓ |
| issue 未被标为 Skip（否则 `blocked: issue_skipped`） | ✓ | ✓ |
| 未被指派，或指派给我 | ✓ | ✓ |
| timeline 中没有别人的关联 PR（cross-referenced / connected 事件），以及 GraphQL `closedByPullRequestsReferences`（Development 侧栏关联的 PR 可能不出现在 timeline 里） | ✓ | ✓（排除我自己的分支） |
| 最近评论里没有人认领（启发式判断，只报告不裁决） | ✓ | ✓ |
| 仓库未归档 | ✓ | ✓ |
| 仓库或组织已有政策标记（没有标记则 `blocked: policy_unmarked`） | ✓ | ✓ |
| 标记立场是 Bans 则 `blocked: policy_bans` | ✓ | ✓ |
| 实时返回政策原文（CONTRIBUTING、PR 模板、AI 政策等候选文件），交给 Agent 阅读；规则若有变化，由 Agent 自己发现并停下或更新标记 | 只返回清单 | ✓ 返回全文 |
| issue 标签是否落在标记里的 `forbidden_labels` 中 | ✓ | ✓ |
| 是否需要维护者事先批准，以及 issue 里能否找到批准证据 | ✓（只报告） | ✓ |
| 本地认领：获取或续租 | 获取 | 必须持有 |
| 输出 PR 要求清单（声明 AI、`Assisted-by:`、PR 描述必须由人写、禁止自主 Agent） | 预告 | ✓ 完整输出 |

结论分三档：`pass`；`blocked`（附原因，例如 `policy_unmarked`、`policy_bans`、`human_required`、`claimed_by_other`、`pr_exists`）；`error`（网络或配额问题，按失败即拒绝处理）。

`GateReport` 包含**原始数据**：issue 正文、最近 N 条评论原文、timeline 中的 PR 链接、政策标记的提醒（note、证据摘录、永久链接），以及政策候选文件的当前原文。Agent 依据原文做判断，不依赖 CLI 的摘要。

当标记里的 scope 包含 `pr_text` 或 `comments` 为 `HumanWritten` / `Forbidden`、`autonomous_agents: Forbidden`、`code: Forbidden`，或 requirements 含 `HumanWrittenDescription` / `HumanWrittenReplies` 时，`gate pr` 的结论是 `blocked: human_required`。`code: Forbidden`（立场不是 Bans 时）在 `gate start` 也给出 `blocked: human_required`，因为 AI Agent 根本不能写代码。skill 规定 Agent 遇到这个结论必须停下，把工作交给人。

## 9. 多 Agent 协调

- **只拉一次**：按缓存键（URL 或仓库加数据类型的哈希）加文件锁，沿用 dwave 的 `lock_scope` 做法。第二个进程等一小段时间，然后读第一个进程写入的结果。
- **共享配额**：每个响应都把 `x-ratelimit-*` 头写入 `quota` 表。发请求前先 `reserve`：剩余低于保留阈值（默认 core 500、search 5）就拒绝，或者等到重置时间。`request_log` 再加一层全局上限，比如每小时 N 次、每分钟 M 次，所有 Agent 合计，用来防次级限流。
- **认领租约**：`agent_id` 来自 `--agent` 参数、`CONTRIB_SCOUT_AGENT` 环境变量，或会话 ID。默认租约 48 小时，再次执行 `gate start` 或 `claim renew` 续租，过期自动释放。认领只在本机生效，不写到 GitHub。
- **计划任务**作为唯一的后台写入者：`contrib-scout sync --all-tracked` 定时运行。Agent 平时用 `Default` 或 `Offline`，只有关卡阶段用 `Live`。

## 10. CLI 界面

```
contrib-scout discover [--lang L] [--topic T] [--label L]... [--limit N] [--explain]
contrib-scout repo show <owner/repo> [--fresh]                  # 元数据 + 活跃度 + 政策
contrib-scout policy files <owner/repo>                         # 只取原文：组织 .github 加仓库候选路径
contrib-scout policy show <owner/repo|org>                    # 当前标记与历史
contrib-scout policy set <owner/repo|org> --stance S --evidence PATH[:LINES]... --note T
                 [--forbid-label L]... [--require R]... [--scope K=V]...
contrib-scout policy unset <owner/repo|org>
contrib-scout policy due                                        # 列出已到 review_due 的标记
contrib-scout candidates [--min-score N] [--lang L] [--limit N] # 只读本地数据，零请求
contrib-scout issue show <ref> [--fresh]                       # 附带 issue 标记与仓库标记的提醒
contrib-scout issue mark <ref> --verdict skip|shortlist [--reason R] --note T [--until D]
contrib-scout issue unmark <ref>
contrib-scout issue marks [--verdict V] [--repo R]              # 列出 issue 标记
contrib-scout gate start <ref> [--agent ID]
contrib-scout gate pr <ref> [--agent ID] [--branch B]
contrib-scout claim list | renew <ref> | release <ref>
contrib-scout track add|rm|list <owner/repo>                    # 计划任务刷新的范围
contrib-scout sync [--all-tracked] [--max-requests N]
contrib-scout quota                                             # 共享配额与时间窗口用量
contrib-scout cache stats | prune [--older-than D]
contrib-scout config get|set <key> [value]
```

全局选项：`--json`、`--ndjson`、`--db`、`--config`、`--offline`、`--fresh`、`--max-requests`、`-v`、`--agent`。

JSON 信封沿用 dwave 的格式：`{ "schema": "contrib-scout/v1", "meta": {as_of, requests, served_304, partial, warnings, quota, reminders}, "data": … }`。所有涉及仓库的命令都会填充 `meta.reminders`。

退出码：`0` 成功或 pass；`10` 有新变化或新候选；`20` 关卡 blocked；`1` 错误（关卡阶段即失败拒绝）；`2` 用法错误。

## 11. 待你确认的开放问题（已给默认值）

1. **传输层默认用哪个**：默认 `GhTransport`（`gh api -i` 子进程，认证和代理设置完全交给 gh）。`HttpTransport`（reqwest 加 `gh auth token`）做备选，大批量同步时更快。
2. **导入外部政策清单作种子**：默认关闭。导入后只作为 `hint` 类提醒（"某公开清单称该仓库禁止 AI"），不算正式标记；Agent 仍需阅读原文后自己执行 `policy set`。
3. **gh 扩展别名**：默认不做。以后如有需要，再加一个薄壳仓库 `gh-contrib-scout`。
4. **`gate pr` 是否检查 CLA 和 DCO**：默认只报告，不裁决。

## 12. 交付顺序（每一步都能编译、能测试）

1. 骨架：workspace、模型、trait、`Freshness::Live` 类型约束、命令树和空实现处理函数。
2. Transport（gh 和夹具实现）、Gateway、Store 和迁移、共享配额、只拉一次的锁。
3. 政策原文获取、标记存储和历史、提醒注入 `meta.reminders`；`policy files|show|set|unset|due`。
4. 仓库画像、发现流水线；`discover`、`repo show`、`candidates`。
5. issue 增量索引和 issue 标记；`sync`、`track`、`issue show|mark|unmark|marks`。
6. 关卡和认领；`gate start|pr`、`claim`。
7. skill、README、CI、npm 原生二进制发布。

## 13. 验证策略

- 用夹具传输做全部单元测试，CI 中不发真实请求。另外保留一个 `--ignored` 的冒烟测试，会真实访问网络。
- **gh 传输**：304 时退出码为 1 但能正确识别；弱 ETag 原样回传；同时兼容 HTTP/1.1 和 HTTP/2 的状态行。
- **跨进程**：两个进程同时请求同一个键，断言只有一次真实请求；配额表跨进程可见；剩余低于阈值时拒绝。
- **关卡**：网络断开时，`Live` 返回 error 而不是缓存；304 算作通过的实时确认；未标记的仓库返回 `policy_unmarked`；`gate pr` 实时返回政策文件全文。
- **提醒**：仓库被标记后，`discover`、`candidates`、`repo show`、`issue show` 和两道关卡的输出里都带同一条提醒。组织标记对其下所有仓库生效；仓库标记优先于组织标记。标记为 Bans 的仓库默认被排除，并给出计数。
- **issue 标记**：Skip 的 issue 默认从 `candidates`、`discover` 中排除并计数；Shortlist 排序靠前；索引更新后出现 `changed_since_mark`；`gate start` 对 Skip 的 issue 返回 `issue_skipped`。
- **标记写入**：没有证据时拒绝写入；每次写入都追加历史记录；过期不删除标记。
- **端到端**：用临时数据库跑一遍 `discover → repo show → gate start → gate pr`，检查 `meta.requests` 和 `served_304` 是否符合预期。第二次运行 `candidates` 时请求数应为 0。
