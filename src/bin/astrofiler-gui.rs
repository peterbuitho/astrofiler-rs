// Desktop launcher: same app as `astrofiler gui`, but without a console
// window on Windows.
#![cfg_attr(windows, windows_subsystem = "windows")]

fn main() {
    astrofiler::logging::init(false, false);
    let result = astrofiler::config::Config::load().and_then(astrofiler::gui::run);
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
