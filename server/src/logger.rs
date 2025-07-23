use fern::colors::{Color, ColoredLevelConfig};

pub(crate) fn try_init_logger() -> Result<(), log::SetLoggerError> {
    let colors = ColoredLevelConfig::new()
        .debug(Color::Blue)
        .info(Color::Green)
        .warn(Color::Yellow)
        .error(Color::Red);

    // 1. Custom format + outputs
    let (_, log) = fern::Dispatch::new()
        .chain(
            fern::Dispatch::new()
                .format(move |out, message, record| {
                    let formatted = format!(
                        "{} {} {} > {}",
                        chrono::Local::now().format("[%Y-%m-%d %H:%M:%S]"),
                        colors.color(record.level()),
                        record.target(),
                        message
                    );
                    out.finish(format_args!("{}", formatted))
                })
                .chain(std::io::stdout()),
        )
        .chain(
            fern::Dispatch::new()
                .format(|out, message, record| {
                    let formatted = format!(
                        "{} {} {} > {}",
                        chrono::Local::now().format("[%Y-%m-%d %H:%M:%S]"),
                        record.level(),
                        record.target(),
                        message
                    );
                    out.finish(format_args!("{}", formatted))
                })
                .chain(fern::DateBased::new("", "%Y-%m-%d.log")),
        )
        .into_log();

    // 2. Env filter
    let filter = env_filter::Builder::from_env("RUST_LOG").build();
    let max_level = filter.filter();

    let logger = env_filter::FilteredLog::new(log, filter);
    log::set_boxed_logger(Box::new(logger))?;
    log::set_max_level(max_level);
    Ok(())
}
