# SmartPaste

Windows 上的轻量截图 + 钉图工具。全局热键唤起、框选、标注、复制/保存/钉到屏幕，全程 Win32 + GDI，单个 exe，无运行时依赖。

```
Ctrl+Shift+A  →  框选  →  标注  →  复制 / 保存 / 钉住
```

- 版本：0.3.0
- 平台：Windows 10 / 11（x64）
- 技术栈：Rust + `windows-sys`，纯 GDI，不依赖任何 UI 框架

## 功能

**抓取与框选**

- 全局热键默认 `Ctrl+Shift+A`，可在设置里改；托盘图标右键也可触发
- 先抓整个虚拟桌面再弹遮罩，所以遮罩本身不会被截进去；多显示器拼成一张大图
- Per-Monitor V2 DPI 感知，高 DPI 下尺寸标签与实际像素一致
- 框选时显示实时尺寸标签；松开鼠标后锁定选区，可继续标注

**标注工具**（相对选区左上角存储，改选区不影响已画内容）

| 工具 | 可调项 |
| --- | --- |
| 矩形 / 椭圆 | 6 色、3 档线宽、填充开关、直角/圆角 |
| 箭头 | 6 色、3 档线宽、实线/虚线 |
| 画笔 | 6 色、3 档线宽 |
| 文本 | 6 色、3 档字号（18/26/34 px）、加粗、描边、投影 |
| 橡皮 | 3 档笔刷粗细（12/24/40 px）、方形/圆形 |

文本点一下落笔，`Enter` 提交、`Shift+Enter` 换行、`Esc` 取消；输入走系统 EDIT 控件，
中文输入法可以直接用。

每个工具只显示自己用得上的属性项，选中色统一为玫红 `#FF2E63`。

**输出**

- **复制**：同时写入 `CF_DIB` 和 PNG 两种剪贴板格式 —— 只写 DIB 在浏览器里会丢，只写 PNG 老程序不认
- **保存**：PNG 落到 `%USERPROFILE%\Pictures\SmartPaste_<unix_ts>.png`（没有 Pictures 目录则用桌面）
- **钉住**：生成置顶贴图窗口，可拖动、滚轮缩放（0.1x–6.0x）、`Ctrl+滚轮` 调透明度（40–255），右击或双击关闭

## 快捷键

| 按键 | 作用 |
| --- | --- |
| `Ctrl+Shift+A` | 唤起截图遮罩（全局，可在设置里改） |
| 拖拽左键 | 框选 |
| `Enter` | 确认选区 → 复制到剪贴板 |
| `Ctrl+S` | 确认选区 → 保存 PNG |
| `Ctrl+Z` | 撤销上一笔标注 |
| 文本输入中 `Enter` / `Shift+Enter` / `Esc` | 提交 / 换行 / 取消 |
| `Esc` | 有工具时先退出工具，再按一次关闭遮罩 |
| 右键 | 取消截图 |

钉图窗口：滚轮缩放，`Ctrl+滚轮` 调透明度，右击/双击关闭。

## 构建

需要 MSVC 工具链（`link.exe`）和 Rust stable：

```bash
cargo build --release
# 产物：target/release/smartpaste.exe
```

`build.rs` 会把图标和版本信息编进 exe。它只调用 Windows SDK 自带的 `rc.exe`（不引入 `winres` 之类的依赖），自动在 `C:\Program Files (x86)\Windows Kits\10\bin` 下挑版本号最高、优先 x64 的那一个。找不到 `rc.exe` 时只打印一条 warning，程序照样能编，只是 exe 没有图标和版本信息。

发布 profile 用 `opt-level = "z"` + `lto` + `codegen-units = 1` + `panic = "abort"` + `strip`，主打体积小。

## 命令行参数

主要给调试和自动化测试用：

| 参数 | 作用 |
| --- | --- |
| `--selftest` | 数据链路自检：抓屏 → 裁剪 → 写剪贴板 → 存 PNG，结果写日志，不弹界面 |
| `--rect x y w h` | 遮罩直接锁定指定区域，跳过鼠标拖拽（模拟鼠标在 DPI 缩放下坐标会飘） |
| `--auto` | 启动 1.5s 后自我投递一次 `WM_HOTKEY`，验证消息链路 |

## 项目结构

```
src/
  main.rs       入口、消息循环、全局热键、托盘、命令分发
  overlay.rs    全屏遮罩：框选、工具条、属性条、绘制流水线
  edit.rs       标注元素的数据结构、GDI 绘制、撤销栈
  config.rs     配置读写（%APPDATA%\smartpaste\config.txt）
  settings.rs   设置窗口：卡片式面板、键帽展示、热键录制
  gdiplus.rs    GDI+ 的最小封装，只为抗锯齿的圆角与线条
  capture.rs    抓屏，直接持有 DIB Section（既是 HBITMAP 又能读像素）
  clipboard.rs  剪贴板，CF_DIB + PNG 双格式
  pin.rs        钉图窗口：置顶、拖动、缩放、透明度、软件柔和阴影
  icons.rs      工具栏图标：编译期嵌入雪碧图，运行时按色着色
  icon.rs       exe / 托盘图标
  win.rs        Win32 小工具（宽字符、矩形、字体、日志）
build.rs        编译期调用 rc.exe 打包图标与版本信息
resources/      app.rc：图标 + VERSIONINFO
assets/         图标源文件（svg / png / ico）、工具栏雪碧图
scripts/        从 SVG 重新生成图标的脚本（构建资源用）
temp/          开发期调试脚本与临时产物（git 忽略，不入库）

开发约定见 [AGENTS.md](AGENTS.md)：渲染铁律、验证方法、踩过的坑。
```

## 设计取舍

- **不引 UI 框架**。Win32 消息循环 + GDI 手绘，冷启动快、内存占用低，也避免为一个截图工具拖进几百 KB 依赖。
- **抓屏用 DIB Section**。同一块内存既是 `HBITMAP`（能 `BitBlt` 上屏），又能拿到像素指针直接裁剪，全程零拷贝。
- **绘制走单缓冲**。先把抓好的图 `BitBlt` 到后台缓冲，再用 1x1 黑位图 `AlphaBlend` 出选区外的半透明蒙层，然后画标注、边框、尺寸标签、工具条，最后整块一次性提交到窗口 DC —— 没有闪烁。
- **钉图用 `UpdateLayeredWindow`**。阴影要逐像素控制 alpha，`SetLayeredWindowAttributes` 只能整窗一个透明度、也画不出半透明边缘，所以整张图预乘 alpha 后一次提交。
- **热键注册在线程上**。主窗口是 `HWND_MESSAGE` 类型，收不到 `WM_HOTKEY`，线程级热键直接进本线程消息队列，循环里截获最稳。

## 已知限制

- 只支持 Windows。
- `BitBlt` 抓屏拿不到部分硬件加速内容（独占全屏游戏、部分硬件解码视频会黑屏）。
- 设置窗口目前只有热键一项，其他参数还没暴露。
- 热键被别的程序占用时无法保存，会回滚到原来的组合并弹提示。
- 没有滚动截图、OCR、贴图混合模式。

## 脚本

`scripts/` 只有构建资源用的工具，需要 Edge + Pillow：

| 脚本 | 用途 |
| --- | --- |
| `make_icon.py` | 从 `assets/smartpaste*.svg` 重新生成 `.ico`（9 帧，16→256） |
| `make_ui_icons.py` | 从 `assets/icons.svg` 重新生成工具栏雪碧图 `icons.png` |

`temp/` 是本地开发目录（**已 git 忽略**），放调试脚本和临时截图，比如端到端冒烟、热键探测、内存采样、遮罩状态截图。这些脚本依赖本机路径、随时会改，所以不纳入版本管理。

程序运行日志写在 `%TEMP%\smartpaste.log`。

## License

MIT
