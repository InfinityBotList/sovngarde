use std::time::Duration;

use actix_cors::Cors;
use actix_web::{http, middleware, web, App, HttpServer};
use libavacado::types::CacheHttpImpl;
use log::info;
use serenity::async_trait;
use serenity::client::{Context, EventHandler};
use serenity::model::gateway::{GatewayIntents, Ready};
use sqlx::postgres::PgPoolOptions;

mod models;
mod routes;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

struct MainHandler {}

#[async_trait]
impl EventHandler for MainHandler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("Bot is connected: {}", ready.user.name);
    }
}

#[actix_rt::main]
async fn main() -> std::io::Result<()> {
    const MAX_CONNECTIONS: u32 = 3;

    info!("Starting up now!");

    std::env::set_var("RUST_LOG", "api=info");

    env_logger::init();

    let pool = PgPoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect(&libavacado::CONFIG.database_url)
        .await
        .expect("Could not initialize connection");

    info!("Connected to postgres with pool size: {}", pool.size());

    let mut main_cli = serenity::Client::builder(
        &libavacado::CONFIG.token,
        GatewayIntents::GUILDS
            | GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::GUILD_MEMBERS
            | GatewayIntents::GUILD_PRESENCES,
    )
    .event_handler(MainHandler {})
    .await
    .unwrap();

    let cache_http = CacheHttpImpl {
        cache: main_cli.cache.clone(),
        http: main_cli.http.clone(),
    };

    tokio::task::spawn(async move { main_cli.start().await });

    let app_state = web::Data::new(models::AppState {
        pool,
        cache_http: cache_http,
        ratelimits: moka::future::Cache::builder()
        // Time to live (TTL): 7 minutes
        .time_to_live(Duration::from_secs(60 * 7))
        // Create the cache.
        .build(),        
    });

    HttpServer::new(move || {
        let cors = Cors::default()
            .allowed_origin_fn(|origin, _req_head| {
                origin.as_bytes().ends_with(libavacado::CONFIG.frontend_url.as_bytes())
                || origin.as_bytes().ends_with("localhost:3000".as_bytes())
            })
            .allowed_methods(vec!["POST", "OPTIONS"])
            .allowed_headers(vec![
                http::header::ACCEPT,
                http::header::CONTENT_TYPE,
            ])
            .max_age(1);

        App::new()
            .app_data(app_state.clone())
            .wrap(cors)
            .wrap(middleware::Compress::default())
            .wrap(middleware::Logger::default())
            .service(routes::web_rpc_api)
    })
    // The below can be increased if needed
    .workers(2)
    .backlog(32)
    .max_connection_rate(32)
    // Some requests can take a while
    .client_disconnect_timeout(Duration::from_secs(0))
    .bind("localhost:3010")?
    .run()
    .await
}
