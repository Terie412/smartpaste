//! 主题：浅色 / 深色 / 跟随系统。
//!
//! 界面上所有颜色都从这里取，绘制代码里不要再写死 `rgb(...)`。
//! 只有两类例外：压在截图上、注定要跟画面抢注意力的浮层（尺寸标签、
//! 倒计时、放大镜、数值气泡），它们是固定深色，不进色板。
//!
//! 「跟随系统」读的是注册表 `HKCU\...\Themes\Personalize\AppsUseLightTheme`，
//! 0 = 深色。系统切换深浅色会广播 `WM_SETTINGCHANGE`，收到后调 [`refresh`]
//! 重新判定；读注册表不便宜，所以结果缓存在 `SYS_DARK` 里。

use windows_sys::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD};

/// 主题模式。`System` 是默认值，也是老配置文件缺项时的取值。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Light,
    Dark,
    System,
}

/// 设置面板里从左到右的排列顺序。
pub const MODES: [Mode; 3] = [Mode::Light, Mode::Dark, Mode::System];

impl Mode {
    /// 写进 config.txt 的值
    pub fn key(self) -> &'static str {
        match self {
            Mode::Light => "light",
            Mode::Dark => "dark",
            Mode::System => "system",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" => Some(Mode::Light),
            "dark" => Some(Mode::Dark),
            "system" | "auto" | "follow" => Some(Mode::System),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Mode::Light => "浅色",
            Mode::Dark => "深色",
            Mode::System => "跟随系统",
        }
    }
}

/// 一套颜色。字段名按用途起，不按色值起——深色不是把浅色取反，
/// 每一档单独调过：深色底的描边要往亮走、强调色要提一档才够对比。
#[derive(Clone, Copy)]
pub struct Palette {
    // ---- 通用 ----
    pub bg: u32,
    pub divider: u32,
    pub txt1: u32,
    pub txt2: u32,
    pub txt3: u32,

    // ---- 强调色 ----
    /// 品牌玫红。两套主题都用原色，深色底上它自己就够亮
    pub accent: u32,
    /// 浅底上压深一档、深底上提亮一档。用在 12px 分节标题这类小字上
    pub accent_label: u32,
    pub on_accent: u32,

    // ---- 截图快捷键那张 hero 卡 ----
    /// hero 卡两套主题都是玫红实心：它是主操作，不该跟着背景一起变暗
    pub hero_bg: u32,
    pub hero_hover: u32,
    pub hero_txt: u32,
    pub hero_muted: u32,
    /// 录制态描边（金黄）
    pub hero_accent: u32,
    /// 不可用态（恢复默认按钮的灰字）
    pub hero_off: u32,

    // ---- 键帽 ----
    pub key_bg: u32,
    pub key_bg_hover: u32,
    pub key_border: u32,
    pub key_border_hover: u32,
    pub key_txt: u32,
    pub plus: u32,

    // ---- 控件 ----
    pub hover_bg: u32,
    pub segment_bg: u32,

    // ---- 截图遮罩：工具条 / 属性条 ----
    /// 工具条和属性条的底色
    pub surf: u32,
    pub surf_hover: u32,
    pub surf_sel: u32,
    pub surf_border: u32,
    pub surf_sep: u32,
    /// 工具条上的图标、开关的"开"态
    pub on_surf: u32,
    /// 开关的"关"态
    pub on_surf_muted: u32,
    /// 形状阴影那格：后面那片灰
    pub prop_shadow: u32,

    // ---- 取色面板 ----
    pub panel_bg: u32,
    pub panel_border: u32,
    pub field_bg: u32,
    pub field_border: u32,
    pub field_hover: u32,

    // ---- 工具条操作图标（这两个是固定色，深色下要提亮） ----
    pub icon_copy: u32,
    pub icon_pin: u32,
}

impl Palette {
    pub const LIGHT: Palette = Palette {
        bg: rgb(0xFA, 0xF7, 0xF8),
        divider: rgb(0xEE, 0xE3, 0xE7),
        txt1: rgb(0x22, 0x1A, 0x1D),
        txt2: rgb(0x6B, 0x5E, 0x63),
        txt3: rgb(0x96, 0x89, 0x8E),
        accent: rgb(0xFF, 0x2E, 0x63),
        accent_label: rgb(0xD9, 0x0A, 0x42),
        on_accent: rgb(0xFF, 0xFF, 0xFF),
        hero_bg: rgb(0xFF, 0x2E, 0x63),
        hero_hover: rgb(0xD9, 0x0A, 0x42),
        hero_txt: rgb(0xFF, 0xFF, 0xFF),
        hero_muted: rgb(0xFF, 0xF0, 0xF4),
        hero_accent: rgb(0xFF, 0xC3, 0x00),
        hero_off: rgb(0xFF, 0x9F, 0xBA),
        key_bg: rgb(0xFF, 0xFF, 0xFF),
        key_bg_hover: rgb(0xFF, 0xE9, 0xEF),
        key_border: rgb(0xEC, 0xDA, 0xE0),
        key_border_hover: rgb(0xFF, 0x2E, 0x63),
        key_txt: rgb(0x2A, 0x1A, 0x20),
        plus: rgb(0xFF, 0xC6, 0xD6),
        hover_bg: rgb(0xF6, 0xEC, 0xEF),
        segment_bg: rgb(0xF1, 0xE7, 0xEA),
        surf: rgb(0xFF, 0xFF, 0xFF),
        surf_hover: rgb(0xF1, 0xF1, 0xF5),
        surf_sel: rgb(0xE3, 0xE3, 0xEB),
        surf_border: rgb(0xD6, 0xD6, 0xDC),
        surf_sep: rgb(0xD0, 0xD0, 0xD6),
        on_surf: rgb(0x45, 0x45, 0x4C),
        on_surf_muted: rgb(0x8A, 0x8A, 0x92),
        prop_shadow: rgb(0xC6, 0xC6, 0xCE),
        panel_bg: rgb(0xFF, 0xFF, 0xFF),
        panel_border: rgb(0xD6, 0xD6, 0xDC),
        field_bg: rgb(0xF1, 0xF1, 0xF5),
        field_border: rgb(0xDD, 0xDD, 0xE3),
        field_hover: rgb(0xE7, 0xE7, 0xEE),
        icon_copy: rgb(0x00, 0x8F, 0xB3),
        icon_pin: rgb(0xD9, 0x7E, 0x00),
    };

    pub const DARK: Palette = Palette {
        // 带一点暖紫的近黑，纯 #000 会让玫红显得脏
        bg: rgb(0x1B, 0x1A, 0x21),
        divider: rgb(0x2E, 0x2D, 0x36),
        txt1: rgb(0xF2, 0xF0, 0xF4),
        txt2: rgb(0xA8, 0xA2, 0xAE),
        txt3: rgb(0x7C, 0x76, 0x84),
        accent: rgb(0xFF, 0x2E, 0x63),
        accent_label: rgb(0xFF, 0x7C, 0x9E),
        on_accent: rgb(0xFF, 0xFF, 0xFF),
        hero_bg: rgb(0xFF, 0x2E, 0x63),
        hero_hover: rgb(0xD9, 0x0A, 0x42),
        hero_txt: rgb(0xFF, 0xFF, 0xFF),
        hero_muted: rgb(0xFF, 0xE3, 0xEB),
        hero_accent: rgb(0xFF, 0xC3, 0x00),
        hero_off: rgb(0xC0, 0x6B, 0x86),
        key_bg: rgb(0x2A, 0x29, 0x33),
        key_bg_hover: rgb(0x3A, 0x2A, 0x34),
        key_border: rgb(0x3D, 0x3C, 0x47),
        key_border_hover: rgb(0xFF, 0x2E, 0x63),
        key_txt: rgb(0xF5, 0xF3, 0xF7),
        plus: rgb(0x7A, 0x5C, 0x68),
        hover_bg: rgb(0x2A, 0x29, 0x33),
        segment_bg: rgb(0x24, 0x23, 0x2B),
        surf: rgb(0x26, 0x25, 0x2D),
        surf_hover: rgb(0x34, 0x33, 0x3D),
        surf_sel: rgb(0x41, 0x40, 0x4C),
        surf_border: rgb(0x3A, 0x39, 0x44),
        surf_sep: rgb(0x3A, 0x39, 0x44),
        on_surf: rgb(0xE6, 0xE4, 0xEA),
        on_surf_muted: rgb(0x7A, 0x77, 0x84),
        prop_shadow: rgb(0x55, 0x53, 0x60),
        panel_bg: rgb(0x26, 0x25, 0x2D),
        panel_border: rgb(0x3A, 0x39, 0x44),
        field_bg: rgb(0x34, 0x33, 0x3D),
        field_border: rgb(0x43, 0x42, 0x4E),
        field_hover: rgb(0x41, 0x40, 0x4C),
        icon_copy: rgb(0x35, 0xB6, 0xDB),
        icon_pin: rgb(0xF2, 0xA3, 0x3C),
    };
}

const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

static mut MODE: Mode = Mode::System;
/// 系统当前是不是深色。None = 还没读过注册表
static mut SYS_DARK: Option<bool> = None;

pub fn set_mode(m: Mode) {
    unsafe {
        MODE = m;
    }
}

/// 当前实际生效的深浅。`Mode::System` 时才去问系统。
pub fn dark() -> bool {
    unsafe {
        match MODE {
            Mode::Light => false,
            Mode::Dark => true,
            Mode::System => {
                if SYS_DARK.is_none() {
                    SYS_DARK = Some(read_system_dark());
                }
                SYS_DARK.unwrap_or(false)
            }
        }
    }
}

/// 重新问一次系统。返回深浅有没有变化——变了才值得重绘。
pub fn refresh() -> bool {
    unsafe {
        let before = dark();
        SYS_DARK = Some(read_system_dark());
        dark() != before
    }
}

/// 当前色板。所有绘制代码通过它取色。
pub fn c() -> &'static Palette {
    if dark() {
        &Palette::DARK
    } else {
        &Palette::LIGHT
    }
}

fn read_system_dark() -> bool {
    let sub = crate::win::to_wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let name = crate::win::to_wide("AppsUseLightTheme");
    let mut light: u32 = 1;
    let mut size = std::mem::size_of::<u32>() as u32;
    let rc = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            sub.as_ptr(),
            name.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut light as *mut u32 as *mut core::ffi::c_void,
            &mut size,
        )
    };
    // 读不到（Win10 早期版本没这个键）就当浅色，跟系统默认一致
    rc == 0 && light == 0
}
