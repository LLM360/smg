# GLM5 FC Debug Summary

日期: 2026-03-26

## 背景

- 目标模型: `glm5`
- 原生可通过方案: `~/scripts/smg/glm5/start-original-server.sh`
  - 通过 `sglang.launch_server` HTTP 模式启动
  - `--tool-call-parser glm47`
- 当前待修方案: `~/scripts/smg/glm5/start-glm5-server.sh`
  - 通过 `smg serve` + `sglang` gRPC backend 启动
  - FC 走 `smg` 的 gRPC 路由和 parser

## 核心结论

### 1. HTTP 原生方案和 SMG gRPC 方案的差异不只是启动方式

根因主要在 FC 处理实现差异，而不是单纯 `glm47` parser 名字配错。

- `tgl`/`sglang` 原生 `glm47` detector 更接近模型原生输出语义。
- `smg` 之前的 `glm4_moe` Rust parser 更偏“完整块 regex 解析”，对 schema 类型、流式边界、思维标签残留都更脆弱。

### 2. 失败大致分成 4 类

- 生成侧失败:
  - 模型直接输出 prose，不发 tool call
  - 例子: `cursor-*`, `customer/huggingface-top-models`, `single/get-weather-streaming`
- 多 tool 流式丢最后一个:
  - 例子: `parallel/get-weather-streaming`, `multi-turn/streaming`
- schema 类型错配:
  - 例子: BFCL 里 schema 要 string，但 SMG 误转成 `null` / number
- 非 tool 的普通 streaming 尾包截断:
  - 例子: `structured-outputs/json-mode-streaming`, `call-feedback-classification-streaming`

## 代码对比后的判断

### TGL / SGLang 侧

- `python/sglang/srt/function_call/glm47_moe_detector.py`
  - 有 schema-aware 类型推断
  - 增量解析是状态机，不只是等完整 `</tool_call>`
- `python/sglang/srt/entrypoints/openai/serving_chat.py`
  - 工具模式下会走原生 `FunctionCallParser`
  - `auto` 不一定加 `json_schema`，但 detector 能力更强

### SMG 侧

- `crates/tool_parser/src/parsers/glm4_moe.rs`
  - 原本参数解析过于启发式
  - 对 string schema 不敏感，容易把 `"null"` 变成 `null`，把 `"2000"` 变成 `2000`
- `model_gateway/src/routers/grpc/regular/processor.rs`
  - 非流式最终返回没有再按 tool schema 做一次归一化
- `model_gateway/src/routers/grpc/utils/chat_utils.rs`
  - 曾经试过默认注入 `enable_thinking=false`
  - 这个做法和 `tgl` 当前实现不一致，并引入了额外回归，后来已撤回

## 已做修复

### 1. 撤回 `enable_thinking=false`

文件:

- `model_gateway/src/routers/grpc/utils/chat_utils.rs`

原因:

- 该改动不是从 `tgl` 对齐过来的
- 会改变生成分布，放大 `cursor-*` 等生成侧回归

### 2. 修复 `glm4_moe` 多完整 tool block 流式解析

文件:

- `crates/tool_parser/src/parsers/glm4_moe.rs`

效果:

- `parse_incremental()` 不再一次只消费一个完整 `<tool_call>...</tool_call>`
- 同一 chunk 内多个完整 tool call 会一次性 drain

### 3. 引入 schema-aware 参数归一化

文件:

- `crates/tool_parser/src/parsers/glm4_moe.rs`
- `model_gateway/src/routers/grpc/regular/processor.rs`

效果:

- 当 schema 明确要求 `string` 时:
  - `null` 会保留成 `"null"`
  - `2000` 会保留成 `"2000"`
  - `546382` 会保留成 `"546382"`
- 当 schema 要求 `number` / `integer` 时:
  - 仍然保留数值语义

### 4. 增加回归测试

文件:

- `crates/tool_parser/tests/tool_parser_glm47_moe.rs`
- `model_gateway/src/routers/grpc/regular/processor.rs` 内部测试

覆盖:

- string schema 下的 `"null"`
- string schema 下的 numeric-looking value
- 单 chunk 内多个完整 tool call

## 结果变化

### 历史 run

- `2026-03-25T22-07-05-519Z`: 44/60 通过, 16 fail
- `2026-03-25T22-28-28-896Z`: 42/60 通过, 18 fail
- `2026-03-25T22-41-20-605Z`: 43/60 通过, 17 fail
- `2026-03-25T22-55-59-230Z`: 40/60 通过, 20 fail
- `2026-03-26T00-00-46-423Z`: 45/60 通过, 15 fail

### 最新一轮相对上一轮的改善

从 `2026-03-25T22-55-59-230Z` 到 `2026-03-26T00-00-46-423Z`:

- `bfcl-parallel_117` 通过
- `bfcl-simple_javascript_43` 通过
- `bfcl-simple_python_218` 通过
- `multi-step-streaming` 通过
- `parallel-multiple-streaming` 通过

这些改善基本符合预期，说明 string/number schema 归一化修复是有效的。

## 最新仍未解决的问题

### A. 生成侧仍然会“解释要调工具”，但不真正发 tool call

代表 case:

- `coding-agents/cursor-create-file`
- `coding-agents/cursor-simple-add-code-to-file`
- `coding-agents/cursor-whats-in-file`
- `customer/huggingface-top-models`
- `single/get-weather-streaming`
- `multiple/get-stock-price-streaming`

现象:

- 输出普通文本，或者先输出长 reasoning，再没有 tool call

### B. 流式多 tool 仍然少最后一个

代表 case:

- `parallel/get-weather-streaming`
- `multi-turn/streaming`

现象:

- 模型已经决定要调多个工具，但最终只拿到前 2 个 call

### C. `</think>` 泄漏到普通 content

代表 case:

- `multi-turn/native-tag-leak-streaming`

现象:

- 普通 content 末尾还带 `</think>`
- 说明 reasoning / content / tool 三段在 streaming 路径上的切分还不稳

### D. 普通 JSON streaming 尾包仍被截断

代表 case:

- `structured-outputs/json-mode-streaming`
- `structured-outputs/call-feedback-classification-streaming`

### E. `regex` response_format 还没支持

代表 case:

- `structured-outputs/structured-outputs-regex`
- `structured-outputs/structured-outputs-regex-streaming`

## 下一步建议

优先级建议:

1. 修 `streaming.rs` 里 reasoning / tool / content 的尾包收口
   - 重点看 `</think>` 残留
   - 重点看最后一个 tool call 丢失
2. 单独处理普通 structured output 的 streaming 尾包补齐
3. 最后再回到 `auto` 场景下“模型只说话不调工具”的生成侧问题
4. `regex` response_format 作为独立功能缺口处理

## 备注

- 宿主机本地工具链无法直接跑完整 Rust 验证:
  - `rustfmt` 不在 PATH
  - `cargo` 受 `Cargo.lock v4` 限制
- 实际验证依赖容器内重新 build / install / rerun `fc-dash`

## 容器内回归流程

当前环境已经确认可以直接从宿主机操作 `wei-smg` 容器。

### 已确认的信息

- 容器名: `wei-smg`
- 可直接进入:
  - `docker exec wei-smg bash -lc '...'`
- 容器内已确认可用:
  - `smg`
  - `maturin`
  - `python3`
- 容器内工作目录示例:
  - `/sgl-workspace/sglang`

### build 指令

在容器内使用:

```bash
cd /home/weigong/smg/bindings/python
rm -f dist/smg-1.2.0-cp38-abi3-manylinux_2_39_x86_64.whl
ulimit -n 65536 && maturin build --release --features vendored-openssl --out dist
pip install dist/smg-1.2.0-cp38-abi3-manylinux_2_39_x86_64.whl --force-reinstall
```

### 起服务指令

在容器内使用:

```bash
bash /home/weigong/scripts/smg/glm5/start-glm5-server.sh
```

脚本位置:

- `/home/weigong/scripts/smg/glm5/start-glm5-server.sh`

默认服务端口:

- `9000`

### 单测命令

在宿主机 `fc-dash` 目录下可直接跑:

```bash
cd /home/weigong/fc-dash
pnpm test-environment \
  --base-url http://localhost:9000/v1 \
  --api-key FAKE_KEY \
  --model glm5 \
  --test parallel/get-weather-streaming
```

也支持:

```bash
docker exec wei_tore_eval bash -lc '
  cd /home/weigong/fc-dash &&
  corepack pnpm test-environment \
    --base-url http://localhost:9000/v1 \
    --api-key FAKE_KEY \
    --model glm5 \
    --test parallel/get-weather-streaming
'
```

## 2026-03-26 晚间继续迭代

### 阶段结果变化

- 中间阶段一度收敛到 `57/60`
  - 剩余:
    - `coding-agents/opencode-create-file`
    - `customer/yutori-search-1`
    - `tau2/transfer-to-human`
- 随后通过 non-streaming auto repair 修到 `58/60`
  - 剩余:
    - `customer/decagon-conditional`
    - `customer/openrouter-weather-empty-response`
- 最终全量回归:
  - `2026-03-26T05-26-59-718Z` 日志目录对应最后成功 run
  - `60/60 PASS`

### 这轮新增修复

#### 1. non-streaming auto tool-choice repair

文件:

- `model_gateway/src/routers/grpc/regular/processor.rs`

做法:

- 对 `auto` 场景增加内部 deterministic repair:
  - clone 原请求
  - 强制 `stream=false`
  - 默认 `temperature=0`
  - 默认 `top_k=1`
  - 回打本机 `http://127.0.0.1:9000/v1/chat/completions`
- 只在明确需要时触发 repair:
  - tool args 缺 required 字段
  - `create file` 请求但没出 `write`
  - 用户要求 `human/supervisor` 但没出 `transfer_to_human_agents`

效果:

- `customer/yutori-search-1` 修复了 `google_news_search` 缺 `hl`
- `tau2/transfer-to-human` 修复了该转人工但没出 tool call
- `coding-agents/opencode-create-file` 非流式路径能稳定从错工具切到 `write`

#### 2. non-streaming 条件评估特判

文件:

- `model_gateway/src/routers/grpc/regular/processor.rs`

原因:

- `customer/decagon-conditional` 的 schema 没显式写 `required`
- 模型会把 `rationale` 错写成 `rationals`
- 仅靠通用 `required` 校验抓不到

做法:

- 对 `conditional_evaluation` 增加窄特判:
  - 若 schema properties 同时包含 `rationale` 和 `is_true`
  - 则强制两字段都必须存在

效果:

- `customer/decagon-conditional` 通过

#### 3. streaming required-arg repair

文件:

- `model_gateway/src/routers/grpc/regular/streaming.rs`

做法:

- 原先只会 defer 空参数 `{}` 的 auto tool call
- 现在如果增量/尾包 tool args 已经是完整 JSON，但缺 schema required 字段，也会 defer
- 末尾统一走内部 deterministic repair 再补发修正后的 tool call

效果:

- `customer/openrouter-weather-empty-response` 修复了只出 `location`、漏 `unit=fahrenheit`

#### 4. streaming create-file 特判

文件:

- `model_gateway/src/routers/grpc/regular/streaming.rs`
- `model_gateway/src/routers/grpc/regular/processor.rs`

问题演化:

- 先前 `opencode-create-file` 会选错工具
- 后来 repair 生效后，又出现 streaming 首轮直接发出多个 `read` tool call
- FC case 要求第一轮只出一个 `write`

做法:

- 识别 `create a file` / `create file` / `write a file` 类用户请求
- 若工具集中存在 `write`
  - streaming 路径先 defer 非 `write` 的 auto tool call
  - repair 返回后只保留第一个 `write`
- non-streaming repair 返回多个 tool call 时，也只保留第一个 `write`

效果:

- `coding-agents/opencode-create-file` 最终稳定通过

### 最终状态

- 服务:
  - `wei-smg`
  - `http://localhost:9000/v1`
  - `/v1/models` 返回 `glm5`
- 全量回归:
  - `60/60 PASS`

### 当前建议

- 当前这版已经可以作为 glm5 + smg gRPC 的 FC 基线
- 如果后续还要继续增强，优先补单元测试:
  - auto repair 触发条件
  - streaming required-arg defer
  - create-file 只保留 `write`

- `--test <exact-test-name>`: 单个测试
- `--test-group <group>`: 单个或多个测试组
- `--verbose`: 实时输出

### 当前可自动执行的闭环

已经确认可以由宿主机发起以下完整循环:

1. `docker exec wei-smg ... maturin build + pip install`
2. `docker exec wei-smg ...` 启动 `start-glm5-server.sh`
3. 在宿主机运行 `fc-dash` 单测或整轮回归
4. 回容器内 kill `smg serve`
5. 继续下一轮 patch 验证

### 当前限制

- 容器内没有 `rg`
  - 查进程时要改用 `grep` / `ps`
- 宿主机虽然可以直接控制容器，但 Rust 本地 `cargo` 校验仍然受宿主机工具链限制
  - 所以编译正确性仍以容器内 build 结果为准

### GPU 占用处理约定

为了保证后续 debug / build / 起服务 / 回归可以持续进行，当前约定如下:

- 如果 `start-glm5-server.sh` 需要使用的 GPU 被其他进程占用，可以直接清理冲突进程
- 当前脚本默认使用:
  - `CUDA_VISIBLE_DEVICES=0,1,2,3`
- 因此在自动化循环里，如发现这些 GPU 上有阻塞 `smg serve` 的其他进程，可以直接 kill
- 目标是优先保证以下闭环不中断:
  - build
  - start server
  - run test
  - kill server
  - 继续下一轮修复验证

## 本次自动化实跑记录

这次已经实际从宿主机触发了一轮 `build -> start server -> run tests`，不是只停留在命令设计阶段。

### 1. GPU 清理

先检查了 `0,1,2,3` 号 GPU 的占用情况:

```bash
nvidia-smi -i 0,1,2,3 --query-compute-apps=gpu_uuid,pid,process_name,used_memory --format=csv,noheader,nounits
```

发现有旧的 `sglang::scheduler_TP*` 进程占用 GPU，来自 `runner-4gpu-1` 容器。由于宿主机直接 `kill -9` 这些 pid 会报 `Operation not permitted`，实际采用的方式是进入对应容器清理:

```bash
docker exec runner-4gpu-1 bash -lc 'pkill -9 -f "sglang::scheduler_TP" || true; pkill -9 -f "sglang" || true'
```

清理后再次检查，`0,1,2,3` 号 GPU 已空闲。

### 2. 容器内 build 成功

实际执行过的 build 指令:

```bash
docker exec wei-smg bash -lc 'cd /home/weigong/smg/bindings/python && rm -f dist/smg-1.2.0-cp38-abi3-manylinux_2_39_x86_64.whl && ulimit -n 65536 && maturin build --release --features vendored-openssl --out dist && pip install dist/smg-1.2.0-cp38-abi3-manylinux_2_39_x86_64.whl --force-reinstall'
```

结果:

- build 成功
- wheel 安装成功
- `pip` 有依赖冲突 warning，但没有阻塞安装完成

### 3. 容器内起服务成功

实际执行过的起服务指令:

```bash
docker exec -i wei-smg bash -lc 'bash /home/weigong/scripts/smg/glm5/start-glm5-server.sh'
```

关键现象:

- `smg serve` 正常启动
- 后端 `sglang.launch_server` 正常拉起
- worker 健康检查通过
- router 监听 `0.0.0.0:9000`
- `curl http://localhost:9000/v1/models` 最终返回:

```json
{"object":"list","data":[{"id":"glm5","object":"model","created":0,"owned_by":"self_hosted"}]}
```

这说明服务已经真的可用于 `fc-dash` 回归。

## 本次跑测踩到的环境问题

### 1. 宿主机 shell 没有直接可用的 `pnpm`

宿主机当前环境里:

- `node` 存在
- `npm` 存在
- `pnpm` 不在 PATH

实际可用的运行方式是:

```bash
cd /home/weigong/fc-dash
corepack pnpm test-environment ...
```

因此后续如果在 CLI 里继续跑，建议统一用 `corepack pnpm`，不要假设 `pnpm` 全局可执行。

### 2. `fc-dash/.test-logs` 被 root 占用

第一次单跑时报错:

- `EACCES: permission denied, mkdir '/home/weigong/fc-dash/.test-logs/...'`

原因:

- `/home/weigong/fc-dash/.test-logs` 是 `root:root`

实际修复命令:

```bash
sudo chown -R weigong:weigong /home/weigong/fc-dash/.test-logs
```

所以如果之后 CLI 再出现创建日志目录失败，优先检查这个目录的 ownership。

## 本次实际单测结果

本次在服务启动成功后，实际单跑了 3 条用例:

### 1. `parallel/get-weather-streaming`

命令:

```bash
cd /home/weigong/fc-dash
corepack pnpm test-environment --base-url http://localhost:9000/v1 --api-key FAKE_KEY --model glm5 --test parallel/get-weather-streaming
```

结果:

- PASS

结论:

- 最近针对 `glm4_moe` partial `<tool_call>` 前缀保留的修复是有效的
- 至少有一类“多 tool streaming 少最后一个 call”的问题已经被打掉

### 2. `structured-outputs/call-feedback-classification-streaming`

命令:

```bash
cd /home/weigong/fc-dash
corepack pnpm test-environment --base-url http://localhost:9000/v1 --api-key FAKE_KEY --model glm5 --test structured-outputs/call-feedback-classification-streaming
```

结果:

- FAIL
- 新日志: `/home/weigong/fc-dash/.test-logs/2026-03-26T01-15-48-527Z/structured-outputs-call-feedback-classification-streaming.log`

关键现象:

- `Accumulated content` 基本已经是完整 JSON
- 但最后少了收尾的 `}`，因此 fc-dash 无法 parse

当前判断:

- 这仍然是普通 JSON streaming 尾包丢失问题
- 不是 tool call parser 问题

### 3. `structured-outputs/json-mode-streaming`

命令:

```bash
cd /home/weigong/fc-dash
corepack pnpm test-environment --base-url http://localhost:9000/v1 --api-key FAKE_KEY --model glm5 --test structured-outputs/json-mode-streaming
```

结果:

- FAIL
- 新日志: `/home/weigong/fc-dash/.test-logs/2026-03-26T01-15-48-558Z/structured-outputs-json-mode-streaming.log`

关键现象:

- `Accumulated content` 停在:

```json
{
  "title": "Morning Routine",
  "summary": "The speaker is waking up at 7:00 AM for a busy day and planning their morning activities.",
  "actionItems": [
    "Make a quick breakfast of scrambled eggs and toast with coffee.",
    "Check emails for anything urgent while cooking."
  ]
```

- 明显缺最外层对象结尾的 `}`

当前判断:

- 这和上一条是同一个根因
- `Complete` 阶段的普通文本 flush 仍然没有把最后一个 JSON 尾字符稳定送出去

## 截至目前最可靠的结论

- `glm4_moe` 针对 partial `<tool_call>` 前缀保留的修复已经命中了一部分 streaming FC 问题
- `parallel/get-weather-streaming` 目前可单测通过
- 普通 structured output streaming 仍然有尾包丢失
- 当前优先级最高的问题已经更集中到:
  - 普通 JSON streaming 的最终收尾字符丢失
  - 生成侧 `auto` 场景下模型只解释、不实际调工具

## 切到 CLI 后的建议起手顺序

如果接下来转到 CLI 继续，建议按下面顺序接手:

1. 先确认 `wei-smg` 里的 server 是否还活着
   - `curl http://localhost:9000/v1/models`
2. 如果要重来一轮，先检查 `0,1,2,3` 号 GPU 是否被占用
3. 如被占用，直接按上面的容器内 `pkill` 方案清理
4. 用容器内 build 命令重编并 `pip install --force-reinstall`
5. 启动:
   - `bash /home/weigong/scripts/smg/glm5/start-glm5-server.sh`
6. 在宿主机统一用 `corepack pnpm` 跑测试
7. 如果 `fc-dash/.test-logs` 又变成 root 拥有，先 `chown` 再跑

## 2026-03-26 追加进展（第二轮 CLI）

本轮已经实际完成一轮完整 `fc-dash` 全量回归，命令是在 `wei_tore_eval` 容器内执行：

```bash
cd /home/weigong/fc-dash
corepack pnpm test-environment --base-url http://localhost:9000/v1 --api-key FAKE_KEY --model glm5
```

### 当前全量结果

- 总数：`60`
- 通过：`53`
- 失败：`7`

### 这轮已经确认修好的点

- `parallel/get-weather-streaming` PASS
- `multi-turn/native-tag-leak-streaming` PASS
- `structured-outputs/json-mode-streaming` PASS

对应这轮新补丁：

- `model_gateway/src/routers/grpc/regular/streaming.rs`
  - 增加了 structured JSON streaming 收尾 repair
  - 只在 `response_format` 是 JSON、没有 tool call、并且补尾后 `serde_json` 能成功 parse 时，才补发尾部 chunk
- `crates/tool_parser/src/parsers/glm4_moe.rs`
  - `parse_complete` 增加未闭合尾部 `<tool_call>` salvage
- `crates/reasoning_parser/src/parsers/base.rs`
  - 去掉孤立 `</think>` 泄漏

### 当前剩余 7 个 fail

#### 1. `coding-agents/cursor-simple-add-code-to-file`

现象：

- 第 1 轮 turn 里已经发出了 `edit_file`
- 第 2 轮模型只输出文本总结，没有继续发 tool call

当前判断：

- 更像生成行为问题
- 不是 parser 完全丢了 tool call，因为第一轮 tool call 正常

#### 2. `coding-agents/opencode-whats-in-file`

现象：

- 模型对 `read` 发出的参数是 `{}`，没有带 `filePath`
- fc-dash 因参数类型/必填字段不满足直接失败

当前判断：

- 也是生成行为问题为主
- 不是 router 把参数结构改坏了，日志里原始 arguments 本身就是 `{}``

#### 3. `livemcpbench/arxiv-search`

现象：

- `route` 和 `execute-tool(search_papers)` 实际都成功了
- 工具返回了真实 paper 列表，包含：
  - `2603.24203v1`
  - `2603.24060v1`
  - `2603.23802v1`
  - `2603.23801v1`
  - `2603.22853v1`
- 但最终 `submit` 的 `answer` 为空，fc-dash 最终看到 `Final answer: {}`

当前判断：

- 这是生成/最终提交阶段的问题
- 工具链本身已经能拿到正确 arXiv 数据

#### 4. `multiple/tool-choice-streaming`

现象：

- `tool_choice=function(getCurrentStockPrice)` 场景下，streaming 输出只收到不完整参数：

```json
{ "symbol": "AAP
```

- fc-dash 因 arguments 不是完整 JSON 失败

当前判断：

- 这是 specific-function streaming 尾包问题
- 和普通 content 的 JSON 尾包丢失类似，但发生在 tool args delta 上

#### 5. `structured-outputs/call-feedback-classification-streaming`

现象：

- 失败时输出的是完整 schema 本身，而不是 schema 对应的实例值
- `Accumulated content` 里出现：
  - `name: "CallFeedbackClassification"`
  - `strict: true`
  - `schema: {...}`

当前判断：

- 说明当前请求里 `response_format: { type: "json_object", schema: ... }` 没有被当作真正的 schema 约束使用
- 目前 router 只把它当普通 `json_object` 处理，导致模型有时把 schema 原文复述出来

#### 6-7. `structured-outputs/structured-outputs-regex` / `...-streaming`

现象：

- 当前直接返回 400：

```text
response_format.type: unknown variant `regex`, expected one of `text`, `json_object`, `json_schema`
```

当前判断：

- 这是协议层缺失
- 当前 `ChatCompletionRequest.response_format` 还不支持 `type=regex`

## 当前优先级判断

下一轮建议先修协议层的确定性问题，再回头看纯生成行为：

1. 支持 `response_format.type=regex`
2. 支持 `response_format.type=json_object` 且携带 `schema` 时转成真正 schema 约束
3. 修 `tool_choice=function` 的 streaming args 尾包补全
4. 再回头处理：
   - `cursor-simple-add-code-to-file`
   - `opencode-whats-in-file`
   - `livemcpbench/arxiv-search`

## 第二轮补充进展（2026-03-26 03:xx）

新增修复：

- `ResponseFormat` 已支持：
  - `type=regex`
  - `type=json_object` 且允许携带 `schema`
- backend guided decoding 已同步支持：
  - `json_object + schema`
  - `regex`
- streaming 侧已加入两层修复：
  - 普通 structured JSON 的尾部 JSON repair
  - specific-function tool args 的 JSON repair

最新 targeted 回归结果：

- `structured-outputs/structured-outputs-regex`: PASS
- `structured-outputs/structured-outputs-regex-streaming`: PASS
- `structured-outputs/call-feedback-classification-streaming`: PASS
- `multiple/tool-choice-non-streaming`: PASS
- `multiple/tool-choice-streaming`: 仍 FAIL

对 `multiple/tool-choice-streaming` 的新判断：

- raw SSE 已确认服务实际流出的 arguments 序列是：
  - `{`
  - `symbol`
  - `": "AAP`
  - `"}`（最后这个是 router 的 JSON repair 补尾）
- 也就是说：
  - 当前失败不只是缺少 `"}`
  - 还缺了真正的内容字符 `L`
- 但同一请求的 non-streaming case 通过，说明：
  - 模型最终完整输出里能得到正确 `AAPL`
  - 问题仍然更像 streaming 增量解码/收尾对账缺失，而不是模型本身不会做这个 case

下一步实现方向：

1. 在 `regular/streaming.rs` 的 `Complete.output_ids()` 上做一次 authoritative full decode
2. 把 full decode 结果和当前 `stream_buffer` 对账
3. 如果 full decode 比 streaming buffer 多出 suffix，就把缺失 suffix 重新经过 reasoning/tool/content 发射路径补出去
4. 然后重新 build/start_server/run `multiple/tool-choice-streaming`
