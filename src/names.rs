//! Controller nicknames, stored per Bluetooth address.
//!
//! The address never changes for a physical controller, so a nickname keyed by it survives
//! Windows forgetting and re-pairing the controller (which this app does on every Connect).
//! Saved in %APPDATA%\SwitchControllerConnector\nicknames.tsv as `ADDRESS<TAB>nickname<TAB>model`.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

/// Windows stores Bluetooth device names in a 248-character buffer; keep nicknames well inside it.
pub const MAX_LEN: usize = 40;

#[derive(Clone, Debug)]
pub struct Nick {
    pub nickname: String,
    /// The controller's own name, e.g. "Joy-Con (L)" or "Pro Controller".
    pub model: String,
}

pub struct Names {
    path: Option<PathBuf>,
    map: HashMap<String, Nick>,
}

fn key(address: &str) -> String {
    address.trim().to_ascii_uppercase()
}

fn clean(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    s.trim().chars().take(MAX_LEN).collect()
}

fn store_path() -> Option<PathBuf> {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).or_else(|| {
        std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf()))
    })?;
    Some(base.join("SwitchControllerConnector").join("nicknames.tsv"))
}

impl Names {
    pub fn load() -> Self {
        let path = store_path();
        let mut map = HashMap::new();
        if let Some(text) = path.as_ref().and_then(|p| fs::read_to_string(p).ok()) {
            for line in text.lines() {
                let mut parts = line.split('\t');
                let (Some(addr), Some(nick)) = (parts.next(), parts.next()) else { continue };
                let model = parts.next().unwrap_or("").to_string();
                let nick = clean(nick);
                if !addr.trim().is_empty() && !nick.is_empty() {
                    map.insert(key(addr), Nick { nickname: nick, model });
                }
            }
        }
        Names { path, map }
    }

    pub fn get(&self, address: &str) -> Option<&Nick> {
        if address.is_empty() {
            return None;
        }
        self.map.get(&key(address))
    }

    /// Sets (or, with an empty name, removes) a nickname. Returns the cleaned nickname.
    pub fn set(&mut self, address: &str, nickname: &str, model: &str) -> Result<Option<String>, String> {
        let nick = clean(nickname);
        if nick.is_empty() {
            self.map.remove(&key(address));
            self.save()?;
            return Ok(None);
        }
        self.map.insert(key(address), Nick { nickname: nick.clone(), model: model.to_string() });
        self.save()?;
        Ok(Some(nick))
    }

    /// Remember the controller's real model name if we learn it later.
    pub fn note_model(&mut self, address: &str, model: &str) {
        let changed = match self.map.get_mut(&key(address)) {
            Some(n) if n.model != model && !model.is_empty() => {
                n.model = model.to_string();
                true
            }
            _ => false,
        };
        if changed {
            let _ = self.save();
        }
    }

    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.path else {
            return Err("no place to save nicknames".into());
        };
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut rows: Vec<_> = self.map.iter().collect();
        rows.sort_by(|a, b| a.0.cmp(b.0));
        let mut out = String::new();
        for (addr, n) in rows {
            out.push_str(&format!("{addr}\t{}\t{}\r\n", n.nickname, clean(&n.model)));
        }
        fs::write(path, out).map_err(|e| e.to_string())
    }
}
