//! 配置文件读写。
//!
//! 放在 `%APPDATA%\smartpaste\config.txt`，纯 key=value 文本，
//! 出问题能直接拿记事本改。目前只有热键一项，够用。
//!
//! 修饰键用 Win32 的位掩码：Alt=1、Ctrl=2、Shift=4、Win=8，
//! 跟 RegisterHotKey 的参数直接对应，读出来不用转换。

use std::path::PathBuf;

pub const MOD_ALT: u32 = 1;
pub const MOD_CONTROL: u32 = 2;
pub const MOD_SHIFT: u32 = 4;
pub const MOD_WIN: u32 = 8;

/// 默认 Ctrl + Shift + A
pub const DEFAULT_MOD: u32 = MOD_CONTROL | MOD_SHIFT;
pub const DEFAULT_VK: u32 = 0x41; // 'A'

#[derive(Clone)]
pub struct Config {
    pub hotkey_mod: u32,
    pub hotkey_vk: u32,
    /// 每个工具上次用过的样式，原样存字符串，解析交给 edit::Style
    pub styles: [Option<String>; 5],
    /// 上次"另存为"选的目录，下次直接打开这里
    pub save_dir: Option<String>,
    /// 录屏帧率，只允许 30 / 60
    pub record_fps: u32,
    /// 点录制后倒计时几秒，0 = 立即开始
    pub record_countdown: u32,
    /// 录像保存目录，空则用系统 Videos
    pub video_dir: Option<String>,
    /// 录完是否弹"另存为"。false = 直接落到 video_dir
    pub video_ask_save: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey_mod: DEFAULT_MOD,
            hotkey_vk: DEFAULT_VK,
            styles: [None, None, None, None, None],
            save_dir: None,
            record_fps: 60,
            record_countdown: 3,
            video_dir: None,
            video_ask_save: false,
        }
    }
}

impl Config {
    /// 录像目录。没配置就按 Videos → Desktop → 临时目录兜底。
    pub fn video_dir(&self) -> PathBuf {
        if let Some(d) = &self.video_dir {
            return PathBuf::from(d);
        }
        let Some(home) = std::env::var_os("USERPROFILE").map(PathBuf::from) else {
            return std::env::temp_dir();
        };
        let videos = home.join("Videos");
        if videos.is_dir() {
            videos
        } else {
            home.join("Desktop")
        }
    }
}

fn path() -> Option<PathBuf> {
    let appdata = std::env::var_os("APPDATA")?;
    Some(PathBuf::from(appdata).join("smartpaste").join("config.txt"))
}

impl Config {
    pub fn load() -> Self {
        let mut cfg = Self::default();
        let Some(p) = path() else { return cfg };
        let Ok(text) = std::fs::read_to_string(&p) else {
            return cfg;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            match k.trim() {
                "hotkey_mod" => {
                    if let Ok(n) = v.trim().parse::<u32>() {
                        // 只要一个修饰键都没有，热键会变成单键，太容易误触
                        if n & (MOD_ALT | MOD_CONTROL | MOD_SHIFT | MOD_WIN) != 0 {
                            cfg.hotkey_mod = n;
                        }
                    }
                }
                "hotkey_vk" => {
                    if let Ok(n) = v.trim().parse::<u32>() {
                        if n > 0 && n < 0x100 {
                            cfg.hotkey_vk = n;
                        }
                    }
                }
                "save_dir" => {
                    let v = v.trim();
                    if !v.is_empty() {
                        cfg.save_dir = Some(v.to_string());
                    }
                }
                "record_fps" => {
                    if let Ok(n) = v.trim().parse::<u32>() {
                        if n == 30 || n == 60 {
                            cfg.record_fps = n;
                        }
                    }
                }
                "record_countdown" => {
                    if let Ok(n) = v.trim().parse::<u32>() {
                        if n <= 10 {
                            cfg.record_countdown = n;
                        }
                    }
                }
                "video_dir" => {
                    let v = v.trim();
                    if !v.is_empty() {
                        cfg.video_dir = Some(v.to_string());
                    }
                }
                "video_ask_save" => {
                    let v = v.trim();
                    cfg.video_ask_save = v == "1" || v.eq_ignore_ascii_case("true");
                }
                k if k.starts_with("style.") => {
                    if let Ok(i) = k["style.".len()..].parse::<usize>() {
                        if i < 5 {
                            cfg.styles[i] = Some(v.trim().to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        cfg
    }

    pub fn save(&self) -> bool {
        let Some(p) = path() else { return false };
        if let Some(dir) = p.parent() {
            if std::fs::create_dir_all(dir).is_err() {
                return false;
            }
        }
        // style.0..4 对应 形状 / 箭头 / 画笔 / 文本 / 橡皮
        let mut text = format!(
            "; smartpaste 配置\nhotkey_mod={}\nhotkey_vk={}\n",
            self.hotkey_mod, self.hotkey_vk
        );
        if let Some(d) = &self.save_dir {
            text.push_str(&format!("save_dir={d}\n"));
        }
        text.push_str(&format!(
            "record_fps={}\nrecord_countdown={}\n",
            self.record_fps, self.record_countdown
        ));
        if let Some(d) = &self.video_dir {
            text.push_str(&format!("video_dir={d}\n"));
        }
        text.push_str(&format!(
            "video_ask_save={}\n",
            if self.video_ask_save { 1 } else { 0 }
        ));
        text.push_str("; 每个工具的样式：RRGGBB,线宽,标志位\n");
        for (i, s) in self.styles.iter().enumerate() {
            if let Some(v) = s {
                text.push_str(&format!("style.{i}={v}\n"));
            }
        }
        std::fs::write(&p, text).is_ok()
    }

    pub fn hotkey_text(&self) -> String {
        hotkey_text(self.hotkey_mod, self.hotkey_vk)
    }
}

/// 把修饰位 + 主键拆成 ["Ctrl", "Shift", "A"]。
/// 界面上每个部件都要画成独立的键帽，所以这里不拼字符串。
pub fn hotkey_parts(mods: u32, vk: u32) -> Vec<String> {
    let mut parts = Vec::new();
    if mods & MOD_CONTROL != 0 {
        parts.push("Ctrl".to_string());
    }
    if mods & MOD_SHIFT != 0 {
        parts.push("Shift".to_string());
    }
    if mods & MOD_ALT != 0 {
        parts.push("Alt".to_string());
    }
    if mods & MOD_WIN != 0 {
        parts.push("Win".to_string());
    }
    parts.push(key_name(vk));
    parts
}

/// 拼成 "Ctrl + Shift + A" 这种可读形式
pub fn hotkey_text(mods: u32, vk: u32) -> String {
    hotkey_parts(mods, vk).join(" + ")
}

fn key_name(vk: u32) -> String {
    match vk {
        0x30..=0x39 => ((b'0' + (vk - 0x30) as u8) as char).to_string(),
        0x41..=0x5A => ((b'A' + (vk - 0x41) as u8) as char).to_string(),
        0x70..=0x87 => format!("F{}", vk - 0x6F),
        0x20 => "Space".into(),
        0x09 => "Tab".into(),
        0x25..=0x28 => ["Left", "Up", "Right", "Down"][(vk - 0x25) as usize].into(),
        0x08 => "Backspace".into(),
        _ => format!("VK_{vk:02X}"),
    }
}
