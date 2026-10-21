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

#[derive(Clone, Copy)]
pub struct Config {
    pub hotkey_mod: u32,
    pub hotkey_vk: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            hotkey_mod: DEFAULT_MOD,
            hotkey_vk: DEFAULT_VK,
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
        let text = format!(
            "; smartpaste 配置\nhotkey_mod={}\nhotkey_vk={}\n",
            self.hotkey_mod, self.hotkey_vk
        );
        std::fs::write(&p, text).is_ok()
    }

    pub fn hotkey_text(self) -> String {
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
