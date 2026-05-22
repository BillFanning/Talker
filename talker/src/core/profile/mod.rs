use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::core::channel::ChannelConfig;
use crate::core::logging::LoggingConfig;

/// Current profile schema version.
///
/// Schema v2 is the baseline established by the program specification v2.0.
/// The v1 schema predates it and is not supported; a v1 profile is rejected
/// at load time. When a breaking v3 change is introduced, add a migration
/// step and reinstate version-downgrade handling in [`Profile::load`].
pub const CURRENT_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    /// Schema version. A profile whose version differs from
    /// [`CURRENT_VERSION`] is rejected at load time.
    #[serde(default = "current_version")]
    pub version: u32,
    #[serde(default)]
    pub name: String,
    /// The channels defined by this profile. Each channel has one interface
    /// and its own list of messages.
    #[serde(default)]
    pub channels: Vec<ChannelConfig>,
    #[serde(default)]
    pub logging: LoggingConfig,
}

fn current_version() -> u32 {
    CURRENT_VERSION
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            name: String::new(),
            channels: vec![],
            logging: LoggingConfig::default(),
        }
    }
}

impl Profile {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Load a profile from a TOML file.
    ///
    /// A profile whose version is newer than [`CURRENT_VERSION`] is rejected,
    /// and so is an older (v1) profile — the v1 schema predates the current
    /// schema and is not migrated automatically.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading profile {:?}", path))?;

        // Parse to a raw Value first so the version can be inspected before
        // deserializing the typed struct.
        let doc: toml::Value =
            toml::from_str(&content).with_context(|| format!("parsing profile {:?}", path))?;

        let version = extract_version(&doc)?;
        if version > CURRENT_VERSION {
            anyhow::bail!(
                "profile version {version} is newer than this binary supports ({CURRENT_VERSION})"
            );
        }
        if version < CURRENT_VERSION {
            anyhow::bail!(
                "profile schema v{version} is not supported; it predates the current \
                 v{CURRENT_VERSION} schema — recreate the profile"
            );
        }

        let profile: Self = serde::Deserialize::deserialize(doc)
            .with_context(|| format!("deserializing profile {:?}", path))?;

        Ok(profile)
    }

    /// Serialize this profile to a TOML file, creating parent directories as
    /// needed.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating directory {:?}", parent))?;
        }
        let content = toml::to_string(self).context("serializing profile to TOML")?;
        std::fs::write(path, content).with_context(|| format!("writing profile {:?}", path))?;
        Ok(())
    }
}

/// The OS-specific directory where profiles are stored by default.
///
/// Returns `None` if the platform config directory cannot be determined.
pub fn default_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("talker").join("profiles"))
}

fn extract_version(doc: &toml::Value) -> anyhow::Result<u32> {
    match doc.get("version") {
        None => Ok(CURRENT_VERSION),
        Some(toml::Value::Integer(v)) => {
            u32::try_from(*v).context("profile version field is out of range")
        }
        Some(other) => {
            anyhow::bail!("profile version field has wrong type: {other:?}")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::*;
    use crate::core::channel::{ChannelConfig, InterfaceConfig, TcpClientConfig, UdpConfig};
    use crate::core::message::{MessageConfig, PayloadConfig};

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("talker_profile_test_{name}.toml"))
    }

    // ── defaults ─────────────────────────────────────────────────────────────

    #[test]
    fn default_profile_has_current_version() {
        assert_eq!(Profile::default().version, CURRENT_VERSION);
    }

    #[test]
    fn default_profile_has_no_channels() {
        assert!(Profile::default().channels.is_empty());
    }

    #[test]
    fn new_sets_name() {
        let p = Profile::new("my-profile");
        assert_eq!(p.name, "my-profile");
        assert_eq!(p.version, CURRENT_VERSION);
    }

    // ── save / load round-trip ────────────────────────────────────────────────

    #[test]
    fn round_trip_empty_profile() {
        let path = temp_path("empty");
        let original = Profile::new("empty");
        original.save(&path).unwrap();
        let loaded = Profile::load(&path).unwrap();
        assert_eq!(loaded.name, "empty");
        assert_eq!(loaded.version, CURRENT_VERSION);
        assert!(loaded.channels.is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn round_trip_with_channels() {
        let path = temp_path("full");
        let addr: SocketAddr = "10.0.0.1:5000".parse().unwrap();
        let mut profile = Profile::new("full");
        profile.channels.push(ChannelConfig::new(
            InterfaceConfig::TcpClient(TcpClientConfig::new(addr)),
            vec![MessageConfig::new(PayloadConfig::raw_hex("AABB"), 500)],
        ));
        profile.channels.push(ChannelConfig::new(
            InterfaceConfig::Udp(UdpConfig::unicast(addr)),
            vec![MessageConfig::new(
                PayloadConfig::nmea("GP", "GGA", vec![]),
                1000,
            )],
        ));

        profile.save(&path).unwrap();
        let loaded = Profile::load(&path).unwrap();

        assert_eq!(loaded.name, "full");
        assert_eq!(loaded.channels.len(), 2);
        assert_eq!(loaded.channels[0].messages.len(), 1);
        assert_eq!(loaded.channels[1].messages.len(), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn save_creates_parent_directories() {
        let path = std::env::temp_dir()
            .join("talker_profile_test_subdir")
            .join("nested")
            .join("profile.toml");
        Profile::new("nested").save(&path).unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    // ── version checks ────────────────────────────────────────────────────────

    #[test]
    fn load_rejects_future_version() {
        let path = temp_path("future");
        let content = format!("version = {}\nname = \"future\"\n", CURRENT_VERSION + 1);
        std::fs::write(&path, content).unwrap();
        let err = Profile::load(&path).unwrap_err();
        assert!(err.to_string().contains("newer than this binary supports"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_rejects_v1_profile() {
        let path = temp_path("v1");
        std::fs::write(&path, "version = 1\nname = \"old\"\n").unwrap();
        let err = Profile::load(&path).unwrap_err();
        assert!(err.to_string().contains("not supported"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_missing_version_defaults_to_current() {
        let path = temp_path("noversion");
        std::fs::write(&path, "name = \"no-version\"\n").unwrap();
        let loaded = Profile::load(&path).unwrap();
        assert_eq!(loaded.version, CURRENT_VERSION);
        assert_eq!(loaded.name, "no-version");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_nonexistent_file_returns_error() {
        let err = Profile::load(Path::new("/no/such/profile.toml")).unwrap_err();
        assert!(err.to_string().contains("reading profile"));
    }

    #[test]
    fn load_invalid_toml_returns_error() {
        let path = temp_path("bad_toml");
        std::fs::write(&path, "this is not toml ][").unwrap();
        let err = Profile::load(&path).unwrap_err();
        assert!(err.to_string().contains("parsing profile"));
        let _ = std::fs::remove_file(&path);
    }

    /// The sample profile shipped at the repository root must stay valid.
    #[test]
    fn sample_profile_toml_loads() {
        let path = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../profile.toml"));
        let profile = Profile::load(path).expect("repo profile.toml should load");
        assert_eq!(profile.version, CURRENT_VERSION);
        assert!(!profile.channels.is_empty());
    }

    // ── default_dir ───────────────────────────────────────────────────────────

    #[test]
    fn default_dir_ends_with_talker_profiles() {
        if let Some(dir) = default_dir() {
            assert!(dir.ends_with("talker/profiles") || dir.ends_with("talker\\profiles"));
        }
        // On platforms where config_dir() is None this test is a no-op.
    }
}
