// Copyright (c) 2022 Unfolded Circle ApS, Markus Zehnder <markus.z@unfoldedcircle.com>
// Copyright (c) 2026 Henrik Widlund
// SPDX-License-Identifier: MPL-2.0

//! Apple TV integration for the Unfolded Circle Remote.
//!
//! Implements the Integration-API as a WebSocket server. The Apple TV devices are controlled
//! with the `ratv` library.

mod config_store;
mod configuration;
mod controller;
mod device;
mod entities;
mod errors;
mod logging;
mod server;
mod startup;
mod util;

use crate::configuration::{ENV_DISABLE_MDNS_PUBLISH, Settings};
use crate::controller::Controller;
use crate::server::{ArtworkStore, publish_service};
use crate::util::bool_from_env;
use actix::Actor;
use actix_web::{App, HttpServer, web};
use log::{error, info};
use startup::{APP_VERSION, built_info};
use std::io;
use std::net::TcpListener;
use uc_api::intg::IntegrationDriverUpdate;
use uc_api::util::text_from_language_map;

#[actix_web::main]
async fn main() -> io::Result<()> {
    let settings = Settings::from_env();
    let driver_settings = config_store::load_settings(&settings.config_home);
    logging::init(driver_settings.debug_log);
    info!("Starting {} {APP_VERSION}", built_info::PKG_NAME);
    if driver_settings.debug_log && !logging::env_filter() {
        info!("Debug log is on");
    }
    let driver_metadata = configuration::get_driver_metadata()?;
    let address = format!("{}:{}", settings.interface, settings.http_port);
    let listener = TcpListener::bind(&address)?;
    info!("Listening on: {address}");

    let artwork = ArtworkStore::new(settings.config_home.join("artwork"));
    let heartbeat = web::Data::new(settings.heartbeat);
    let api_port = settings.http_port;
    let controller = web::Data::new(
        Controller::new(
            settings,
            driver_settings,
            driver_metadata.clone(),
            artwork.clone(),
        )
        .start(),
    );
    let artwork = web::Data::new(artwork);

    let http_server = HttpServer::new(move || {
        App::new()
            .app_data(heartbeat.clone())
            .app_data(controller.clone())
            .app_data(artwork.clone())
            .service(server::ws_index)
            .service(server::artwork_index)
    })
    .workers(1)
    // Open WebSocket connections do not end by themselves: do not wait the default 30 s.
    .shutdown_timeout(10)
    .listen(listener)?;

    if !bool_from_env(ENV_DISABLE_MDNS_PUBLISH) {
        publish_mdns(api_port, driver_metadata);
    }

    http_server.run().await
}

/// Advertise the integration driver with mDNS.
fn publish_mdns(api_port: u16, drv_metadata: IntegrationDriverUpdate) {
    let Some(driver_id) = drv_metadata.driver_id else {
        error!("driver_id missing in driver metadata, mDNS service not published");
        return;
    };
    let txt = vec![
        format!(
            "name={}",
            text_from_language_map(drv_metadata.name.as_ref(), "en").unwrap_or("Apple TV")
        ),
        format!(
            "developer={}",
            drv_metadata
                .developer
                .and_then(|d| d.name)
                .unwrap_or_default()
        ),
        "ws_path=/".into(),
        format!("pwd={}", drv_metadata.pwd_protected.unwrap_or_default()),
        format!("ver={APP_VERSION}"),
    ];
    if let Err(e) = publish_service(driver_id, "uc-integration", "tcp", api_port, txt) {
        error!("Error publishing mDNS service: {e}");
    }
}
