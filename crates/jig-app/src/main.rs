//! The `jig-workbench` binary — a shim over [`jig_app::run`].

// Release builds on Windows must not open a console window behind the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![forbid(unsafe_code)]

fn main() {
    jig_app::run();
}
