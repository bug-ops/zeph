// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

fn main() {
    println!("cargo:rerun-if-changed=migrations/");
}
