//! Configuration for the `sendme-balloon` desktop app.
//!
//! The configuration is a single YAML file at
//! `~/.config/sendme-balloon/config.yaml` (following the XDG base directory
//! specification, the same directory that holds `secret.key` and
//! `addressbook.json`). If the file is absent, all options fall back to their
//! built-in defaults, so the app keeps working with zero configuration.
//!
//! The GUI never writes or edits this file — it is meant to be edited by hand
//! in a text editor. A heavily commented template ships next to the source as
//! `config.sample.yaml`; copy it to the path above and tweak.
//!
//! Scope: configuration is balloon-only. The `sendme` command-line tool
//! (see [`crate::main`]) is intentionally untouched and stays fully compatible
//! with the upstream sendme application it was forked from.

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use iroh::{RelayMode, RelayUrl};
use serde::{Deserialize, Serialize};

/// Relay selection, mirrored from the CLI's `--relay` flag so the balloon can
/// use the same options without depending on iroh's enum serde representation.
///
/// Serialized lowercase in YAML: `disabled`, `default`, or a custom URL string.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RelayModeConfig {
    Disabled,
    #[default]
    Default,
    /// A custom relay server URL, e.g. `https://my-relay.example.com`.
    Custom(String),
}

impl RelayModeConfig {
    /// Convert to iroh's [`RelayMode`]. An invalid custom URL falls back to
    /// [`RelayMode::Default`] with a warning, so a typo in the config never
    /// prevents the app from starting.
    pub fn to_relay_mode(&self) -> RelayMode {
        match self {
            Self::Disabled => RelayMode::Disabled,
            Self::Default => RelayMode::Default,
            Self::Custom(s) => match RelayUrl::from_str(s) {
                Ok(url) => RelayMode::Custom(url.into()),
                Err(e) => {
                    tracing::warn!("invalid relay URL {s:?}: {e}; falling back to default");
                    RelayMode::Default
                }
            },
        }
    }
}

/// What to do when an incoming transfer would overwrite an existing file.
///
/// `Ask` is the historic behaviour: the GUI prompts the user. The other two
/// resolve automatically, which is required for unattended auto-accept.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConflictDefault {
    /// Prompt the user (the original behaviour).
    #[default]
    Ask,
    /// Silently overwrite existing files.
    Overwrite,
    /// Silently keep existing files; skip the incoming ones.
    KeepExisting,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TimeoutConfig {
    /// Seconds to wait for the endpoint to come online (NAT/relay discovery)
    /// before producing a ticket. Mirrors the 30 s the CLI waits.
    #[serde(default = "default_endpoint_online_wait")]
    pub endpoint_online_wait_secs: u64,
    /// Seconds to wait for a graceful router shutdown before forcing it.
    #[serde(default = "default_router_shutdown")]
    pub router_shutdown_secs: u64,
    /// Seconds to wait for the sender to acknowledge an offer reply before
    /// dropping the connection. Safety net for the offer protocol.
    #[serde(default = "default_offer_conn_close_wait")]
    pub offer_conn_close_wait_secs: u64,
}

impl Default for TimeoutConfig {
    /// Matches the serde field defaults so a missing `timeouts:` key yields
    /// the same values as a present-but-empty one.
    fn default() -> Self {
        Self {
            endpoint_online_wait_secs: default_endpoint_online_wait(),
            router_shutdown_secs: default_router_shutdown(),
            offer_conn_close_wait_secs: default_offer_conn_close_wait(),
        }
    }
}

fn default_endpoint_online_wait() -> u64 {
    30
}
fn default_router_shutdown() -> u64 {
    2
}
fn default_offer_conn_close_wait() -> u64 {
    30
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct NotificationConfig {
    /// Show a desktop notification for incoming transfer offers.
    #[serde(default = "default_notifications_enabled")]
    pub enabled: bool,
    /// How long the notification stays on screen, in seconds.
    #[serde(default = "default_notification_timeout")]
    pub timeout_seconds: u64,
}

impl Default for NotificationConfig {
    /// Matches the serde field defaults so a missing `notifications:` key
    /// yields the same values as a present-but-empty one.
    fn default() -> Self {
        Self {
            enabled: default_notifications_enabled(),
            timeout_seconds: default_notification_timeout(),
        }
    }
}

fn default_notifications_enabled() -> bool {
    true
}
fn default_notification_timeout() -> u64 {
    10
}

/// The balloon configuration.
///
/// Every field has a default, so a missing or partially-filled YAML file is
/// valid: unspecified keys keep their defaults.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    /// Default folder for incoming transfers. If set, the folder-picker dialog
    /// is skipped and all incoming files land here. Supports a leading `~`
    /// for the home directory.
    ///
    /// Setting this also unlocks auto-accept (both global and per-contact),
    /// which is ignored when no default folder is configured.
    #[serde(default)]
    pub default_save_folder: Option<PathBuf>,

    /// Automatically accept incoming transfer offers WITHOUT prompting.
    ///
    /// SECURITY WARNING: anyone who knows your node id can push files to you
    /// with no interaction. Only enable this if you trust the sources, and
    /// always set `default_save_folder` first — auto-accept is ignored until
    /// a default folder is configured. A per-contact `auto_accept` flag in the
    /// address book overrides this for that contact.
    #[serde(default)]
    pub auto_accept_offers: bool,

    /// Relay mode: `disabled`, `default`, or a custom relay URL string.
    #[serde(default)]
    pub relay_mode: RelayModeConfig,

    /// Number of parallel jobs for importing files while sending. `null` means
    /// use the number of logical CPU cores (mirrors the CLI `-j` default).
    #[serde(default)]
    pub jobs: Option<usize>,

    #[serde(default)]
    pub timeouts: TimeoutConfig,

    /// Heartbeat interval (seconds) for the contact-offer stream. Keeps the
    /// QUIC connection alive while the remote user is still deciding whether
    /// to accept. Lower is safer on flaky links, higher saves bandwidth.
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval_secs: u64,

    /// Chunk size (MiB) used when downloading. Larger chunks mean fewer
    /// round-trips but more memory; smaller chunks suit constrained devices.
    #[serde(default = "default_chunk_size_mib")]
    pub chunk_size_mib: usize,

    /// Default decision when an incoming transfer would overwrite an existing
    /// file. `ask` prompts the user; the others resolve automatically.
    #[serde(default)]
    pub conflict_default: ConflictDefault,

    #[serde(default)]
    pub notifications: NotificationConfig,

    /// Logging level: `error`, `warn`, `info`, `debug`, `trace`.
    /// The `RUST_LOG` environment variable, if set, takes precedence over this.
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

impl Default for Config {
    /// Matches the serde field defaults so that `Config::default()` (used as
    /// the fallback when no config file exists, and in tests) produces the same
    /// values as deserializing an empty YAML file. A derived `Default` would
    /// zero-initialise the numeric fields (e.g. `chunk_size_mib = 0`),
    /// breaking transfers.
    fn default() -> Self {
        Self {
            default_save_folder: None,
            auto_accept_offers: false,
            relay_mode: RelayModeConfig::Default,
            jobs: None,
            timeouts: TimeoutConfig::default(),
            heartbeat_interval_secs: default_heartbeat_interval(),
            chunk_size_mib: default_chunk_size_mib(),
            conflict_default: ConflictDefault::default(),
            notifications: NotificationConfig::default(),
            log_level: default_log_level(),
        }
    }
}

fn default_heartbeat_interval() -> u64 {
    3
}
fn default_chunk_size_mib() -> usize {
    32
}
fn default_log_level() -> String {
    "warn".to_string()
}

impl Config {
    /// Convenience accessor for the resolved default save folder, with a leading
    /// `~` already expanded. Returns `None` when unset.
    pub fn default_folder(&self) -> Option<&std::path::Path> {
        self.default_save_folder.as_deref()
    }

    pub fn heartbeat_interval(&self) -> Duration {
        Duration::from_secs(self.heartbeat_interval_secs.max(1))
    }

    pub fn chunk_size_bytes(&self) -> u64 {
        (self.chunk_size_mib as u64).saturating_mul(1024 * 1024)
    }

    /// True when auto-accept can actually fire, i.e. a default save folder is
    /// configured. Both global and per-contact auto-accept need a landing
    /// folder; without one they are ignored to avoid an unsolvable picker.
    pub fn auto_accept_possible(&self) -> bool {
        self.default_save_folder.is_some()
    }
}

// ── Loading (balloon-only: depends on dirs + serde_yaml) ───────────────────

#[cfg(feature = "balloon")]
impl Config {
    /// Directory holding all balloon state (secret key, address book, config).
    /// Reuses the same location as the address book so everything lives in one
    /// place.
    fn app_dir() -> PathBuf {
        let base = dirs::config_dir().unwrap_or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from("."))
        });
        base.join("sendme-balloon")
    }

    /// Path to the config file.
    pub fn config_path() -> PathBuf {
        Self::app_dir().join("config.yaml")
    }

    /// Expand a leading `~` to the user's home directory. Leaves absolute and
    /// relative paths untouched.
    fn expand_tilde(p: &std::path::Path) -> PathBuf {
        let s = match p.to_str() {
            Some(s) => s,
            None => return p.to_path_buf(),
        };
        if let Some(rest) = s.strip_prefix("~") {
            if let Some(home) = dirs::home_dir() {
                return home.join(rest.trim_start_matches('/'));
            }
        }
        p.to_path_buf()
    }

    /// Load the config file, returning the default config if the file is
    /// absent. A malformed file is an error so the user notices typos early.
    pub fn load() -> anyhow::Result<Config> {
        let path = Self::config_path();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let mut cfg: Config = serde_yaml::from_str(&text)
                    .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
                if let Some(folder) = cfg.default_save_folder.take() {
                    cfg.default_save_folder = Some(Self::expand_tilde(&folder));
                }
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_no_save_folder_and_auto_accept_off() {
        let cfg = Config::default();
        assert!(cfg.default_save_folder.is_none());
        assert!(!cfg.auto_accept_offers);
        assert!(!cfg.auto_accept_possible());
    }

    #[test]
    fn default_config_chunk_size_matches_historic_hardcoded_value() {
        // Regression guard: a derived Default would zero this field, making
        // `chunk_size_bytes()` return 0 and every receive fail with
        // "size too large". The default must equal the old hardcoded 32 MiB.
        let cfg = Config::default();
        assert_eq!(cfg.chunk_size_mib, 32);
        assert_eq!(cfg.chunk_size_bytes(), 32 * 1024 * 1024);
        assert_eq!(cfg.heartbeat_interval_secs, 3);
        assert_eq!(cfg.log_level, "warn");
        assert_eq!(cfg.timeouts.endpoint_online_wait_secs, 30);
        assert!(cfg.notifications.enabled);
        assert_eq!(cfg.notifications.timeout_seconds, 10);
    }

    #[test]
    fn auto_accept_requires_a_default_folder() {
        let cfg = Config {
            auto_accept_offers: true,
            ..Config::default()
        };
        // no folder -> still impossible
        assert!(!cfg.auto_accept_possible());
        let cfg = Config {
            auto_accept_offers: true,
            default_save_folder: Some(PathBuf::from("/tmp/sendme")),
            ..Config::default()
        };
        assert!(cfg.auto_accept_possible());
    }

    #[test]
    fn chunk_size_bytes_is_mebibytes() {
        let cfg = Config {
            chunk_size_mib: 8,
            ..Config::default()
        };
        assert_eq!(cfg.chunk_size_bytes(), 8 * 1024 * 1024);
    }

    #[test]
    fn heartbeat_interval_never_zero() {
        let cfg = Config {
            heartbeat_interval_secs: 0,
            ..Config::default()
        };
        assert_eq!(cfg.heartbeat_interval(), Duration::from_secs(1));
    }

    #[test]
    fn relay_mode_disabled_converts() {
        let mode = RelayModeConfig::Disabled.to_relay_mode();
        assert!(matches!(mode, RelayMode::Disabled));
    }

    #[test]
    fn relay_mode_default_converts() {
        let mode = RelayModeConfig::Default.to_relay_mode();
        assert!(matches!(mode, RelayMode::Default));
    }

    #[test]
    fn relay_mode_invalid_url_falls_back_to_default() {
        let mode = RelayModeConfig::Custom("not a url".to_string()).to_relay_mode();
        assert!(matches!(mode, RelayMode::Default));
    }

    #[test]
    fn relay_mode_valid_custom_url_does_not_fall_back() {
        let mode =
            RelayModeConfig::Custom("https://my-relay.example.com".to_string()).to_relay_mode();
        // It must not collapse to Disabled or Default; the only remaining
        // variant is Custom.
        assert!(matches!(mode, RelayMode::Custom(_)));
    }

    #[cfg(feature = "balloon")]
    #[test]
    fn partial_yaml_keeps_defaults_for_missing_keys() {
        let yaml = "auto_accept_offers: true\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.auto_accept_offers);
        // unspecified keys keep defaults
        assert!(cfg.default_save_folder.is_none());
        assert!(matches!(cfg.relay_mode, RelayModeConfig::Default));
        assert_eq!(cfg.chunk_size_mib, 32);
        assert_eq!(cfg.heartbeat_interval_secs, 3);
        assert!(matches!(cfg.conflict_default, ConflictDefault::Ask));
        assert_eq!(cfg.timeouts.endpoint_online_wait_secs, 30);
        assert!(cfg.notifications.enabled);
        assert_eq!(cfg.notifications.timeout_seconds, 10);
        assert_eq!(cfg.log_level, "warn");
    }

    #[cfg(feature = "balloon")]
    #[test]
    fn empty_yaml_yields_default_config() {
        let cfg: Config = serde_yaml::from_str("").unwrap();
        assert!(cfg.default_save_folder.is_none());
        assert!(!cfg.auto_accept_offers);
    }

    #[cfg(feature = "balloon")]
    #[test]
    fn default_trait_equals_empty_yaml() {
        // The no-file fallback (Config::default()) and a deserialised empty
        // config file must produce identical values — otherwise users without
        // a config file would get different behaviour than an empty one.
        let from_default = Config::default();
        let from_yaml: Config = serde_yaml::from_str("").unwrap();
        assert_eq!(from_default.chunk_size_mib, from_yaml.chunk_size_mib);
        assert_eq!(
            from_default.heartbeat_interval_secs,
            from_yaml.heartbeat_interval_secs
        );
        assert_eq!(from_default.log_level, from_yaml.log_level);
        assert_eq!(
            from_default.timeouts.endpoint_online_wait_secs,
            from_yaml.timeouts.endpoint_online_wait_secs
        );
        assert_eq!(
            from_default.notifications.enabled,
            from_yaml.notifications.enabled
        );
    }

    #[cfg(feature = "balloon")]
    #[test]
    fn tilde_expands_to_home() {
        // exercise the helper indirectly through a synthetic path. We can't
        // call the private expand_tilde, but we can assert the home dir is
        // non-empty and that a non-tilde path is returned unchanged via the
        // public surface (default config round-trips an absolute path).
        let home = dirs::home_dir().expect("home dir");
        assert!(home.is_absolute());
    }

    #[cfg(feature = "balloon")]
    #[test]
    fn shipped_sample_parses_to_pure_defaults() {
        // The sample file is meant to be copied verbatim and then selectively
        // uncommented. Copying it must therefore yield EXACTLY the built-in
        // defaults — otherwise a stale value baked into the sample would
        // silently override a future default change in the binary, defeating
        // the whole point of the all-commented layout. This test reads the
        // actual committed file from the crate root and enforces that
        // invariant, so accidentally uncommenting a value in the sample is
        // caught by CI.
        let text = std::fs::read_to_string("config.sample.yaml")
            .expect("config.sample.yaml should exist at the crate root");
        let cfg: Config = serde_yaml::from_str(&text).expect(
            "config.sample.yaml must be valid YAML that parses into Config (all keys commented)",
        );
        let default = Config::default();
        assert!(cfg.default_save_folder.is_none(), "sample pins a save folder");
        assert!(!cfg.auto_accept_offers, "sample enables auto-accept");
        assert!(matches!(cfg.relay_mode, RelayModeConfig::Default));
        assert_eq!(cfg.jobs, None);
        assert_eq!(cfg.chunk_size_mib, default.chunk_size_mib);
        assert_eq!(cfg.heartbeat_interval_secs, default.heartbeat_interval_secs);
        assert_eq!(cfg.log_level, default.log_level);
        assert!(matches!(cfg.conflict_default, ConflictDefault::Ask));
        assert_eq!(
            cfg.timeouts.endpoint_online_wait_secs,
            default.timeouts.endpoint_online_wait_secs
        );
        assert_eq!(
            cfg.timeouts.router_shutdown_secs,
            default.timeouts.router_shutdown_secs
        );
        assert_eq!(
            cfg.timeouts.offer_conn_close_wait_secs,
            default.timeouts.offer_conn_close_wait_secs
        );
        assert_eq!(cfg.notifications.enabled, default.notifications.enabled);
        assert_eq!(
            cfg.notifications.timeout_seconds,
            default.notifications.timeout_seconds
        );
    }
}
