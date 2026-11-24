#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ui;

fn main() -> eframe::Result {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.iter().any(|arg| arg == "--organize") {
        match mldo::organizer_cli::run(&args) {
            Ok(report) => println!("Relatório: {}", report.display()),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }
    if std::env::args().any(|arg| arg == "--browser-worker") {
        mldo::browser::run_worker();
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("MLDO — Multi Link Download Organizer")
            .with_inner_size([1180.0, 820.0])
            .with_min_inner_size([1020.0, 720.0])
            .with_icon(ui::app_icon()),
        ..Default::default()
    };
    eframe::run_native(
        "MLDO",
        options,
        Box::new(|cc| Ok(Box::new(ui::App::new(cc)))),
    )
}
