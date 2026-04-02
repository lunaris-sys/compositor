// SPDX-License-Identifier: GPL-3.0-only

use std::sync::OnceLock;

/// An _unstable_ interface to customize cosmic-comp at compile-time by providing
/// hooks to be run in specific code paths.
#[derive(Default, Debug, Clone)]
pub struct Hooks {}

pub static HOOKS: OnceLock<Hooks> = OnceLock::new();
