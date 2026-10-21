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
- 窗口是 per-monitor DPI aware，但 UI 尺寸用的是硬编码 `UI_SCALE = 1.6`，
  **不是**系统 DPI。改缩放只改这一个常量，但 `icons::SIZE` 是独立硬编码的，要一起改。

---

## 5. 目录约定

- `scripts/` 只放**生成入库资源**的脚本（图标、雪碧图），需要 Edge + Pillow
- `temp/` 放调试脚本、临时截图、一次性工具，**已 git 忽略**，不进仓库
- `assets/_*.png` 也是 git 忽略的临时截图

---

## 6. UI 规格速查

```rust
UI_SCALE = (8, 5)          // 1.6 倍
ui(v) = v * 8 / 5          // 所有尺寸都过这个函数
```

| 用途 | 值 |
| --- | --- |
| 页面底色 | `#F7F7F9` |
| 卡片底 / 描边 | `#FFFFFF` / `#E6E6EC`，圆角 10 |
| 主 / 次 / 辅助文字 | `#1A1A20` / `#6E6E78` / `#9A9AA4` |
| 强调色（录制态） | `#FF2E63`，浅底 `#FFF2F6` |
| 描边宽度 | `HAIRLINE = 1.6` |

字体：中文 `Microsoft YaHei`，拉丁 `Segoe UI`。层级靠字号 + 字重 + 颜色三轴同时拉开。
