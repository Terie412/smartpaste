# AGENTS.md

给接手这个仓库的人（或 AI）看的工程约定。只写**已经踩过、还会再踩**的坑，不写通用编程常识。

---

## 1. 项目速览

- Rust + 纯 Win32 / GDI，无 UI 框架，所有控件自绘
- 构建：`cargo build --release` → `target/release/smartpaste.exe`
- 运行：托盘常驻 + 全局热键（默认 `Ctrl+Shift+A`），**没有主窗口**
- 日志：`%TEMP%\smartpaste.log`（不是仓库里的 run.log）
- 配置：`%APPDATA%\smartpaste\config.txt`，纯 `key=value`
- 单实例：`Global\SmartPaste.SingleInstance`。**残留进程会让新实例静默退出**，
  测试脚本开头一律先 `taskkill /F /IM smartpaste.exe`
- 调试入口：`--selftest`、`--auto`、`--rect x y w h`、`--settings`

---

## 2. 渲染：UI 一律用 GDI+，不要用 GDI 的绘制原语

### 2.1 三条硬规则

1. **圆角、椭圆、斜线、任意曲线** → `gdiplus::fill_round / stroke_round / ellipse / line`。
   禁止 `RoundRect` / `Ellipse` / `Polyline`：GDI 这些接口**没有任何抗锯齿**，
   8~16px 圆角在 1:1 下就是肉眼可见的台阶。
2. **文字** → `gdiplus::draw_text`（`TextRenderingHintAntiAliasGridFit`）。
   禁止 `TextOutW` / `DrawTextW`：GDI 会对字形做网格拟合，CJK 的斜笔在 20~30px
   会出现白缺口和硬阶梯。
3. **描边宽度必须跟 UI 缩放走**。写死 `1.0` 的描边放在 1.6 倍 UI 里，
   在 16px 半径的圆角上会断成一节一节。用 `HAIRLINE = 1.6`，或 `ui(n)`。

### 2.2 例外：1px 直线仍然用 GDI

对齐像素的水平/垂直 1px 线（直角选区边框、分隔线）用 `FillRect` 最锐利。
换成 GDI+ 反而会被抗锯齿糊成两条半灰线。

### 2.3 同一个坑的三种长相（历史记录）

| 时间 | 症状 | 真因 | 修法 |
| --- | --- | --- | --- |
| 2026-10 | 遮罩圆角有明显台阶 | GDI `RoundRect` 无抗锯齿 | 圆角改走 GDI+ 路径 |
| 2026-10 | 尺寸标签文字带橙蓝色边 | ClearType 绘制在深色底上 | 改灰度抗锯齿 |
| 2026-10 | 设置面板文字"发毛"、描边断续 | GDI 网格拟合 + 1px 描边没缩放 | 文字走 GDI+，描边 1.6px |

**教训：凡是"像素边缘不好看"的问题，先问一句"这里是不是还在用 GDI"。**
三次都是同一个家族，只是表现不同。改完不要凭缩略图判断，按第 3 节验证。

### 2.4 不要指望 `CreateFontW` 的 quality 参数

实测（同一台机器、24px 雅黑、同一串中文）：

- 向 **alpha=255 的 32bpp DIB** 绘制：`CLEARTYPE_QUALITY(5)` 与 `ANTIALIASED_QUALITY(6)`
  **逐像素完全相同**，都带彩边（最大通道差 161）。
- 向 **alpha=0 的 32bpp DIB**（遮罩的 back buffer）绘制：输出是纯灰度（通道差 ≤ 1）。
- 向**窗口 DC** 绘制：灰度，无彩边。

也就是说，是否走 ClearType 取决于**目标表面的 alpha**，不是只看 quality 标志。
想确定性地拿到灰度抗锯齿，用 GDI+ 的 `TextRenderingHintAntiAlias*`。

### 2.5 GDI+ 初始化

`gdiplus::startup()` 在 `main` 最先调用。`StartupInput.SuppressBackgroundThread`
**必须是 0**：置 1 时 GDI+ 要求同时传出一个 `GdiplusStartupOutput`，
传 NULL 会直接返回 `GenericError(2)`，表现为所有圆角矩形凭空消失。

### 2.6 32bpp DIB 的通道顺序：R 和 B 是反的

`image::RgbaImage` 的像素是 `R,G,B,A`，而 32bpp top-down `BI_RGB` DIB 的内存字节序是
`B,G,R,X`，当成 u32 看就是 `0xAARRGGBB`。**手工拼 u32 时必须是 `R<<16 | G<<8 | B`。**

写反的后果不是"偏色"，是红蓝对调：纯黄 `(255,255,0)` 显示成青色 `(0,255,255)`，
肉眼看就是"钉住的图瞬间变蓝"。2026-10 `pin.rs` 就是这样写错的。

对照记忆：同一个仓库里两处正确写法——`capture.rs` 读（`b = px & 0xFF`, `r = px >> 16`）、
`clipboard.rs` 写（`[px[2], px[1], px[0], 255]`）。`overlay.rs` 不做手工转换（直接 BitBlt
原 DIB），所以遮罩里颜色一直是对的——**只有经过 `RgbaImage` 中转的路径才会中招**，
这也是为什么"钉住之前正常、钉住之后变色"。

验证手法：内存 DC 上 `GetPixel` 会返回 `CLR_INVALID`，要先 `BitBlt` 到 DDB 再读；
DIB→DDB 那一步就是 Windows 解释字节序的地方。见 `temp/check_dib_channels.py`。

---

## 3. 验证渲染：不要靠肉眼

- 截图至少等 **1.5 秒**。全屏 `BitBlt` 没刷完就抓，会截到上一帧（曾误判"属性条没画出来"）。
- 取窗口干净图用 `PrintWindow(hwnd, hdc, PW_RENDERFULLCONTENT=2)`，再裁掉 9px 外框，
  比全屏截图少掉阴影和背景干扰。
- 判断有没有彩边：统计区域内的 `max(R,G,B) - min(R,G,B)`。
  **> 10 就是有子像素渲染**；灰度抗锯齿应该 ≤ 2。
- 判断有没有抗锯齿：看灰度级数量和中间调像素比例，二值化（只有两三种灰度）说明 AA 没开。
- 放大看细节用 `Image.NEAREST`，不要用 LANCZOS——后者会把台阶糊掉，问题就看不见了。

```python
# 常用套路
im.crop(box).resize((w*6, h*6), Image.NEAREST)          # 看边缘
max(max(p)-min(p) for p in crop.getdata())              # 看彩边
```

---

## 4. 窗口与消息：踩过的坑

- **`CreateWindowExW` 收的是外框尺寸**，不是客户区。要客户区精确尺寸就先用
  `AdjustWindowRectEx` 换算，否则底部内容会被标题栏吃掉。
- **`WM_ERASEBKGND` 忘了 import** → 它在 match 里变成变量绑定，匹配一切，
  后面所有分支不可达。编译只报 `unreachable pattern`，运行时表现为窗口半死。
- **Alt 组合键走 `WM_SYSKEYDOWN`**，不是 `WM_KEYDOWN`；而且主键的 keydown 可能被
  系统当菜单加速键吞掉，只有 keyup 能收到。录制热键时要两类消息都处理，并用
  `GetAsyncKeyState` 兜底修饰键状态（`GetKeyState` 依赖本线程队列进度，会读到 0）。
- **录制热键前必须先注销已有热键**，否则按到当前生效的组合会被系统直接拿去触发截图。
- 窗口是 per-monitor DPI aware，但 UI 尺寸用的是硬编码缩放，**不是**系统 DPI：
  遮罩 `overlay.rs` 用 1.6（截图时的临时 UI，可以大），设置面板 `settings.rs` 用 1.25
  （常态窗口，贴近系统字号）。`icons::SIZE` 是独立硬编码的，改缩放要一起改。
- **没有 manifest 时，Win11 会把进程当旧程序**：窗口方角、标题栏是 Win10 的直角按钮。
  `resources/app.manifest` 里的 `supportedOS` 那一段就是修这个的，别删。
- 设置窗口的标题栏是自绘的（原生 caption 的关闭按钮改不了圆角）。两个要点：
  1. `WM_NCCALCSIZE` 必须返回 0，而且要在取全局 State 之前处理——它在
     `CreateWindowExW` 内部就会发一次，那时 State 还没建好，落到 `DefWindowProc`
     就会把原生标题栏算进去，之后再也去不掉。
  2. 创建时 `wParam` 是 `FALSE`，只挡 `TRUE` 的话初始标题栏仍会留下，两种都要返回 0；
     建完再 `SetWindowPos(..., SWP_FRAMECHANGED)` 强制重算一次最稳。
- 自绘标题栏要保留 `WS_CAPTION` 但去掉非客户区，DWM 的圆角和阴影才会继续生效；
  拖动靠 `WM_NCHITTEST` 返回 `HTCAPTION`，关闭按钮那块返回 `HTCLIENT`。

---

## 5. 文本输入（EDIT 控件）

- 文字的输入框必须用系统 `EDIT` 控件：中文输入法要 IME 组字、候选窗、光标跟随，
  自己收 `WM_CHAR` 是打不出中文的。
- **子类化必须用 comctl32 的 `SetWindowSubclass` + `DefSubclassProc`。**
  用 `SetWindowLongPtrW(GWLP_WNDPROC)` 拿「原过程」再 `CallWindowProcW` 转发，
  在系统 EDIT 上会稳定失败：`WM_GETTEXTLENGTH` 返回 0，读出来的文字永远是空串。
  这个坑很隐蔽——控件看起来一切正常，就是读不到字。
- `EDIT` 内部的换行是**单个 `\r`**。归一化成 `\n` 时不能把 `\r` 当垃圾删掉，
  否则多行会并成一行。
- 判断 Shift 有没有按下，用 `GetAsyncKeyState`。`GetKeyState` 依赖本线程队列的
  处理进度，注入的按键经常读不到（设置面板录热键时已经踩过一次）。
- 输入法组字期间，按键会以 `VK_PROCESSKEY` 到达，`Enter` 先被输入法拿去上屏，
  用户要再按一次才提交——这是正常行为，不要试图拦截。
- Enter / Esc 只投递消息给遮罩窗口，**不要在 EDIT 自己的窗口过程里销毁它**，
  那等于在自己的栈上拆房子。

## 6. 选区的坐标约定（改动时最容易踩）

- 所有标注的坐标都**相对选区左上角**存储，绘制时加 `(sel.left, sel.top)`。
- 因此选区一旦移动或改动**左/上**边界，必须调用 `Doc::shift(-dx, -dy)` 反向平移，
  否则标注会跟着选区一起跑，看起来像"画的东西自己动了"。
  改右/下边界不用平移。
- 锁定后的拖拽状态机集中在 `Grab`：`Edge(位掩码)` 改边、`Move` 整体移动、
  `None` 空闲。边带宽度是 `ui(4)`，角上两条边同时命中就是角拖拽。
- 选了工具时，内部拖拽归工具；`Ctrl` 按下时强制走移动。

## 7. 目录约定

- `scripts/` 只放**生成入库资源**的脚本（图标、雪碧图），需要 Edge + Pillow
- `temp/` 放调试脚本、临时截图、一次性工具，**已 git 忽略**，不进仓库
- `assets/_*.png` 也是 git 忽略的临时截图

---

## 8. UI 规格速查

```rust
// overlay.rs：1.6 倍（截图时的临时 UI）
UI_SCALE = (8, 5)
ui(v) = v * 8 / 5

// settings.rs：1.25 倍（常态窗口，对齐系统 125% DPI）
UI_SCALE = (5, 4)
```

所有尺寸都过各自的 `ui()`，不要写死像素。

| 用途 | 值 |
| --- | --- |
| 页面底色 | `#F7F7F9` |
| 卡片底 / 描边 | `#FFFFFF` / `#E6E6EC`，圆角 10 |
| 主 / 次 / 辅助文字 | `#1A1A20` / `#6E6E78` / `#9A9AA4` |
| 强调色（录制态） | `#FF2E63`，浅底 `#FFF2F6` |
| 描边宽度 | `HAIRLINE = 1.6` |

字体：中文 `Microsoft YaHei`，拉丁 `Segoe UI`。层级靠字号 + 字重 + 颜色三轴同时拉开。

---

## 9. 钉图（pin.rs）：透明度与提示条

- **透明度不要用 `BLENDFUNCTION.SourceConstantAlpha`**，要乘进像素 alpha 里
  （别忘了 RGB 一起预乘）。整窗常量 alpha 会把提示条自己也调透明——把图调到
  20% 时，正好要看"20%"的那个提示条也只剩 20%。
- 提示条药丸必须是**完全不透明**的：GDI+ 在预乘 alpha 的半透明底上画字，
  会把预乘过的 RGB 当直通色再混一遍，字边出一圈发亮毛边。
  药丸的形状用有符号距离场自己算覆盖率（`round_coverage`），不信 GDI 的抗锯齿。
- 提示条贴**图片左上角**，因为缩放的锚点就是左上角（窗口左上角不动、尺寸往右下长）。
  放底部居中的话每缩放一格它自己也会往下跑。
- 滚轮事件里 `SetTimer(hwnd, TIMER_HUD, 900ms)` 重置停留，`WM_TIMER` 里清掉
  再重画一次撤掉提示条。透明度/缩放都存成**百分点**（`alpha_pct`），
  这样提示条上的数字和实际值严格一致，不会出现"显示 60% 实际 58%"。

---

## 10. 遮罩的十字光标（自绘）

- **别用系统 `IDC_CROSS`**：纯黑单色，压在深色桌面上直接消失。
- **也别用"反色"方案**：50% 中性灰反色后还是 50% 灰（128→127），照样看不见。
- 现方案 `make_cross_cursor()`：自绘 32bpp 光标，黑白**沿轴交替**（每 4px 换色）。
  相邻段互为正反面，任何底色上总有一半线段对比度够高，眼睛能把整条线连起来。
- 光标位图按中心镜像对称，所以 `CreateBitmap` 要求的 bottom-up 行序对图案没有影响；
  单色遮罩（1 = 透明）也照实填——万一系统不认 32bpp 的 alpha，形状也还在。
- **截图验证光标**：全屏 BitBlt 不含光标，要 `GetCursorInfo` 拿到句柄再 `DrawIconEx`
  自己叠上去。注意目标 DC 的原点是裁剪框左上角，不是屏幕原点（写错过一次，
  表现为"截出来根本没有光标"）。工具见 `temp/curcap.py`。

---

## 11. 录屏（`record/`）：全 GPU 通路

### 11.1 数据流

```
DDA 整屏纹理
  → CopySubresourceRegion 裁到 clean
  → CopyResource 到输出画布（6 张轮转）
  → 光标 64×64 小块回填
  → MFCreateDXGISurfaceBuffer
  → SinkWriter（硬编）→ MP4
```

整帧不回读 CPU。唯一往返 CPU 的是光标覆盖的那一小块。

### 11.2 三个必踩的坑

1. **`MFCreateDXGISurfaceBuffer` 之后必须 `SetCurrentLength(w*h*4)`**。
   不补这句，`WriteSample` 直接返回 `E_INVALIDARG`，而错误信息完全不指向这里。
   文档里没写，只有踩过的人知道。
2. **指定 adapter 时 driver type 必须是 `D3D_DRIVER_TYPE_UNKNOWN`**。
   `D3D11CreateDevice(Some(&adapter), D3D_DRIVER_TYPE_HARDWARE, ...)` 会返回
   `E_INVALIDARG`。DDA 必须按 adapter 枚举（`DuplicateOutput` 要 device 和 output
   同源），所以这个躲不开。
3. **`WM_APP + n` 的消息常量会撞值**。`WM_SNAP_DONE` 和后来的 `WM_REC_STOP`
   都取了 `WM_APP + 2`，编译只报一句 `unreachable pattern`，运行时表现为
   "点了停止没反应"——消息被前一个分支悄悄吃掉了。加消息前先 grep 一遍
   `WM_APP +`。
4. **局部更新要用 `DEFAULT` 上传纹理 + `CopySubresourceRegion`**。
   `read_block` 能读回来，不代表 MF 能读到 —— 实测 `UpdateSubresource` 带 box 的
   局部更新在编码结果里完全不可见，整帧更新却正常。另外
   `D3D11_USAGE_DYNAMIC` 的纹理**不能**当 `CopySubresourceRegion` 的源，会静默失败。

### 11.3 其它约定

- 画布 6 张轮转。MF 异步读纹理，张数少了会读到被后续帧覆盖的内容。
- 光标小块：`GetCursorInfo` + `GetIconInfo`，画完必须
  `DeleteObject(hbmMask/hbmColor)`，否则每帧泄漏两个 GDI 对象。
- 帧节奏用 `CREATE_WAITABLE_TIMER_HIGH_RESOLUTION`。`Sleep(1)` 的 15.6ms 精度
  喂不了 60fps。
- 编码跟不上时**跳帧并推进时间戳**，成片时长才准；不能"少写几帧但时间戳连续"。
- DDA 只在屏幕变化时给帧，无新帧就复用 `clean` 画布。
- HDR 屏输出 `R16G16B16A16_FLOAT`，v1 直接拒绝并提示关 HDR；跨屏不支持，
  选区必须完整落在一块显示器内（`Capturer::new` 会拒绝）。
- `DXGI_ERROR_ACCESS_LOST` 只能停止并 `Finalize`，不要热重建——会让时间戳断裂。
- 调试入口：`smartpaste --rectest x y w h seconds [path] [fps]`，结果写
  `%TEMP%\smartpaste.log`；加 `--soft` 强制微软软编做对照。

### 11.4 实测基线（2560×1440 / RTX 3060 Ti / Win11 22621）

| 场景 | 帧率 | 丢帧 | CPU |
| --- | --- | --- | --- |
| 1440p60 全屏动态 | 60 | 0 | 1.2% |
| 1440p30 全屏动态 | 30 | 0 | 0.2% |
| 1080p60 区域动态 | 60 | 0 | 0.6% |
| 1440p60 强制软编 | 60 | 0 | 8.9% |

CPU 口径：进程 CPU 时间 ÷ 墙钟时间，1.2% = 0.012 个核。

### 11.5 验证方法

抽帧**不要用 `ffmpeg -ss`**：非 faststart 的 MP4（moov 在末尾）会 seek 错，
稳定抽出黑图，一度误判成"编码管线坏了"。用
`-vf "select=eq(n\,120)"` 按帧号取，再用 `-vsync 0` 落盘。

### 11.6 会话与 UI（`record/session.rs`）

- 录制跑在独立线程，主线程只做开录、每秒刷托盘、停止时 join。60fps 循环
  放主线程会把热键和托盘全堵死。
- 录制中再按一次全局热键是"停止"，不是重新截图。`WM_HOTKEY` 和托盘菜单
  两条路径都要判 `session::is_recording()`。
- 红框是 `WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE` 的分层窗口，
  画在选区**外面** 3px。位置在选区外，所以不会被 DDA 录进去。
- 控制条是**普通窗口 + `SetWindowRgn` 圆角**，不是 layered。
  `UpdateLayeredWindow` 和 `WDA_EXCLUDEFROMCAPTURE` 一起用时，窗口在显示器上
  也会一起消失（实测 `IsWindowVisible` 返回 1，但屏幕上看不见）。
- **位置策略：先外后内。** 依次尝试选区的上 / 下 / 右 / 左，只要有一边放得下
  就放外面——外面既不依赖任何系统 API，也永远录不到。只有四面都被屏幕边缘
  挡死（基本就是全屏录制）才退到选区内底边居中（距底边 72px，让开任务栏），
  这时才调 `WDA_EXCLUDEFROMCAPTURE`。
- 排除失败（Win10 2004 以下）就再退回选区外，宁可挡内容也不污染录像。
- `SMARTPASTE_HUD_NOEXCLUDE=1` 可以跳过排除，用来区分"没画出来"和"被排除掉了"。
  排查时先用它跑一遍，看屏幕截图里有没有控制条。
- 副作用：系统截图（Win+Shift+S）和其他录屏软件同样看不到控制条，这是 API
  的设计，不是 bug。调试时用 PIL 的 `ImageGrab` 也截不到它，别误判成"没画出来"。
- 控制条上的颜色必须走 `rgb(r,g,b)`（COLORREF）。直接传红框用的
  `0x00FF2E63` 这种写像素常量，会被 `fill_round` 里的 `argb()` 再转一次，
  玫红变紫（R/B 对调）。
- 倒计时用遮罩自己的 `SetTimer`，不另开线程；倒计时结束才 `finish(CMD_RECORD)`，
  这时遮罩已经关掉，录到的是干净桌面。
- 录制不产出 `RgbaImage`：`finish(CMD_RECORD)` 只记 `PENDING_RECT`，
  省掉一次整块裁剪和 8MB 中转。
- 不足 1 秒的录制直接删文件；线程报错也删，绝不留半截 MP4。

### 11.7 保存位置与另存为

- 默认落 `Config::video_dir()`：配置了就用配置，否则 Videos → Desktop → 临时目录。
- 勾了"录完询问另存为"时，录制先写 `%TEMP%\rec_SmartPaste_*.mp4`，
  停止后主线程弹 `savedlg::ask_video`，同盘 `rename`、跨盘退化成 `copy`；
  用户取消就删临时文件。
- `SHBrowseForFolderW` 在 `windows-sys` 0.59 里没有导出，选目录和打开目录都走
  `src/explorer.rs`（`windows` crate）。别在 settings 里直接用 windows-sys 找它。

### 11.8 单实例会让新构建"跑不起来"

托盘常驻 + `Global\SmartPaste.SingleInstance`：**旧进程还在托盘里时，双击新
exe 会直接静默退出**，用户以为在验证新代码，其实还在跑旧代码。这条坑过一次——
改了控制条可见性，用户反馈"还是没有"，实际是上一版进程没退。

现在再启动第二个实例会弹框提示"请先在托盘右键退出"。改完代码让用户验收前，
先确认没有残留进程（测试脚本开头一律 `taskkill /F /IM smartpaste.exe`）。

### 11.9 模拟点击测试：先设 DPI 感知

非 DPI-aware 的脚本里 `SetCursorPos(811, 839)` 在 125% 缩放下会落到物理
`(1014, 1049)`，表现为"点了没反应"。脚本开头必须
`ctypes.windll.user32.SetProcessDPIAware()`，之后所有坐标都是物理像素。
`GetWindowRect` 的返回值也会跟着变，别混用两套坐标。

坐标也别从截图目测。按布局常量算：设置窗口客户区原点等于窗口左上角
（`WM_NCCALCSIZE` 返回 0），所有 `ui(378)` 这类值直接加窗口偏移即可。
目测过一次差了 90px，凭空多排查了半小时。

---

## 12. 提交约定（本仓库的工作习惯，不要外推到别的项目）

- **改完立即 `commit` + `push`**，不要攒着等提醒。
- **一个改动一个 commit**，不把不相关的改动塞进同一个提交。
- 提交信息沿用仓库现有风格：`feat(scope): …` / `fix(scope): …` / `docs: …`，正文中文。
- 提交前先 `git status` 确认没有把 `temp/`、测试产物、`%USERPROFILE%\Videos` 里的
  录制文件带进来（`temp/` 与 `assets/_*.png` 已 git 忽略，但 Videos 不在仓库内）。
- 推送后确认 `git log origin/main --oneline -1` 与本地 HEAD 一致再收工。


