# 推理框架调研：llama.cpp 是不是过时了

> 调研时间：2026-10-22。触发问题：听说有个叫 Strata 的框架能大幅降低同一个模型的硬件要求，llama.cpp 是不是老了？

---

## 0. 结论

| 问题 | 回答 |
| --- | --- |
| Strata 是真的吗 | 真的。2026-10 出现，MIT，C++20 + CUDA，作者 Niko Veit。但**它是给超大 MoE 模型做专家卸载的专用引擎，不是通用加速器** |
| 它能帮我们吗 | **不能**。我们的模型是 dense 1B（OCR）和 dense 1.8B（MT），没有 MoE 稀疏性可利用；Strata 要 64 GB 内存才跑得动它支持的那一个模型 |
| llama.cpp 老了吗 | **不老，但定位不同**。它现在还在日更（build 号已经 b11xxx）。它优化的是"可嵌入 + 跨硬件 + 单文件分发"，vLLM/Strata 优化的是别的 |
| 该换吗 | 不该。这个项目的约束是"塞进一个 Rust 桌面程序、无 Python、用户自己下模型"，目前只有 llama.cpp 满足 |

一句话：**新框架降低硬件要求，靠的是利用模型结构里的某种稀疏性（MoE 专家、激活稀疏、极低比特），不是通用的"更快"。模型结构不对口，换框架一分钱不省。**

---

## 1. Strata 到底是什么

它的目标写得很明确：让消费级 PC 跑 **Qwen3.8-Flash-Next（约 125B MoE）**。

做法是三层存储 + 异构并行：

- 模型有 24,576 个专家（48 层 × 512），每个 token 路由只激活约 10 个 → **99.96% 的专家参数是冷的**
- 常驻显存：注意力、路由器、共享专家、输出头、KV 热区
- 专家权重放**系统内存**，最热门的几千个缓存在显存
- 显存没命中的专家，**CPU 用 AVX-512 同步算**，和 GPU 并行不互相等
- 更冷的权重放 SSD，配一张约 29 GB 的查找表
- 附加：投机解码（1.6–1.8×）、Hadamard 旋转 + 4-bit KV 量化（长上下文）

实测口径（官方自报，RTX 5070 12 GB + 64 GB RAM）：Q2_0 约 94 tok/s，IQ3_S 约 53 tok/s。

**关键点：它把"显存不够"换成了"内存要够"。** 12 GB 显存 + 64 GB 内存跑 125B——总内存需求是 76 GB，不是"变小了"，是**换了个地方放**。32 GB 内存的机器直接出局。

而且它是**单模型专用**：针对 Flash-Next 的专家布局和路由写的内核。换个模型，尤其是 dense 模型，它的核心机制直接失效。

---

## 2. 2026 年推理框架全景（按优化目标分，不按新旧分）

| 框架 | 优化目标 | 典型负载 | 能塞进 Rust 桌面程序吗 |
| --- | --- | --- | --- |
| **llama.cpp** | 可嵌入、跨硬件、单文件 | 消费级、端侧、CPU/Vulkan/CUDA/Metal | ✅ 唯一现实选择 |
| Ollama | 易用性（llama.cpp 的封装 + 守护进程） | 个人尝鲜 | ⚠️ 多一层进程和依赖，不如直接用 llama-server |
| vLLM | 服务端吞吐（PagedAttention） | 多并发 API | ❌ Python + CUDA + 大显存 |
| SGLang | 结构化输出的前缀复用（RadixAttention） | 多轮 / JSON 约束生成 | ❌ 同上 |
| TensorRT-LLM | NVIDIA 卡上的极限延迟 | 生产推理集群 | ❌ 要编译引擎、绑死 N 卡 |
| **Strata** | 超大 MoE 的显存卸载 | 125B MoE 跑在 12 GB 卡上 | ❌ 单模型专用 + 要 64 GB 内存 |
| KTransformers | CPU+GPU 混合跑 MoE | DeepSeek 级 MoE 本地 | ❌ 同上，模型不对口 |
| MLC-LLM | TVM 编译到任意后端 | 跨端部署固定模型 | ⚠️ 每个模型要单独编译，和"用户自己下模型"冲突 |
| OpenVINO GenAI | Intel CPU/iGPU/NPU 加速 | 轻薄本、AI PC | ⚠️ **值得评估**，见下 |
| ONNX Runtime | 通用算子图 | 传统 CV/OCR | ✅ 但只适合 PP-OCR 这类小模型 |
| MLX | Apple Silicon | Mac | ❌ 平台不符 |

**注意这张表的一个事实**：真正"降低硬件要求"的框架（Strata、KTransformers、PowerInfer、bitnet.cpp）全部依赖**模型结构特性**——MoE 专家稀疏、激活稀疏、1.58-bit 权重。它们不是通用加速器。dense 1B 模型在这类框架上不会比 llama.cpp 快，通常更慢（少了针对性的 kernel 调优）。

---

## 3. llama.cpp 到底"老"在哪，不老在哪

**不老的证据**：build 号已经到 b11xxx（一天几个 build），HunyuanOCR 的 DFlash 投机解码、PaddleOCR-VL 的架构支持都是**今年才合入主线**的。它不是停滞项目，是持续跟进新模型的。

**它真正的短板**：
1. 高并发吞吐打不过 vLLM/SGLang（没有 continuous batching 的深度优化）——但我们只有单用户单请求，用不上
2. 长上下文 prefill 不如 SGLang 的 RadixAttention 复用——我们上下文只有 2K
3. 部分新架构要等社区适配（HY-MT 的 1.25-bit STQ kernel 至今没合）

**它不可替代的地方**（正好是这个项目需要的）：
- 纯 C/C++，无 Python，能编成 DLL / 静态链接 / 单 exe
- CPU SIMD（AVX2/AVX512）+ Vulkan（N/A/Intel 通吃）+ CUDA + Metal
- GGUF 模型即文件，用户下完就能用，不需要"为每个模型编译一次引擎"
- 社区量化生态最全（官方 GGUF、IQ 系列、imatrix）

---

## 4. 那什么才是真正能降低门槛的手段

按性价比排序，都不是"换框架"：

| 手段 | 收益 | 代价 |
| --- | --- | --- |
| **只送选区，不送整屏** | 图像 token 数按像素线性下降，视觉开销可能降到 1/5–1/10 | 无（本来就是这个需求） |
| **限制输入长边**（如 1024 px） | 同上，且防止 4K 全屏把 prefill 打爆 | 极小字可能糊 |
| **Vulkan 后端** | 比纯 CPU 快 2–4 倍，只多 10–20 MB | 驱动兼容性要测 |
| **量化**（LM Q4/Q8，mmproj 保持 f16） | 内存减半 | Q4 对小模型精度有损，需实测 |
| **空闲卸载** | 常驻内存从 2 GB 降到 0 | 冷启动 2–5 s |
| **投机解码**（DFlash 已内建） | 长输出 2× 左右 | 多 0.2 GB 草稿模型 |
| **OpenVINO / NPU 后端** | Intel 平台上 iGPU/NPU 比 CPU 快数倍，功耗低 | 只覆盖 Intel；要评估 VLM 支持度 |

前两条加起来的效果，比换任何一个框架都大。

---

## 5. 给这个项目的结论

1. **继续用 llama.cpp**，但只用它的**两个后端**：Vulkan（默认）+ CPU（兜底）。不碰 CUDA（600 MB+ 运行时）。
2. **Strata 这类框架只在一种情况下值得回头看**：哪天我们决定在本地跑一个百亿参数以上的 MoE 模型，并且用户机器有 64 GB 内存。当前需求（1B OCR / 1.8B MT）完全用不上。
3. **真正该测的替代后端是 OpenVINO**：Intel 核显/Arc/NPU 在 Windows 上很常见，如果 HunyuanOCR 的视觉塔能在 iGPU 上跑，延迟可能从 8 s 降到 2 s。这是唯一有实质收益的"换框架"选项。
4. **别被"新"绑架**。框架选择的第一性问题是：**我的瓶颈在哪**。这里的瓶颈是"单次请求的视觉编码 + 短文本解码"，那是硬件和模型决定的，不是调度器决定的。

---

## 6. 参考

- Strata：MIT，C++20 + CUDA，2026-10 出现在 GitHub（Niko1221/Niko Veit），面向 Qwen3.8-Flash-Next 的专家卸载；12 GB VRAM + 64 GB RAM 实测 53–94 tok/s（官方自报）
- llama.cpp 主线：HunyuanOCR DFlash（PR #28890，b11103+）、PaddleOCR-VL（b8110+）
- OpenVINO GenAI：Intel CPU/iGPU/NPU 的 VLM 路径，值得单独评估
- vLLM / SGLang / TensorRT-LLM：服务端吞吐导向，不适用于嵌入式桌面场景
