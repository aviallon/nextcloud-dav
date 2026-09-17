// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

use nextcloud_dav::auth::Authenticator;
use nextcloud_dav::config::{Config, Opt, HELP};
use nextcloud_dav::db::Db;
use nextcloud_dav::php::PhpClient;
use nextcloud_dav::routes::{router, AppState};
use std::error::Error;
use std::sync::Arc;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let opt = Opt::parse()?;
    if opt.help {
        print!("{HELP}");
        return Ok(());
    }
    if opt.version {
        println!("nextcloud-dav {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let log_level = opt.log_level.clone().unwrap_or_else(|| "info".to_string());
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();

    let config = Config::from_opt(opt)?;
    log::info!(
        "starting nextcloud-dav: config={}, listen={}, db_prefix={}, secret_configured={}",
        config.config_path.display(),
        config.listen,
        config.database_prefix,
        config.secret_configured()
    );

    let db = Arc::new(
        Db::new(
            config.database.clone(),
            config.database_prefix.clone(),
            config.max_connections,
        )
        .await?,
    );
    db.ping().await?;

    let php = PhpClient::new(
        &config.nextcloud_url,
        config.php_timeout,
        config.allow_self_signed,
    )?;
    let auth = Authenticator::new(
        Arc::clone(&db),
        php,
        config.secret.clone(),
        config.bruteforce.clone(),
    );

    let state = Arc::new(AppState {
        db,
        auth,
        config: config.clone(),
    });
    let app = router(state);

    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    log::info!("listening on http://{}", config.listen);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    log::info!("shutdown signal received");
}
