//! cosmicbar — a libcosmic (wlr-layer-shell) status bar.
//!
//! One process owns one layer surface per output; every module is a Rust
//! widget fed by a push subscription (unix socket, D-Bus signal, niri IPC
//! event stream) instead of a polling shell script.

mod bar;
mod config;
mod control;
mod extension;
mod fill;
mod glyph;
mod hover;
mod modules;
mod popup;
mod theme;

fn main() -> cosmic::iced::Result {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("warn,cosmicbar=info"),
    )
    .init();

    // `cosmicbar toggle network` talks to the running bar instead of starting one.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        let line = args.join(" ");
        if let Err(error) = control::send(&line) {
            eprintln!("cosmicbar: {error:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    let config = config::Config::load();

    theme::set_font(theme::font(config.font_weight_bold));
    theme::set_icon_font(theme::icon_font(config.font_weight_bold));

    let settings = cosmic::app::Settings::default()
        // The bar owns layer surfaces only: no xdg-toplevel, and closing a
        // surface (output unplugged) must not end the process.
        .no_main_window(true)
        .exit_on_close(false)
        .antialiasing(true)
        .client_decorations(false)
        .default_text_size(config.font_size)
        .default_font(theme::font(config.font_weight_bold))
        // Use the live COSMIC preference so default widgets and the bar's own
        // palette agree, including after a light/dark or accent change.
        .theme(cosmic::theme::system_preference());

    cosmic::app::run::<bar::Bar>(settings, config)
}
