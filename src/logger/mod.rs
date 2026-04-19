// SPDX-License-Identifier: GPL-3.0-only

use std::str::FromStr;

use anyhow::Result;

use tracing::{debug, info, warn};
use tracing_journald as journald;
use tracing_subscriber::{EnvFilter, filter::Directive, fmt, prelude::*};

pub fn init_logger() -> Result<()> {
    // When `RUST_LOG` is set we respect it verbatim. When it isn't, fall
    // back to `info` in dev builds / `warn` in release. Either way, the
    // two noisy crates (`cosmic_text`, `calloop`) are always silenced —
    // they emit traces users never want to read.
    //
    // Previously this also hardcoded `smithay=debug` and
    // `cosmic_comp=debug` via `add_directive`, which silently
    // *overrode* whatever `RUST_LOG` the caller had set. A user asking
    // for `RUST_LOG=info` still got ~300 DEBUG lines per keypress
    // (toml_keybindings trace, focus events, tiling-layer chatter).
    // Those upgrade directives now only apply when `RUST_LOG` is unset,
    // i.e. only as a dev-mode default.
    let env_set = std::env::var("RUST_LOG").is_ok_and(|v| !v.is_empty());
    let level = if cfg!(debug_assertions) {
        "debug"
    } else {
        "warn"
    };
    let mut filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| {
            EnvFilter::new(if cfg!(debug_assertions) {
                "info"
            } else {
                "warn"
            })
        })
        .add_directive(Directive::from_str("cosmic_text=error").unwrap())
        .add_directive(Directive::from_str("calloop=error").unwrap());
    if !env_set {
        filter = filter
            .add_directive(Directive::from_str(&format!("smithay={level}")).unwrap())
            .add_directive(Directive::from_str(&format!("cosmic_comp={level}")).unwrap());
    }

    let fmt_layer = fmt::layer().compact();

    match journald::layer() {
        Ok(journald_layer) => tracing_subscriber::registry()
            .with(fmt_layer)
            .with(journald_layer)
            .with(filter)
            .init(),
        Err(err) => {
            tracing_subscriber::registry()
                .with(fmt_layer)
                .with(filter)
                .init();
            warn!(?err, "Failed to init journald logging.");
        }
    };
    log_panics::init();

    info!("Version: {}", std::env!("CARGO_PKG_VERSION"));
    if cfg!(feature = "debug") {
        debug!(
            "Debug build ({})",
            std::option_env!("GIT_HASH").unwrap_or("Unknown")
        );
    }

    Ok(())
}
