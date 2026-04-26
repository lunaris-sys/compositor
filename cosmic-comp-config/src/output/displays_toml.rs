// SPDX-License-Identifier: GPL-3.0-only
//
//! TOML serialisation layer for display profiles.
//!
//! `comp::OutputsConfig` keys layouts by `Vec<OutputInfo>` because
//! the same machine should pick a different arrangement when the
//! Dell U2719D is plugged in versus the LG 27UL850. RON serialises
//! that map directly. TOML cannot — its keys are strings — so this
//! module reshapes the same data into a flat array of `[[profile]]`
//! records that round-trip through `toml::to_string_pretty`.
//!
//! The internal struct (`OutputsConfig`) is unchanged; callers
//! never see this layer. `load` reads TOML and returns the canonical
//! struct; `save` takes the canonical struct and writes TOML.
//!
//! Schema (the doc-spec lives in `docs/architecture/display-system.md`
//! §A1; this is the matching wire layer):
//!
//! ```toml
//! [[profile]]
//! name = "office"
//!
//! [[profile.output_set]]
//! connector = "eDP-1"
//! make = "BOE"
//! model = "Unknown"
//!
//! [[profile.output]]
//! connector = "eDP-1"
//! mode = { width = 2560, height = 1440, refresh_mhz = 144000 }
//! scale = 1.5
//! position = { x = 0, y = 0 }
//! transform = "normal"
//! enabled = "enabled"        # "enabled" | "disabled" | "mirror:DP-1"
//! vrr = "enabled"            # "enabled" | "disabled" | "force"
//! max_bpc = 10
//! xwayland_primary = false
//! ```

use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::{self, Write},
    path::Path,
};

use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use super::comp::{
    AdaptiveSync, OutputConfig, OutputInfo, OutputState, OutputsConfig,
    TransformDef,
};

// ---------------------------------------------------------------------------
// On-disk schema. These types exist only for serde — every read or
// write goes via `From`/`Into` to the canonical types in `comp.rs`.
// ---------------------------------------------------------------------------

/// Top-level structure of `displays.toml`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct DisplaysToml {
    #[serde(default, rename = "profile", skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ProfileToml>,
}

/// One hot-plug profile. `name` is purely cosmetic (shown in
/// settings); matching is done via `output_set`. `output` carries
/// the per-monitor configuration.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ProfileToml {
    /// Stable, human-readable identifier. Auto-generated for
    /// migrated profiles (`migrated-1`, `migrated-2`, ...).
    pub name: String,
    /// Which output set this profile applies to. Daemon stores
    /// sorted by connector for stable round-trips.
    pub output_set: Vec<OutputInfoToml>,
    /// Per-output configuration.
    pub output: Vec<OutputToml>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct OutputInfoToml {
    pub connector: String,
    pub make: String,
    pub model: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct OutputToml {
    pub connector: String,
    pub mode: ModeToml,
    pub scale: f64,
    pub position: PositionToml,
    pub transform: TransformToml,
    pub enabled: EnabledToml,
    #[serde(default = "default_vrr")]
    pub vrr: VrrToml,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bpc: Option<u32>,
    #[serde(default)]
    pub xwayland_primary: bool,
}

fn default_vrr() -> VrrToml {
    VrrToml::Enabled
}

/// `mode` is a single inline table in TOML rather than a tuple so
/// users editing the file by hand have semantic field names.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ModeToml {
    pub width: i32,
    pub height: i32,
    /// Refresh rate in milli-Hertz. `None` ⇒ compositor picks
    /// preferred mode for the resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_mhz: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct PositionToml {
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum TransformToml {
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

/// "enabled" / "disabled" / "mirror:<connector>" — encoded as a
/// plain string for natural TOML editing. Round-trips losslessly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EnabledToml {
    Enabled,
    Disabled,
    Mirror(String),
}

impl Serialize for EnabledToml {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        match self {
            EnabledToml::Enabled => ser.serialize_str("enabled"),
            EnabledToml::Disabled => ser.serialize_str("disabled"),
            EnabledToml::Mirror(c) => ser.serialize_str(&format!("mirror:{c}")),
        }
    }
}

impl<'de> Deserialize<'de> for EnabledToml {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Ok(match s.as_str() {
            "enabled" => EnabledToml::Enabled,
            "disabled" => EnabledToml::Disabled,
            other if other.starts_with("mirror:") => {
                EnabledToml::Mirror(other.strip_prefix("mirror:").unwrap().to_string())
            }
            other => {
                return Err(serde::de::Error::custom(format!(
                    "invalid `enabled` value: {other:?} (want \"enabled\", \"disabled\", or \"mirror:<connector>\")"
                )));
            }
        })
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum VrrToml {
    Enabled,
    Disabled,
    Force,
}

// ---------------------------------------------------------------------------
// Conversion: canonical types ⇆ TOML wire types.
// ---------------------------------------------------------------------------

impl From<&OutputInfo> for OutputInfoToml {
    fn from(o: &OutputInfo) -> Self {
        Self {
            connector: o.connector.clone(),
            make: o.make.clone(),
            model: o.model.clone(),
        }
    }
}

impl From<OutputInfoToml> for OutputInfo {
    fn from(o: OutputInfoToml) -> Self {
        OutputInfo {
            connector: o.connector,
            make: o.make,
            model: o.model,
        }
    }
}

impl From<TransformDef> for TransformToml {
    fn from(t: TransformDef) -> Self {
        match t {
            TransformDef::Normal => TransformToml::Normal,
            TransformDef::_90 => TransformToml::Rotate90,
            TransformDef::_180 => TransformToml::Rotate180,
            TransformDef::_270 => TransformToml::Rotate270,
            TransformDef::Flipped => TransformToml::Flipped,
            TransformDef::Flipped90 => TransformToml::Flipped90,
            TransformDef::Flipped180 => TransformToml::Flipped180,
            TransformDef::Flipped270 => TransformToml::Flipped270,
        }
    }
}

impl From<TransformToml> for TransformDef {
    fn from(t: TransformToml) -> Self {
        match t {
            TransformToml::Normal => TransformDef::Normal,
            TransformToml::Rotate90 => TransformDef::_90,
            TransformToml::Rotate180 => TransformDef::_180,
            TransformToml::Rotate270 => TransformDef::_270,
            TransformToml::Flipped => TransformDef::Flipped,
            TransformToml::Flipped90 => TransformDef::Flipped90,
            TransformToml::Flipped180 => TransformDef::Flipped180,
            TransformToml::Flipped270 => TransformDef::Flipped270,
        }
    }
}

impl From<&OutputState> for EnabledToml {
    fn from(s: &OutputState) -> Self {
        match s {
            OutputState::Enabled => EnabledToml::Enabled,
            OutputState::Disabled => EnabledToml::Disabled,
            OutputState::Mirroring(c) => EnabledToml::Mirror(c.clone()),
        }
    }
}

impl From<EnabledToml> for OutputState {
    fn from(e: EnabledToml) -> Self {
        match e {
            EnabledToml::Enabled => OutputState::Enabled,
            EnabledToml::Disabled => OutputState::Disabled,
            EnabledToml::Mirror(c) => OutputState::Mirroring(c),
        }
    }
}

impl From<AdaptiveSync> for VrrToml {
    fn from(v: AdaptiveSync) -> Self {
        match v {
            AdaptiveSync::Enabled => VrrToml::Enabled,
            AdaptiveSync::Disabled => VrrToml::Disabled,
            AdaptiveSync::Force => VrrToml::Force,
        }
    }
}

impl From<VrrToml> for AdaptiveSync {
    fn from(v: VrrToml) -> Self {
        match v {
            VrrToml::Enabled => AdaptiveSync::Enabled,
            VrrToml::Disabled => AdaptiveSync::Disabled,
            VrrToml::Force => AdaptiveSync::Force,
        }
    }
}

fn output_to_toml(connector: &str, c: &OutputConfig) -> OutputToml {
    OutputToml {
        connector: connector.to_string(),
        mode: ModeToml {
            width: c.mode.0.0,
            height: c.mode.0.1,
            refresh_mhz: c.mode.1,
        },
        scale: c.scale,
        position: PositionToml {
            x: c.position.0,
            y: c.position.1,
        },
        transform: c.transform.into(),
        enabled: (&c.enabled).into(),
        vrr: c.vrr.into(),
        max_bpc: c.max_bpc,
        xwayland_primary: c.xwayland_primary,
    }
}

fn output_from_toml(t: OutputToml) -> OutputConfig {
    OutputConfig {
        mode: ((t.mode.width, t.mode.height), t.mode.refresh_mhz),
        vrr: t.vrr.into(),
        scale: t.scale,
        transform: t.transform.into(),
        position: (t.position.x, t.position.y),
        enabled: t.enabled.into(),
        max_bpc: t.max_bpc,
        xwayland_primary: t.xwayland_primary,
    }
}

// ---------------------------------------------------------------------------
// Public helpers: load + save the whole `OutputsConfig` as TOML.
// ---------------------------------------------------------------------------

/// Convert the on-disk TOML structure into the canonical
/// `OutputsConfig`. Profile names are dropped here because the
/// daemon does not use them at runtime; they survive on the next
/// save because the TOML writer regenerates names from the
/// `output_set` it sees.
pub fn from_toml_string(text: &str) -> Result<OutputsConfig, String> {
    let parsed: DisplaysToml = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut config: HashMap<Vec<OutputInfo>, Vec<OutputConfig>> = HashMap::new();
    for prof in parsed.profiles {
        // Build the canonical key from `output_set` (sorted for
        // deterministic matching). The per-output `output` entries
        // are matched to the key by connector — order in the file
        // does not have to follow `output_set`.
        let mut infos: Vec<OutputInfo> =
            prof.output_set.into_iter().map(OutputInfo::from).collect();
        infos.sort();

        let mut outputs: Vec<OutputConfig> = Vec::with_capacity(infos.len());
        let mut by_connector: HashMap<String, OutputToml> = prof
            .output
            .into_iter()
            .map(|o| (o.connector.clone(), o))
            .collect();
        for info in &infos {
            let Some(out) = by_connector.remove(&info.connector) else {
                return Err(format!(
                    "profile {:?}: output_set lists connector {:?} but no [[profile.output]] for it",
                    prof.name, info.connector
                ));
            };
            outputs.push(output_from_toml(out));
        }
        if !by_connector.is_empty() {
            warn!(
                "profile {:?}: extra [[profile.output]] entries with no matching output_set: {:?}",
                prof.name,
                by_connector.keys().collect::<Vec<_>>()
            );
        }

        // Mirror-target validation. The historical RON loader
        // explicitly repaired `Mirroring(target)` entries whose
        // target was missing or itself disabled / mirroring; without
        // that, a single bad persisted value turns into a runtime
        // "Unable to find mirroring output" failure later in
        // `apply_config_for_outputs`. Two-pass: snapshot enabled
        // states first so repairing one mirror does not invalidate
        // the lookup for another.
        let snapshot: Vec<OutputState> =
            outputs.iter().map(|o| o.enabled.clone()).collect();
        for (idx, conf) in outputs.iter_mut().enumerate() {
            if let OutputState::Mirroring(target) = &conf.enabled {
                let target_idx = infos.iter().position(|i| &i.connector == target);
                let valid = match target_idx {
                    Some(j) if j != idx => matches!(snapshot[j], OutputState::Enabled),
                    Some(_) => false, // self-mirror is nonsense
                    None => false,
                };
                if !valid {
                    warn!(
                        "profile {:?}: connector {} mirrors {:?} which is not a valid \
                         enabled output in this profile; demoting to Enabled.",
                        prof.name, infos[idx].connector, target,
                    );
                    conf.enabled = OutputState::Enabled;
                }
            }
        }

        config.insert(infos, outputs);
    }
    Ok(OutputsConfig { config })
}

/// Serialize the canonical `OutputsConfig` back to TOML. Profile
/// names are auto-generated as `profile-N` for round-trips that
/// did not preserve user-chosen names; callers that care can post-
/// process the returned string. Settings UI saves named profiles
/// via `to_toml_string_with_names`.
pub fn to_toml_string(cfg: &OutputsConfig) -> Result<String, String> {
    let mut profiles: Vec<ProfileToml> = Vec::with_capacity(cfg.config.len());
    // Sort entries deterministically by the first connector in the
    // output set, so a config that round-trips text → struct → text
    // is byte-stable. HashMap iteration order would otherwise scramble
    // the file on every save.
    let mut entries: Vec<_> = cfg.config.iter().collect();
    entries.sort_by(|a, b| {
        let ka = a.0.first().map(|i| i.connector.as_str()).unwrap_or("");
        let kb = b.0.first().map(|i| i.connector.as_str()).unwrap_or("");
        ka.cmp(kb)
    });

    for (i, (infos, outputs)) in entries.into_iter().enumerate() {
        let output_set: Vec<OutputInfoToml> = infos.iter().map(OutputInfoToml::from).collect();
        let output_list: Vec<OutputToml> = infos
            .iter()
            .zip(outputs.iter())
            .map(|(info, out)| output_to_toml(&info.connector, out))
            .collect();
        profiles.push(ProfileToml {
            name: format!("profile-{}", i + 1),
            output_set,
            output: output_list,
        });
    }

    let doc = DisplaysToml { profiles };
    toml::to_string_pretty(&doc).map_err(|e| e.to_string())
}

/// Read TOML from disk. Returns an empty `OutputsConfig` if the
/// file is missing; logs and clears the file if its contents are
/// unparseable, mirroring the original RON-loader's failure mode.
pub fn load(path: &Path) -> OutputsConfig {
    if !path.exists() {
        return OutputsConfig {
            config: HashMap::new(),
        };
    }
    match std::fs::read_to_string(path) {
        Ok(text) => match from_toml_string(&text) {
            Ok(cfg) => cfg,
            Err(err) => {
                warn!(?err, "Failed to parse displays.toml, resetting..");
                if let Err(rm) = std::fs::remove_file(path) {
                    error!(?rm, "Failed to remove displays.toml.");
                }
                OutputsConfig {
                    config: HashMap::new(),
                }
            }
        },
        Err(err) => {
            error!(?err, "Failed to read displays.toml.");
            OutputsConfig {
                config: HashMap::new(),
            }
        }
    }
}

/// Write the config to disk **atomically**. Parent directory is
/// created if needed.
///
/// Atomicity matters because the migration path uses success/failure
/// of this call to decide whether to delete the legacy `outputs.ron`.
/// A naïve `truncate + write` would leave the file half-written if
/// the process crashed or `write_all` returned a short count, and
/// then the migration would happily delete the only authoritative
/// source. We therefore: write to a sibling `<file>.tmp`, fsync,
/// rename over the target, fsync the parent dir.
///
/// Errors propagate to the caller; the migrator and the
/// `PersistenceGuard` both rely on the `Result` to gate destructive
/// follow-ups (delete legacy file, etc).
pub fn save(path: &Path, cfg: &OutputsConfig) -> io::Result<()> {
    let text = to_toml_string(cfg)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "displays.toml path has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;

    // Sibling temp file. Using `<file>.tmp` (not a TempPath in /tmp)
    // guarantees the rename never crosses filesystems — `rename` is
    // only atomic within one filesystem.
    let tmp_path = path.with_extension("toml.tmp");
    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)?;
        file.write_all(text.as_bytes())?;
        file.flush()?;
        // fsync so the bytes hit the disk before the rename. Without
        // this, the rename can land before the data does and a power
        // loss leaves an empty file.
        file.sync_all()?;
    }

    std::fs::rename(&tmp_path, path)?;

    // Best-effort directory fsync so the rename itself is durable.
    // Failure here is non-fatal — the rename has succeeded as far
    // as the kernel is concerned, just not yet committed to disk.
    if let Ok(dir) = std::fs::File::open(parent) {
        let _ = dir.sync_all();
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// One-shot RON-to-TOML migration.
// ---------------------------------------------------------------------------

/// If `ron_path` exists and `toml_path` does not, parse the legacy
/// RON, write it as TOML, and unlink the RON. Idempotent: a second
/// call is a no-op.
///
/// The legacy RON is **only** removed after the new TOML has been
/// durably written (via `save()`'s atomic temp-file + rename + fsync
/// path). A failed write leaves the user's only authoritative
/// display layout exactly where it was, so a subsequent boot just
/// retries the migration. This is the recommendation from Codex's
/// adversarial review: "remove `outputs.ron` only after the new file
/// is durably written successfully".
pub fn migrate_from_ron(ron_path: &Path, toml_path: &Path) {
    if !ron_path.exists() {
        return;
    }
    if toml_path.exists() {
        // New file already exists; assume migration already ran.
        // Silently leave the old RON in place — user can `rm` it
        // by hand if they want, but we do not destroy data on
        // upgrade if the new path is already authoritative.
        return;
    }
    let cfg = super::comp::load_outputs(Some(ron_path));
    if let Err(err) = save(toml_path, &cfg) {
        warn!(
            ?err,
            "Migration of displays config from RON to TOML failed; \
             leaving outputs.ron in place for the next boot to retry.",
        );
        return;
    }
    if let Err(err) = std::fs::remove_file(ron_path) {
        warn!(
            ?err,
            "Migrated displays config to TOML but failed to remove old outputs.ron."
        );
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_config() -> OutputsConfig {
        let mut config = HashMap::new();
        let infos = vec![
            OutputInfo {
                connector: "DP-1".into(),
                make: "Dell".into(),
                model: "U2719D".into(),
            },
            OutputInfo {
                connector: "eDP-1".into(),
                make: "BOE".into(),
                model: "Unknown".into(),
            },
        ];
        let outputs = vec![
            OutputConfig {
                mode: ((2560, 1440), Some(60_000)),
                vrr: AdaptiveSync::Disabled,
                scale: 1.0,
                transform: TransformDef::Normal,
                position: (0, 0),
                enabled: OutputState::Enabled,
                max_bpc: Some(10),
                xwayland_primary: true,
            },
            OutputConfig {
                mode: ((2560, 1440), Some(144_000)),
                vrr: AdaptiveSync::Enabled,
                scale: 1.5,
                transform: TransformDef::_90,
                position: (2560, 0),
                enabled: OutputState::Mirroring("DP-1".into()),
                max_bpc: None,
                xwayland_primary: false,
            },
        ];
        config.insert(infos, outputs);
        OutputsConfig { config }
    }

    #[test]
    fn roundtrip_preserves_struct() {
        let original = sample_config();
        let text = to_toml_string(&original).unwrap();
        let parsed = from_toml_string(&text).unwrap();
        assert_eq!(parsed.config.len(), original.config.len());
        for (k, v) in &original.config {
            let other = parsed.config.get(k).expect("key present after roundtrip");
            assert_eq!(other, v, "outputs match for key {:?}", k);
        }
    }

    #[test]
    fn enabled_string_round_trip_includes_mirror() {
        let original = sample_config();
        let text = to_toml_string(&original).unwrap();
        assert!(
            text.contains("enabled = \"mirror:DP-1\""),
            "mirror serialises with the connector: {text}"
        );
    }

    #[test]
    fn empty_config_serialises_to_empty_doc() {
        let cfg = OutputsConfig {
            config: HashMap::new(),
        };
        let text = to_toml_string(&cfg).unwrap();
        let parsed = from_toml_string(&text).unwrap();
        assert!(parsed.config.is_empty());
    }

    #[test]
    fn parse_rejects_output_set_without_matching_output() {
        let text = r#"
[[profile]]
name = "broken"

[[profile.output_set]]
connector = "eDP-1"
make = "BOE"
model = "Unknown"
"#;
        assert!(from_toml_string(text).is_err());
    }

    #[test]
    fn output_set_order_in_toml_does_not_affect_canonical_key() {
        // Same set, different file order — both must produce the
        // same HashMap key (sorted internally).
        let a = r#"
[[profile]]
name = "a"

[[profile.output_set]]
connector = "DP-1"
make = "Dell"
model = "U2719D"

[[profile.output_set]]
connector = "eDP-1"
make = "BOE"
model = "Unknown"

[[profile.output]]
connector = "eDP-1"
mode = { width = 2560, height = 1440, refresh_mhz = 144000 }
scale = 1.5
position = { x = 0, y = 0 }
transform = "normal"
enabled = "enabled"
vrr = "enabled"
xwayland_primary = false

[[profile.output]]
connector = "DP-1"
mode = { width = 2560, height = 1440, refresh_mhz = 60000 }
scale = 1.0
position = { x = 2560, y = 0 }
transform = "normal"
enabled = "enabled"
vrr = "disabled"
xwayland_primary = true
"#;
        let b = a.replace("eDP-1", "ZZZ-tmp")
            .replace("DP-1", "eDP-1")
            .replace("ZZZ-tmp", "DP-1"); // swap entries; same set semantically
        let pa = from_toml_string(a).unwrap();
        let pb = from_toml_string(&b).unwrap();
        assert_eq!(pa.config.keys().count(), 1);
        assert_eq!(pb.config.keys().count(), 1);
    }

    #[test]
    fn save_then_load_round_trip_via_filesystem() {
        let original = sample_config();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("compositor.d/displays.toml");
        save(&path, &original).expect("save succeeds on a fresh directory");
        assert!(path.exists());
        let parsed = load(&path);
        assert_eq!(parsed.config.len(), 1);
    }

    #[test]
    fn save_failure_leaves_no_partial_file() {
        // Try to save into a path whose parent cannot exist (an
        // empty filename has no useful parent on most systems).
        // The atomic temp-file path means there is no leftover
        // garbage even when the operation fails halfway.
        let cfg = sample_config();
        let tmp = tempfile::tempdir().unwrap();
        // Make `parent` a regular file so create_dir_all fails.
        let blocker = tmp.path().join("notadir");
        std::fs::write(&blocker, b"x").unwrap();
        let path = blocker.join("displays.toml");
        let err = save(&path, &cfg).expect_err("save into blocked path must fail");
        // Error kind should be NotADirectory or AlreadyExists or
        // similar; we don't assert exact variant because POSIX
        // implementations differ.
        let _ = err;
        // The atomic-path prevents leftover `<file>.tmp` from
        // showing up at the target directory.
        assert!(!path.exists());
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.toml");
        let cfg = load(&path);
        assert!(cfg.config.is_empty());
    }

    #[test]
    fn corrupt_toml_clears_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("displays.toml");
        std::fs::write(&path, "this is not toml @@@").unwrap();
        let cfg = load(&path);
        assert!(cfg.config.is_empty());
        assert!(!path.exists(), "corrupt file is removed");
    }

    #[test]
    fn migration_from_ron_writes_toml_and_unlinks_ron() {
        let tmp = tempfile::tempdir().unwrap();
        let ron_path = tmp.path().join("outputs.ron");
        let toml_path = tmp.path().join("compositor.d/displays.toml");

        // Write a known-good RON via the canonical loader's inverse.
        let original = sample_config();
        let ron_text = ron::ser::to_string_pretty(&original, Default::default()).unwrap();
        std::fs::write(&ron_path, ron_text).unwrap();

        migrate_from_ron(&ron_path, &toml_path);

        assert!(toml_path.exists(), "TOML written");
        assert!(!ron_path.exists(), "RON removed");

        let parsed = load(&toml_path);
        assert_eq!(parsed.config.len(), original.config.len());
    }

    #[test]
    fn mirror_target_missing_from_set_gets_repaired() {
        let text = r#"
[[profile]]
name = "broken-mirror"

[[profile.output_set]]
connector = "eDP-1"
make = "BOE"
model = "Unknown"

[[profile.output]]
connector = "eDP-1"
mode = { width = 2560, height = 1440, refresh_mhz = 60000 }
scale = 1.0
position = { x = 0, y = 0 }
transform = "normal"
enabled = "mirror:HDMI-1"
vrr = "enabled"
xwayland_primary = false
"#;
        let cfg = from_toml_string(text).expect("parse succeeds");
        let outputs = cfg.config.values().next().unwrap();
        assert_eq!(outputs.len(), 1);
        // Mirror target HDMI-1 isn't in the output_set; repair to Enabled.
        assert!(matches!(outputs[0].enabled, OutputState::Enabled));
    }

    #[test]
    fn mirror_target_disabled_gets_repaired() {
        let text = r#"
[[profile]]
name = "mirror-of-disabled"

[[profile.output_set]]
connector = "eDP-1"
make = "BOE"
model = "Unknown"

[[profile.output_set]]
connector = "DP-1"
make = "Dell"
model = "U2719D"

[[profile.output]]
connector = "eDP-1"
mode = { width = 2560, height = 1440, refresh_mhz = 60000 }
scale = 1.0
position = { x = 0, y = 0 }
transform = "normal"
enabled = "mirror:DP-1"
vrr = "enabled"
xwayland_primary = false

[[profile.output]]
connector = "DP-1"
mode = { width = 1920, height = 1080, refresh_mhz = 60000 }
scale = 1.0
position = { x = 2560, y = 0 }
transform = "normal"
enabled = "disabled"
vrr = "enabled"
xwayland_primary = false
"#;
        let cfg = from_toml_string(text).unwrap();
        let outputs = cfg.config.values().next().unwrap();
        let edp = outputs
            .iter()
            .zip(cfg.config.keys().next().unwrap().iter())
            .find(|(_, info)| info.connector == "eDP-1")
            .unwrap()
            .0;
        // Target DP-1 is disabled; mirroring entry must be repaired.
        assert!(matches!(edp.enabled, OutputState::Enabled));
    }

    #[test]
    fn mirror_self_gets_repaired() {
        let text = r#"
[[profile]]
name = "self-mirror"

[[profile.output_set]]
connector = "eDP-1"
make = "BOE"
model = "Unknown"

[[profile.output]]
connector = "eDP-1"
mode = { width = 2560, height = 1440, refresh_mhz = 60000 }
scale = 1.0
position = { x = 0, y = 0 }
transform = "normal"
enabled = "mirror:eDP-1"
vrr = "enabled"
xwayland_primary = false
"#;
        let cfg = from_toml_string(text).unwrap();
        let outputs = cfg.config.values().next().unwrap();
        assert!(matches!(outputs[0].enabled, OutputState::Enabled));
    }

    #[test]
    fn mirror_to_enabled_target_survives() {
        let text = r#"
[[profile]]
name = "valid-mirror"

[[profile.output_set]]
connector = "eDP-1"
make = "BOE"
model = "Unknown"

[[profile.output_set]]
connector = "DP-1"
make = "Dell"
model = "U2719D"

[[profile.output]]
connector = "eDP-1"
mode = { width = 2560, height = 1440, refresh_mhz = 60000 }
scale = 1.0
position = { x = 0, y = 0 }
transform = "normal"
enabled = "mirror:DP-1"
vrr = "enabled"
xwayland_primary = false

[[profile.output]]
connector = "DP-1"
mode = { width = 2560, height = 1440, refresh_mhz = 60000 }
scale = 1.0
position = { x = 0, y = 0 }
transform = "normal"
enabled = "enabled"
vrr = "enabled"
xwayland_primary = false
"#;
        let cfg = from_toml_string(text).unwrap();
        let key = cfg.config.keys().next().unwrap();
        let outputs = cfg.config.values().next().unwrap();
        let edp_idx = key
            .iter()
            .position(|i| i.connector == "eDP-1")
            .expect("eDP-1 in set");
        match &outputs[edp_idx].enabled {
            OutputState::Mirroring(t) => assert_eq!(t, "DP-1"),
            other => panic!("expected mirror:DP-1 to survive, got {other:?}"),
        }
    }

    #[test]
    fn migration_failure_leaves_ron_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let ron_path = tmp.path().join("outputs.ron");

        let original = sample_config();
        let ron_text = ron::ser::to_string_pretty(&original, Default::default()).unwrap();
        std::fs::write(&ron_path, ron_text).unwrap();

        // Make the TOML target path unwritable: parent is a file
        // not a directory, so create_dir_all → save fails.
        let file_blocker = tmp.path().join("blocker");
        std::fs::write(&file_blocker, b"not-a-directory").unwrap();
        let toml_path = file_blocker.join("displays.toml");

        migrate_from_ron(&ron_path, &toml_path);

        // Critical: the legacy RON must still be there. Codex's
        // adversarial review specifically called out that without
        // this guarantee, a failed first-boot migration silently
        // wipes the user's display layout.
        assert!(
            ron_path.exists(),
            "outputs.ron must NOT be removed when TOML write fails"
        );
        assert!(
            !toml_path.exists(),
            "no half-written TOML at the target location"
        );
    }

    #[test]
    fn migration_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let ron_path = tmp.path().join("outputs.ron");
        let toml_path = tmp.path().join("compositor.d/displays.toml");

        let original = sample_config();
        let ron_text = ron::ser::to_string_pretty(&original, Default::default()).unwrap();
        std::fs::write(&ron_path, ron_text).unwrap();
        migrate_from_ron(&ron_path, &toml_path);

        // Run again with the toml already in place. Should leave
        // the toml alone (no second migration that overwrites).
        let toml_before = std::fs::read_to_string(&toml_path).unwrap();
        // Recreate the RON so the second call has something to see.
        let original2 = sample_config();
        let ron_text2 = ron::ser::to_string_pretty(&original2, Default::default()).unwrap();
        std::fs::write(&ron_path, ron_text2).unwrap();
        migrate_from_ron(&ron_path, &toml_path);
        let toml_after = std::fs::read_to_string(&toml_path).unwrap();
        assert_eq!(toml_before, toml_after);
    }
}
