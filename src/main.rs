// SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

use nextcloud_dav::auth::Authenticator;
use nextcloud_dav::config::{Config, Opt, DEFAULT_CARD_SIZE_LIMIT, HELP};
use nextcloud_dav::db::Db;
use nextcloud_dav::outbox::{self, EffectRegistry};
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

    let mut config = Config::from_opt(opt)?;
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

    // `ShareDisableChecker` strips the SHARE permission from listings when
    // `core/shareapi_exclude_groups` is configured. The sidecar cannot
    // reproduce LDAP/circle group expansion, so it delegates instead. The
    // check is cached for the process lifetime (an app-config change needs a
    // restart, like `card_size_limit`).
    config.sharing_exclude_groups = db
        .appconfig_value("core", "shareapi_exclude_groups")
        .await?
        .map(|value| !value.is_empty() && value != "no")
        .unwrap_or(false);
    if config.sharing_exclude_groups {
        log::warn!("core/shareapi_exclude_groups is configured; files PROPFIND stays on PHP");
    }
    // `nc:is-encrypted` is served only by the end-to-end encryption app. Without
    // it PHP has no handler and answers 404, which the sidecar reproduces; with
    // it the sidecar must delegate so the client sees the real value.
    config.e2e_encryption = db
        .appconfig_value("end_to_end_encryption", "enabled")
        .await?
        .map(|value| value == "yes")
        .unwrap_or(false);
    if config.e2e_encryption {
        log::warn!("end_to_end_encryption is enabled; nc:is-encrypted delegates to PHP");
    }
    if config.instance_id.is_empty() {
        log::warn!("config.php has no instanceid; files PROPFIND stays on PHP");
    }

    // The write-size limit: config override first, then the `dav` app-config
    // value, then Nextcloud's default. Cached for the process lifetime.
    let card_size_limit = match config.card_size_limit_override {
        Some(limit) => limit,
        None => db
            .appconfig_value("dav", "card_size_limit")
            .await?
            .and_then(|value| value.trim().parse::<u64>().ok())
            .filter(|limit| *limit > 0)
            .unwrap_or(DEFAULT_CARD_SIZE_LIMIT),
    };

    // The outbox table is owned by the companion PHP app; the sidecar only
    // validates it. Without it (or with event dispatch disabled) writes stay
    // 501 so nginx falls back to PHP, while reads keep working.
    let registry = Arc::new(EffectRegistry::from_config(&config.event_dispatch));
    let mut native_writes = config.event_dispatch.enabled;
    if native_writes {
        match outbox::verify_schema(db.pool(), &config.database_prefix).await {
            Ok(()) => log::info!("event outbox present; native PUT/DELETE enabled"),
            Err(error) => {
                native_writes = false;
                log::error!(
                    "event outbox table is missing or has an unexpected shape ({error}); \
                     refusing native writes and returning 501 for PUT/DELETE"
                );
            }
        }
    } else {
        log::warn!("nextcloud_dav.event_dispatch.enabled is false; native writes refused");
    }

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
        card_size_limit,
        native_writes,
        registry,
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
