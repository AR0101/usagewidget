// Hides the console window that Windows would otherwise attach to a release
// build. Debug builds keep it, since that is where `--dump` prints.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    usagewidget_lib::run()
}
