# Unicode 标识符规范化碰撞工作台

单机、服务端规则、文件持久化的迁移评估工作台。导入带稳定记录号的字符串集合，
配置大小写折叠 / NFC / NFKC / 默认可忽略字符 / 脚本限制，逐阶段查看码点变化、
所用 Unicode 表版本与最终分桶；在冲突组上做迁移决策（重命名、保留旧别名、拒绝
迁移），模拟全有或全无的提交，确定性导出规则快照与每条映射。

原始字节永远保留；任何规范化或修订都另存为不可变的新版本。

## 运行

```sh
cargo fetch
cargo test --quiet
cargo run --bin server -- --listen 127.0.0.1:5213
# 打开 http://127.0.0.1:5213
```

可选参数：`--data-dir PATH`（默认 `./workbench-data`）。

## 架构与取舍

```
浏览器(static/, 纯 JS, 无构建步骤)
   │  HTTP/1.1 (stdlib TcpListener, 每连接一线程, 全局 mutex 串行化写)
   ▼
src/server.rs     薄 HTTP 外壳：路由、JSON、静态资源
src/service.rs    全部业务规则：分桶、方案校验、别名环、碰撞模拟、OCC、导出
src/store.rs      持久化：snapshot.json + wal.log (JSON 行, fsync)
src/unicode.rs    规范化管线（规则唯一真实来源）
src/model.rs      领域实体 + hex 字节序列化
```

- **规则只在服务端。** 网页只显示服务端返回的阶段/码点，不参与等价判定。
  搜索框里的查询自身也由服务端按当前查看规则处理，同时原样回显 `query_raw`。
- **原始字节不可变。** 每条记录存原始字节（JSON 中用 hex）；分析结果、
  方案、映射都是独立实体，靠 ID 与 revision 关联，绝不回写原文。
- **不可变规则集 + revision。** “升级规则表”不是原地改配置，而是创建一个带新
  `revision` 的新规则集（表版本来自当前二进制所链接的 Unicode 数据）。
  已批准方案冻结在批准时的规则快照上，升级不会重算；要比较版本差异，需要对同一
  数据集构建新分析，再用 `/api/compare?a=…&b=…` 对比。
- **冲突只是候选。** 相同规范值会分进同一个桶，但系统不自动删除任何一条；
  视觉相似（例如 Latin `a` 与 Cyrillic `а`）在脚本不同的前提下不会被视为等价，
  只会作为脚本风险单独报告。
- **单机持久化、无外部数据库。** 每个写请求是一条 fsync 的 WAL 记录，周期性把全
  状态原子快照到 `snapshot.json`（先写临时文件、rename、fsync 目录，再截断
  WAL）。WAL 每条带单调事件序号，启动时重放严格新于快照的记录，因此在任意时刻
  `SIGKILL` 都不会重复应用事件或复用 ID。
- **并发控制用资源版本号。** 方案写入带 `base_version`；落后的写入返回 `409`，
  附带 `changes_since_base`（对方变更）与当前版本，不做静默覆盖，也没有
  last-write-wins。
- **全有或全无。** 提交前必须通过模拟：存在未决策组、目标值/别名碰撞、别名环，
  或任何新增试算记录与提案/已提交基线碰撞，提交都会整体失败并返回碰撞明细与基线
  差异。实际提交在单个 mutex 临界区的一次 WAL 追加内完成。
- **确定性导出。** 导出 JSON 字段顺序固定（`BTreeMap` 排序、无时间戳），附
  SHA-256；相同已批准方案重复导出字节一致（有测试断言）。

## 处理管线（五阶段，均带 provenance）

1. `raw` — 宽容解码：合法 UTF-8 → 标量；WTF-8 风格的 3 字节代理序列 → 单独代理
   单元；每个畸形/截断字节 → `invalid_byte`，永不吞掉、永不参与后续阶段。
2. `scalars` — 仅保留合法标量；标记非字符（`U+nFFFE/nFFFF`、`U+FDD0..FDEF`）。
3. `case_fold` — Unicode default case fold（full，多码点输出，如 `ß`→`ss`）。
4. `strip_ignorable` — 可选移除 `Default_Ignorable_Code_Point`（ZWJ、ZWSP、
   变体选择符、软连字符等）。
5. `normalize` — NFC 或 NFKC。

脚本在最终值上计算：Common/Inherited 忽略，出现两个及以上非通用脚本即报告
`mixed_script`；配置脚本限制时，不属于该脚本则报告 `restriction_violated`。
问题分四类、互不混淆：非法 UTF-8、单独代理、非字符、脚本风险。

页面上“展开各阶段精确码点”可以看到每个单元的 `U+xxxx`、类型（标量/代理/非法
字节）、变化说明和指向上一阶段位置的 provenance。

## HTTP 摘要

| 方法 & 路径 | 说明 |
| --- | --- |
| `POST /api/rulesets` | 创建规则集（label/case_fold/normalization/strip/restrict_script） |
| `POST /api/tables/upgrade` | 用当前二进制表创建下一 revision |
| `POST /api/datasets` | 导入；支持 `Idempotency-Key` 头，重复键返回首次响应 |
| `POST /api/analyses` | 在指定数据集+规则集上构建不可变分析 |
| `GET /api/analyses/:id?stage=&issue=1&record=` | 阶段筛选 / 只看问题 / 单记录 |
| `POST /api/plans` | 为分析创建草稿方案（自动列出冲突组） |
| `PUT /api/plans/:id/decisions` | 合并决策，带 `base_version` OCC |
| `POST /api/plans/:id/simulate` | 模拟（可带 `extra_records` 试算，不写盘） |
| `POST /api/plans/:id/approve` | 校验通过才提交；`commit:false` 只模拟 |
| `GET /api/plans/:id/export` | 确定性导出 + SHA-256 |
| `GET /api/search/:id?q=` | 查询自身按规则处理，保留原文 |
| `GET /api/compare?a=&b=` | 两个分析（不同 revision）的逐记录差异 |
| `GET /api/state` | 规则集/数据集/分析/方案一览 |

## 测试

- `tests/pipeline.rs` — 折叠+NFC 碰撞、NFC/NFKC 差异、非法 UTF-8、单独代理、
  非字符、混合脚本与限制、DI 移除、provenance、视觉相似不合流。
- `tests/api_flow.rs` — 幂等重复请求、端到端碰撞→决策→模拟→批准、非法状态跳转
  （批准后再批准/再改决策均 400）、落后写入返回双方差异的 409。
- `tests/recovery.rs` — 制造 70+ 次写（跨越快照压缩）后强杀进程，重开同一目录
  校验无重复重放、ID 连续、幂等结果不变。
- `tests/validation.rs` — 坏脚本码、重复记录号、别名环阻断提交、视觉相似不合并、
  已批准方案不随新规则重算且可比较。

## 已知限制（明确不做的事）

- 单机单进程；没有鉴权、TLS 或横向扩展。全局写 mutex 对评估/迁移类负载足够，
  读路径同样持锁但很短。
- provenance 对折叠/规范化的多对多变化用 LCS 对齐近似（复杂簇形时属于启发式
  展示，不影响规范值本身的精确性）。
- `Default_Ignorable_Code_Point` 用“Cf 类别减例外集 + 硬编码属性区间”近似
  判定，可能与最严格的派生属性表有细微出入；表版本在每个规则集与导出中都有
  记录，升级表后可重建分析比较。
- 冲突组必须由人显式决策；系统永远不会自动挑选一条删除，也不会把视觉相似当作
  等价。`reject` 的记录不进入已提交基线。
- HTTP 请求体上限 8 MiB；更大导入请分批导入多个数据集。
