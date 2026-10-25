# 翻译组件：手动安装 + 启动自检

> 前提变更（2026-10-22）：**不做下载器**。组件由用户自己从 GitHub Releases 下载解压，主程序只在启动时检查"能不能用"。
> 这一刀砍掉了下载器、断点续传、SHA256 校验、进度 UI、远程 manifest——方案体量小了约 2/3。

---

## 0. 交付形态

| | 内容 |
| --- | --- |
| 基础交付 | 仍然只有 1 个 `smartpaste.exe`（+~60 KB 的检查与进程管理代码） |
| 翻译组件 | 用户自己下 `smartpaste-mt-<ver>.zip`，解压到 exe 同级，得到 `mt\` 目录（约 1.15 GB） |
| 不装翻译 | 和今天完全一样，按钮置灰，点击说明怎么装 |
| 卸载 | 删 `mt\` 目录，重启程序回到"未安装" |

```
D:\Tools\smartpaste\
  smartpaste.exe
  config.txt
  mt\                          ← 用户手动解压出来的
    VERSION.txt                （可选，纯展示用）
    runtime\
      llama-server.exe
      ggml-base.dll / ggml-cpu-*.dll / ggml-vulkan.dll
    models\
      hy-mt1.5-1.8b-q4_k_m.gguf
```

---

## 1. 只有两种状态

没有下载状态机了，只剩：

```
启动自检 ──> READY        按钮可用
        ├─> MISSING      目录不存在
        └─> INCOMPLETE   目录在，但缺文件 / 文件明显不完整
```

UI 上就是"能用"和"不能用"，不能用的再分两种文案。`RUNNING`（进程已拉起）是内部状态，不暴露。

---

## 2. 启动自检：查什么，不查什么

### 查（成本 < 1 ms）

一次目录枚举 + 几次 stat：

| 检查项 | 规则 | 目的 |
| --- | --- | --- |
| `mt\` 是否可读 | 能枚举 | 路径错 / 权限 |
| `runtime\llama-server.exe` | 存在且 > 1 MB | 引擎在 |
| `models\*.gguf` | 至少 1 个，且 > 300 MB | 模型在，且不是半截文件 |
| `runtime\*.dll` | 至少 1 个 | 缺 DLL 时提前提示，而不是等拉起才报错 |

模型用通配 + 体积下界，是为了**允许用户自己选量化档**：放 Q4 还是 Q8 都行，程序取第一个 `.gguf`；有多个时按 `mt_model=` 配置优先。

### 不查（重要）

- **不做 SHA256 全量校验**。1.1 GB 哈希一遍要几秒到几十秒，还狂读盘。手动安装真正的风险是"下了半截"或"解压错层级"，体积下界已经能抓住。
- **不拉起进程试跑**。启动时 spawn 一个 llama-server 会占内存、触发杀软、拖慢首屏。真正的校验放到第一次翻译，失败就把 stderr 原样展示。
- **不联网**。没有版本检查，没有"有新组件"提示。

### 一个便宜的容错

用户常犯的错是解压多套一层（`mt\mt\runtime\…`）。检测到 `mt\` 下没有 `runtime\` 但有 `mt\mt\runtime\`，直接用内层，并在设置页注明"检测到嵌套目录，已自动使用内层"。比报错友好得多。

---

## 3. 不可用时的 UI

工具栏按钮置灰但可点，点开是一张说明卡：

```
翻译组件未安装
把组件解压到下面这个目录，让它和 smartpaste.exe 并排：

    D:\Tools\smartpaste\
    └─ mt\  ← 应该出现这个

[打开所在文件夹]   [查看安装说明]   [不再提示]

下载地址：github.com/<user>/smartpaste/releases
```

要点：
- **直接显示绝对路径**，不让用户自己找（这是替代下载器的核心 UX）
- `[打开所在文件夹]` 用 `ShellExecuteW("open", exe_dir)` 把资源管理器开在 exe 所在目录
- 缺件时文案要具体：`缺少 models\*.gguf 或文件不完整`，不要只说"组件不可用"
- `[查看安装说明]` 打开 GitHub README 对应锚点

`INCOMPLETE` 用同一张卡，标题换成"翻译组件不完整"，列出缺的东西。

---

## 4. 运行时（这部分不变）

第一次点翻译时才拉起进程：

```
llama-server.exe -m models\<第一个 gguf> --host 127.0.0.1 --port <空闲端口>
                 -c 2048 -t <物理核/2> --repeat-penalty 1.1
                 [检测到独显] -ngl 99
```

必须保住的三件事：

1. **Job Object 兜底**：`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`。否则主程序被强杀后 `llama-server.exe` 变孤儿，常驻 1 GB。这个项目已经在单实例锁上吃过残留进程的亏。
2. **健康检查**：轮询 `GET /health`，模型加载要 2–5 s，期间 UI 显示"正在加载模型"。
3. **空闲回收**：5 分钟没人用就关掉，`POST /shutdown` 不成就 `TerminateProcess`。

拉起失败时，把 `llama-server.exe` 的 **stderr 尾部（最后 20 行）**显示出来。手动安装最常见的两类错误——缺 DLL、模型文件损坏——都会在这里现形，比任何静态检查都准。

配置里保留三个后门：

```
mt_backend=local        ; local | external
mt_base_url=            ; external 时用，如 http://127.0.0.1:11434/v1
mt_model=               ; 指定用哪个 gguf，留空取第一个
mt_dir=                 ; 覆盖默认目录
```

---

## 5. 代码结构

```
src/mt/mod.rs     状态枚举 + probe() + translate()
src/mt/probe.rs   目录枚举、文件清单、嵌套目录纠正
src/mt/proc.rs    子进程、Job Object、健康检查、空闲回收、stderr 收集
src/mt/client.rs  /v1/chat/completions + HY-MT prompt 模板
src/settings.rs   加一张"翻译组件"卡片（路径、状态、打开文件夹）
src/overlay.rs    工具栏第 6 个按钮
```

对外三个函数：

```rust
pub fn probe() -> MtStatus;                       // 启动调一次，<1 ms
pub fn translate(text: &str, to: Lang) -> Result<String>;
pub fn open_component_dir();                      // 给 UI 的"打开文件夹"
```

`probe()` 在 `main` 里、窗口创建前跑一次就够。用户新放了目录后，用设置页的"重新检测"按钮手动触发即可，不需要文件监视。

---

## 6. 失败模式

| 情况 | 表现 | 处理 |
| --- | --- | --- |
| 没解压 / 放错位置 | MISSING | 显示绝对路径 + 打开文件夹 |
| 解压多套了一层 | 找不到 runtime | 自动用内层并注明 |
| 缺模型或下了一半 | INCOMPLETE | 按体积下界判定，文案说清缺什么 |
| 缺 DLL | 拉起时进程立刻退出 | 展示 stderr 尾部 |
| Program Files 部署 | 无影响 | **只读不写**，这条路彻底不存在了 |
| 端口被占 | 启动失败 | 换端口重试 3 次 |
| 显存不足 | 加载 OOM | 降 `-ngl`，再退 CPU |
| 主程序被强杀 | 孤儿进程 | Job Object 兜底 |
| U 盘被拔出 | 路径消失 | 视为 MISSING，不弹错误 |

**附带收益**：砍掉下载器之后，"exe 目录不可写"这个麻烦自动消失——程序只读不写。

---

## 7. GitHub Release 的打包规范

这现在是**唯一需要严格约定的东西**，结构错了程序就找不到文件。

- 资产名：`smartpaste-mt-1.0.0.zip`
- 压缩包内**顶层就是 `mt\` 目录**，不要多套一层文件夹
- 结构固定：

```
mt\
  VERSION.txt
  runtime\llama-server.exe + ggml-*.dll
  models\hy-mt1.5-1.8b-q4_k_m.gguf
```

- Release 说明写清三行：**下载 → 解压到 smartpaste.exe 同级 → 重启程序**，配一张目录截图。
- 组件版本与主程序版本独立。主程序不校验版本，只在设置页把 `VERSION.txt` 内容显示出来，方便排查。

---

## 8. 体积预算

| 项 | 增量 |
| --- | --- |
| 状态探测 + 目录枚举 + 嵌套纠正 | +15 KB |
| 进程管理（Job Object / 健康检查 / 回收） | +25 KB |
| 本地 HTTP 客户端（手写最小实现） | +20 KB |
| 工具栏图标 | +2 KB |
| **合计** | **约 +60 KB** |

不装翻译的用户，`smartpaste.exe` 从 1.1 MB 变成约 1.16 MB。
