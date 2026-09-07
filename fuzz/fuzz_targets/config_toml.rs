// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

#![no_main]
// zeph-fuzz declares one shared dependency list (fuzz/Cargo.toml) across all five
// [[bin]] targets; each target uses only a subset, so cargo force-warns
// `unused_crate_dependencies` per binary (denied by `build.warnings = "deny"`,
// .cargo/config.toml) for the deps this target doesn't reference.
#![allow(unused_crate_dependencies)]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &str| {
    let _ = toml::from_str::<zeph_config::Config>(data);
});
