//! Hub settings, read from `SEEK_HUB_*` environment variables.

use std::net::SocketAddr;
use std::path::PathBuf;

use seek_sync::transport::HubAddr;
use serde::{Deserialize, Serialize};

/// Port the hub listens on by default.
pub const DEFAULT_PORT: u16 = 47321;

#[derive(Clone, Debug)]
pub struct HubConfig {
    pub name: String,
    pub listen: Vec<SocketAddr>,
    /// Address the PCs are told to dial (shown in pairing info), if different from `listen`.
    pub public: Option<HubAddr>,
    pub data_dir: PathBuf,
    pub keys_dir: PathBuf,
    pub pc_quota_bytes: u64,
    pub pc_max_devices: usize,
}

/// What `seek-hub init` saves in `hub.json`. An environment variable that is set wins over its setting.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    pub name: Option<String>,
    pub pc_quota_mb: Option<u64>,
    pub pc_max_devices: Option<usize>,
    pub lan: Option<bool>,
    pub public: Option<String>,
}

/// The variables a [`Settings`] can stand in for.
pub const SETTING_VARS: [&str; 5] =
    ["SEEK_HUB_NAME", "SEEK_HUB_PC_QUOTA_MB", "SEEK_HUB_PC_MAX_DEVICES", "SEEK_HUB_LAN", "SEEK_HUB_PUBLIC"];

impl Settings {
    /// The setting behind environment variable `var`, spelled as the variable would be.
    pub fn var(&self, var: &str) -> Option<String> {
        match var {
            "SEEK_HUB_NAME" => self.name.clone(),
            "SEEK_HUB_PC_QUOTA_MB" => self.pc_quota_mb.map(|v| v.to_string()),
            "SEEK_HUB_PC_MAX_DEVICES" => self.pc_max_devices.map(|v| v.to_string()),
            "SEEK_HUB_LAN" => self.lan.map(|on| if on { "1" } else { "0" }.to_string()),
            "SEEK_HUB_PUBLIC" => self.public.clone(),
            _ => None,
        }
    }
}

impl HubConfig {
    /// [`HubConfig::from_env`], with `settings` filling in what the environment leaves unset (or empty).
    pub fn with_settings(get: impl Fn(&str) -> Option<String>, settings: &Settings) -> Result<HubConfig, String> {
        HubConfig::from_env(|k| get(k).filter(|v| !v.trim().is_empty()).or_else(|| settings.var(k)))
    }

    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Result<HubConfig, String> {
        let var = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let name = var("SEEK_HUB_NAME").unwrap_or_else(|| "Seek Hub".into());

        let listen_raw = var("SEEK_HUB_LISTEN").unwrap_or_else(|| format!("127.0.0.1:{DEFAULT_PORT}"));
        let mut listen = Vec::new();
        for part in listen_raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            let a: SocketAddr =
                part.parse().map_err(|e| format!("SEEK_HUB_LISTEN: bad address {part:?}: {e}"))?;
            listen.push(a);
        }
        if listen.is_empty() {
            return Err("SEEK_HUB_LISTEN: no address".into());
        }
        if var("SEEK_HUB_LAN").as_deref() == Some("1") {
            let lan = SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT));
            if !listen.contains(&lan) {
                listen.push(lan);
            }
        }

        let public = match var("SEEK_HUB_PUBLIC") {
            Some(s) => Some(HubAddr::parse(&s).map_err(|e| format!("SEEK_HUB_PUBLIC: {e}"))?),
            None => None,
        };
        let data_dir = PathBuf::from(var("SEEK_HUB_DATA").unwrap_or_else(|| "/data".into()));
        let keys_dir = PathBuf::from(var("SEEK_HUB_KEYS").unwrap_or_else(|| "/keys".into()));

        let quota_mb: u64 = match var("SEEK_HUB_PC_QUOTA_MB") {
            Some(s) => s.parse().map_err(|_| format!("SEEK_HUB_PC_QUOTA_MB: not a number: {s:?}"))?,
            None => 2048,
        };
        let pc_max_devices: usize = match var("SEEK_HUB_PC_MAX_DEVICES") {
            Some(s) => s.parse().map_err(|_| format!("SEEK_HUB_PC_MAX_DEVICES: not a number: {s:?}"))?,
            None => 64,
        };
        if quota_mb == 0 {
            return Err("SEEK_HUB_PC_QUOTA_MB: must be at least 1".into());
        }
        if pc_max_devices == 0 {
            return Err("SEEK_HUB_PC_MAX_DEVICES: must be at least 1".into());
        }

        Ok(HubConfig {
            name,
            listen,
            public,
            data_dir,
            keys_dir,
            pc_quota_bytes: quota_mb.saturating_mul(1024 * 1024),
            pc_max_devices,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn config_defaults_and_overrides() {
        let c = HubConfig::from_env(env(&[])).unwrap();
        assert_eq!(c.name, "Seek Hub");
        assert_eq!(c.listen, vec!["127.0.0.1:47321".parse().unwrap()]);
        assert!(c.public.is_none());
        assert_eq!(c.data_dir, std::path::PathBuf::from("/data"));
        assert_eq!(c.keys_dir, std::path::PathBuf::from("/keys"));
        assert_eq!(c.pc_quota_bytes, 2048 * 1024 * 1024);
        assert_eq!(c.pc_max_devices, 64);

        let c = HubConfig::from_env(env(&[
            ("SEEK_HUB_NAME", "Pi"),
            ("SEEK_HUB_LAN", "1"),
            ("SEEK_HUB_PUBLIC", "tls://seek-hub.tail1234.ts.net"),
            ("SEEK_HUB_DATA", "/srv/d"),
            ("SEEK_HUB_KEYS", "/srv/k"),
            ("SEEK_HUB_PC_QUOTA_MB", "10"),
            ("SEEK_HUB_PC_MAX_DEVICES", "3"),
        ]))
        .unwrap();
        assert_eq!(c.name, "Pi");
        assert_eq!(
            c.listen,
            vec!["127.0.0.1:47321".parse().unwrap(), "0.0.0.0:47321".parse().unwrap()]
        );
        assert!(c.public.is_some());
        assert_eq!(c.pc_quota_bytes, 10 * 1024 * 1024);
        assert_eq!(c.pc_max_devices, 3);

        assert!(HubConfig::from_env(env(&[("SEEK_HUB_LISTEN", "nope")])).is_err());
        assert!(HubConfig::from_env(env(&[("SEEK_HUB_PUBLIC", "")])).is_ok());
        assert!(HubConfig::from_env(env(&[("SEEK_HUB_PC_QUOTA_MB", "x")])).is_err());
    }

    #[test]
    fn env_wins_over_settings() {
        let s = Settings {
            name: Some("Pi".into()),
            pc_quota_mb: Some(10),
            pc_max_devices: Some(3),
            lan: Some(true),
            public: Some("tls://seek-hub.tail1234.ts.net".into()),
        };
        let c = HubConfig::with_settings(env(&[]), &s).unwrap();
        assert_eq!((c.name.as_str(), c.pc_quota_bytes, c.pc_max_devices), ("Pi", 10 * 1024 * 1024, 3));
        assert_eq!(c.listen.len(), 2);
        assert!(c.public.is_some());

        let c = HubConfig::with_settings(
            env(&[("SEEK_HUB_NAME", "Env"), ("SEEK_HUB_LAN", "0"), ("SEEK_HUB_PC_QUOTA_MB", " ")]),
            &s,
        )
        .unwrap();
        assert_eq!((c.name.as_str(), c.listen.len(), c.pc_quota_bytes), ("Env", 1, 10 * 1024 * 1024));

        let none = HubConfig::with_settings(env(&[]), &Settings::default()).unwrap();
        assert_eq!(none.name, "Seek Hub");
        // old hub.json without settings, and partial settings, still parse
        let s: Settings = serde_json::from_str(r#"{"lan":false}"#).unwrap();
        assert_eq!(s, Settings { lan: Some(false), ..Settings::default() });
    }

    #[test]
    fn zero_limits_rejected() {
        assert!(HubConfig::from_env(env(&[("SEEK_HUB_PC_QUOTA_MB", "0")])).is_err());
        assert!(HubConfig::from_env(env(&[("SEEK_HUB_PC_MAX_DEVICES", "0")])).is_err());
    }
}
