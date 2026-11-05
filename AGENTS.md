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
- **截图/点击脚本自己必须先声明 per-monitor DPI aware**（`SetProcessDpiAwarenessContext(-4)`）。
  否则 150% 缩放下系统给的是虚拟化坐标：屏幕 2560×1440 读成 1707×960，
  600×867 的窗口读成 400×578，`PrintWindow` 会把整窗内容缩着画进那张小位图，
  看起来像"窗口被裁掉了一半"；`SetCursorPos` 的坐标同样被放大约 1.5 倍，
  点击会落到别处（曾误判"鼠标事件没送到遮罩"）。

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
- **`SetWindowPos` 只给 `SWP_NOSIZE` 会把窗口挪到 (0,0)**，只调 Z 序还得给 `SWP_NOMOVE`
  （滚动截图控制条因此跑到过左上角，见 12.3）。
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

**颜色一律走 `theme::c()`，不要在绘制代码里写 `rgb(...)`。** 色板在 `theme.rs`
的 `Palette` 里，浅色/深色各一套，字段按用途命名（`surf_sel`、`on_surf_muted`…）。
唯一的例外是压在截图上的浮层（尺寸标签、倒计时、放大镜、线宽数值气泡）和
选区那圈黑白描边：它们要跟任意画面抢注意力，固定深色才稳定。

| 用途 | 浅色 | 深色 |
| --- | --- | --- |
| 窗口底色 | `#FAF7F8` | `#1B1A21` |
| 分隔线 | `#EEE3E7` | `#2E2D36` |
| 主 / 次 / 辅助文字 | `#221A1D` / `#6B5E63` / `#96898E` | `#F2F0F4` / `#A8A2AE` / `#7C7684` |
| 强调色（品牌玫红） | `#FF2E63` | `#FF2E63`（不变） |
| 浅底上的小节标题 | `#D90A42` | `#FF7C9E`（深底要提亮，压深就看不清） |
| 工具条 / 属性条底 | `#FFFFFF` | `#26252D` |
| 工具条选中格 | `#E3E3EB` | `#41404C` |
| 工具条图标（未选中） | `#45454C` | `#E6E4EA` |
| 描边宽度 | `HAIRLINE = 1.6` | 同左 |

字体：中文 `Microsoft YaHei`，拉丁 `Segoe UI`。层级靠字号 + 字重 + 颜色三轴同时拉开。

主题三档（浅色 / 深色 / 跟随系统）存在 `config.txt` 的 `theme=`，默认跟随系统。
跟随系统时读 `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`
的 `AppsUseLightTheme`（0 = 深色），结果缓存在 `theme::SYS_DARK`；系统换色会广播
`WM_SETTINGCHANGE`，设置窗口收到后调 `theme::refresh()` 重绘。**设置窗口和遮罩都是
绘制时现读色板**，所以没有"主题变了但某个窗口还留着旧色"的缓存问题——别引入。遮罩
每次弹出前也会 `refresh()` 一次（它常关着，可能错过广播）。

图标着色是在 `icons::load(palette)` 里一次性做完的，不缓存跨主题复用。

### 8.1 工具栏图标：视觉尺寸必须归一化

`assets/icons.svg` 是源头，`scripts/make_ui_icons.py` 渲成 `assets/icons.png`
（十格 x 96px），运行时按 `icons::SIZE = 32` 缩放并着色。

**渲染尺寸一致 ≠ 视觉尺寸一致。** Lucide 原始路径各自占位差得很远（copy 是双层
纸结构顶满 22 格，arrow 斜箭头只有 12 格），并排放在一条工具栏上肉眼立刻能看出
大小不齐。所以每格都包了一层变换：

```svg
<g transform="translate(tx,ty) scale(k)" stroke-width="2/k">…原路径…</g>
```

- `k` 按**墨水量**（含 2 单位描边）反解：墨水 bbox = `(几何 - 2) * k + 2`，
  即 `k = (目标 - 2) / (当前 - 2)`，`tx = 12 - cx*k`。
  直接拿"目标/当前"当 k 会整整偏大一档，别再踩。
- **必须补偿 `stroke-width = 2/k`**，否则缩放把线宽一起缩了，图标越小越细。
  渲染出来所有图标都应该是 2 单位线宽。
- 每格内容还要对中到 `(12,12)`，不然不同形状的重心会让整排看起来高低不一。

目标墨水尺寸（24 格单位）：

| 类别 | 图标 | 目标 |
| --- | --- | --- |
| 方形基准 | rect / save / copy / pen / eraser | 18 ~ 19 |
| 圆形 | circle | 19（光学上圆要略大于方，同尺会显小） |
| 文字 | text | 17.5 × 18.5 |
| 斜向 | arrow | 15.5（斜向填充率低，撑满对角线会显大，要特意收小） |
| 纵长 | pin | 14.5 × 20（保持自身长宽比） |
| 宽扁 | record | 21.5 × 15（同上） |

加图标 / 改图标后：先去掉那层 transform 改原始坐标，再跑 `make_ui_icons.py`，
它会把每格的墨水 bbox 打出来——离标准超过 1 个单位就是没对齐。

**颜色要跟着运行时状态变的图标**（目前只有属性条的调色板，第 11 格）：
雪碧图里画成纯白，运行时用 `icons::make(idx, [r, g, b])` 按当前颜色现做一张。
`make` 和 `load` 走同一个 `build()`，抗锯齿和预乘 alpha 完全一致，只是每次调用
会新建一张 32x32 的 DIB——每帧一次，微秒级，不用做缓存。雪碧图里那格纯白只是
底稿，不参与显示。

（真要做固定配色的彩色格时：`build` 的 tint 是把 RGB 整体替换掉的，彩色图标得
自带颜色另走一条路；另外外层 `<g>` 的 `stroke="#FFFFFF"` 会被子元素继承，
每个填色形状都要显式 `stroke="none"`，否则彩点外面会套一圈白边。）

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
  就放外面——外面既不依赖任何系统 API，也永远录不到。只有四面都被挡住
  （全屏或接近全屏）才退到选区内底边居中（距底边 72px），这时才调
  `WDA_EXCLUDEFROMCAPTURE`。
- **可放范围是工作区 `rcWork`，不是整个屏幕**（`MonitorFromRect` +
  `GetMonitorInfoW`）。用屏幕矩形判断时，接近全屏的选区会把控制条摆到
  任务栏那 60px 上：看得见、点不到，因为点击被任务栏吃掉。这条实际踩过。
- 排除失败（Win10 2004 以下）就再退回选区外，宁可挡内容也不污染录像。
- **本机看不到控制条不是 bug**：这台机器装了 `GameViewer Virtual Display
  Adapter`，用户是透过投屏 / 远程看桌面的。`WDA_EXCLUDEFROMCAPTURE` 会把窗口
  从*所有*捕获里摘掉，投屏画面本身就是一路捕获，所以窗口连用户的视野一起消失。
  换到物理显示器上直接看就正常。别为这个现象改架构——曾经试过"从选区底部让出
  60px 给控制条"，等于偷偷缩小录制区域，被明确否掉了：录制范围必须严格等于选区。
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

### 11.10 取色面板（HSV + 预设 + Hex + 吸管）

- SV 方块和色相条是**同一张 DIB**（`PK_SV_W × (PK_SV_H + PK_HUE_H)`），只在色相变化时重算。
  手工填像素必须过 `dib_px()`：`hsv_rgb` 返回的是 COLORREF，DIB 要 `0x00RRGGBB`。
  忘了换的话渐变会从粉红变成蓝紫，而色相条指针位置仍然是对的——很容易误判成"色相算错"。
- Hex 输入框是**独立的 EDIT**（id=2 + `hex_edit_proc`），不要和文字标注那个共用：
  一个提交成 Shape，一个只改颜色。`WM_CTLCOLOREDIT` 里它的字色必须写死——
  跟着当前颜色走的话，选了白色就是白字白底。
- 拖动时 `set_color(.., remember=false)` 不写盘，抬手 / 关面板才落一次。
  `hex_sync` 用来挡住自己 `SetWindowText` 触发的 EN_CHANGE，去掉会自己咬自己。
- `save_styles()` 是"load → 覆盖字段 → save"，顺带写 `ov.recent`。
  以后往 Config 加字段，记得在那里一起带过去，否则只在内存里亮一下就没了。
- **图标里的圆用 `gdiplus::stroke_arcs` / `fill_rings`（真贝塞尔圆弧），不要用采样折线。**
  半径只有十几像素时，每个采样点四舍五入到整像素，轮廓上会出现肉眼可见的平段——
  8 倍放大一看就是多边形。折线只适合本来就该有折角的东西。
- **小图标不要直接在目标尺寸上画，走雪碧图管线。**
  `icons.png` 是 96px 渲染再 Lanczos 缩到 32px，边缘有 2~3 级灰阶过渡；
  同样 27px 直接在 DC 上 `fill` 只有一级过渡，看着就是台阶、不圆润。
  属性条那个调色板最早是 GDI+ 手画的（椭圆 + 挖孔），就是栽在这上面，
  最后改回雪碧图的第 11 格。**判断标准：32px 的图标，宁可进雪碧图，也别手画。**
- **实心图形开缺口，不要拿一个圆去挖——圆和轮廓相交必然是尖角（cusp）。**
  32px 下就是轮廓上戳出两根刺，放大看尤其明显。缺口的正确做法是画进 outline
  本身，让两端用圆角过渡收进去。现成的 Material palette 路径就是这么设计的，
  直接用（`assets/icons.svg` 第 11 格）。
- `fill-rule="evenodd"` 只能挖**完全落在轮廓内**的形状。缺口圆有一截在轮廓外的话，
  那截奇偶性为 1 会被填实，凭空多出一块月牙（表现为 bbox 变宽、中心偏移）。
  孔都在内部时 evenodd 没问题，越界的要用 `mask`。
- 代码里还留着 `fill_rings` / `stroke_arcs` 两个 GDI+ 原语（都带 `allow(dead_code)`）。
  真要手画实心带孔图形时能用，但先确认是不是该走雪碧图。

---

## 12. 滚动截图（`scroll.rs` + `stitch.rs`）

定位就是**用户自己滚，程序只负责抓稳 → 对齐 → 拼**。为什么不注入滚轮写在 `scroll.rs`
开头：一次滚轮的位移在浏览器 / 编辑器 / Office 里完全不同，替用户滚只会更容易滚歪。

### 12.1 对齐只能从画面里反推

判定关系是 `cur[y] == prev[y + d]`，`y` 落在内容带 `[top, h - bottom - d)` 上。
三条规则缺一不可：

- **纯色行不参与打分**。一整片背景色在任何位移上都对得上，混进去会把结果拉向 0。
- **上下静止带先识别再排除**。吸顶表头每帧都在同一位置，不排除就等于"每帧都没动"。
  用"开头 / 结尾有多少行与上一帧完全一致"判断，上限 `h/3`（纯色页面整帧同色时别把画面吃光）。
- **候选一样好时取离上次位移最近的**。等距列表、重复文字会给出多个同样好的 d；
  滚动是连续的，跳变才是异常。第一次没有 hint 时取最小的 d。

比较用**量化后的行指纹**（每通道右移 3 位）而不是逐字节：同一段内容在两帧里会差一两级，
逐字节比会判成"变了"，整帧被丢。别把这道容差去掉。

### 12.2 必须等"稳定帧"

抓到就拼会拼出重影：滚动惯性中间那些帧是半位移的。**连续两次抓到的帧相似**才提交。
判断相似复用 `stitch::similar`（行指纹），不要写 `a == b`。

### 12.3 这一轮踩到的坑

- **`SetWindowPos` 只给 `SWP_NOSIZE` 等于把窗口挪到 (0,0)**。想只调 Z 序必须同时给
  `SWP_NOMOVE`。控制条就是这么跑到屏幕左上角的——日志一切正常，只有位置不对。
- **控制条不能带 `WS_EX_LAYERED`**（用 `SetWindowRgn` 抠圆角）。录屏那套已经踩过：
  layered 和 `SetWindowDisplayAffinity` 一起用时，窗口在屏幕上也一起消失。
- **`WM_PAINT` 里必须 `BeginPaint` / `EndPaint`**。只用 `GetDC` 画不会把更新区标记掉，
  于是永远在重绘。平时主动刷新走 `GetDC` 没问题，进 `WM_PAINT` 就必须走 Begin/End。
- 抓选区时 `BitBlt` 要带 `CAPTUREBLT`（`0x4000_0000`），否则分层窗口（提示框、半透明面板）
  截不进来，拼出来的图缺一块。

### 12.4 控制条看不见 / 会话自己关掉

这两件事都是一次真实的用户反馈（"点了双箭头，没看见控制条，也没拼上"），根因有两个：

- **控制条位置必须夹在显示器工作区里**（`MonitorFromRect` + `rcWork`），不能拿
  `SM_CYSCREEN` 当底边。全屏选区时会算出 `1440 - 48`，正好落在任务栏底下：
  `IsWindowVisible` 是真、`EnumWindows` 也找得到，但人眼看不见。
  四面都顶死时压在自己右下角，同时 `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)`。
- **"没滚动"不能按空闲超时收工**。原来的规则是 `frames > 0` 就 8 秒收工，
  而第一帧在 240ms 内就进来了，于是用户还在找位置/看页面，会话已经关了，
  还顺手把"只有首屏"的图复制进剪贴板——表现就是"截出来的图尺寸没变"。
  正确的判据是**累计增长**（`grown`，≥60px 才算滚过），因为它对桌面上的动画、
  光标闪烁这类几十像素的假位移不敏感。

### 12.5 怎么验证

单测在 `stitch.rs` 里，用合成"长网页" + 模拟滚动窗口，7 个用例覆盖普通滚动、吸顶表头、
同帧重复、5px 慢滚、超上限、无关帧、周期性内容。**改对齐逻辑先跑 `cargo test`。**

端到端用 `--scroll x y w h` 直接开会话（跳过框选），再用脚本发真滚轮。三个坑：

1. 测前先把目标页面**滚回顶部**——上次测试可能已经把它滚到底，滚轮没内容，几帧就收工；
2. 空闲判定 8 秒，脚本要等够（用 7 秒会读到上一轮残留在剪贴板里的图）；
3. 读剪贴板前先 `EmptyClipboard()`，否则分不清这一轮到底复制没有。

---

## 13. 提交约定（本仓库的工作习惯，不要外推到别的项目）

- **改完立即 `commit` + `push`**，不要攒着等提醒。
- **一个改动一个 commit**，不把不相关的改动塞进同一个提交。
- 提交信息沿用仓库现有风格：`feat(scope): …` / `fix(scope): …` / `docs: …`，正文中文。
- 提交前先 `git status` 确认没有把 `temp/`、测试产物、`%USERPROFILE%\Videos` 里的
  录制文件带进来（`temp/` 与 `assets/_*.png` 已 git 忽略，但 Videos 不在仓库内）。
- 推送后确认 `git log origin/main --oneline -1` 与本地 HEAD 一致再收工。


