/// Internal output configurations used by cosmic-comp
pub mod comp;

#[cfg(feature = "output")]
/// TOML serialisation layer that mirrors `comp::OutputsConfig` into
/// a `[[profile]]`-array document at
/// `~/.config/lunaris/compositor.d/displays.toml`. RON keying by
/// `Vec<OutputInfo>` cannot survive TOML's string-keyed maps, so
/// each map entry becomes a profile record. See
/// `docs/architecture/display-system.md` §A1.
pub mod displays_toml;

#[cfg(feature = "randr")]
/// cosmic-randr style output configurations
pub mod randr;
