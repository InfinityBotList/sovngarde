use std::fmt::Display;
use std::os::unix::prelude::PermissionsExt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use crate::impls;
use crate::impls::target_types::TargetType;
use crate::panelapi::types::{
    auth::{MfaLogin, MfaLoginSecret},
    entity::{PartialBot, PartialEntity},
    cdn::{CdnAssetAction, CdnAssetItem},
    partners::{Partners, PartnerType, Partner, CreatePartner},
    rpc::RPCWebAction,
    webcore::{Capability, CoreConstants, InstanceConfig, PanelServers}
};
use crate::rpc::core::{RPCHandle, RPCMethod};
use axum::body::StreamBody;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderMap;
use axum::Json;

use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{extract::State, http::StatusCode, Router};
use log::info;
use moka::future::Cache;
use rand::Rng;
use serenity::all::User;
use sqlx::PgPool;
use tokio::io::{AsyncWriteExt, AsyncReadExt};
use tower_http::cors::{Any, CorsLayer};

use serde::{Deserialize, Serialize};
use strum::VariantNames;
use strum_macros::{Display, EnumString, EnumVariantNames};
use ts_rs::TS;
use utoipa::ToSchema;
use sha2::{Sha512, Digest};

struct Error {
    status: StatusCode,
    message: String,
}

impl Error {
    fn new(e: impl Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: e.to_string(),
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

pub struct AppState {
    pub cache_http: impls::cache::CacheHttpImpl,
    pub pool: PgPool,
    pub cdn_file_chunks_cache: Cache<String, Vec<u8>>,
}

pub async fn init_panelapi(pool: PgPool, cache_http: impls::cache::CacheHttpImpl) {
    use utoipa::OpenApi;
    #[derive(OpenApi)]
    #[openapi(
        paths(query),
        components(schemas(PanelQuery, InstanceConfig, RPCMethod, TargetType))
    )]
    struct ApiDoc;

    async fn docs() -> impl IntoResponse {
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", "application/json".parse().unwrap());
        let data = ApiDoc::openapi().to_json();

        if let Ok(data) = data {
            return (headers, data).into_response();
        }

        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to generate docs".to_string(),
        )
            .into_response()
    }

    sqlx::query!(
        "CREATE TABLE IF NOT EXISTS staffpanel__authchain (
            itag UUID NOT NULL UNIQUE DEFAULT uuid_generate_v4(),
            paneldata_ref UUID NOT NULL REFERENCES staffpanel__paneldata(itag) ON DELETE CASCADE,
            user_id TEXT NOT NULL REFERENCES users(user_id) ON DELETE CASCADE,
            token TEXT NOT NULL,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
            state TEXT NOT NULL DEFAULT 'pending'
        )"
    )
    .execute(&pool)
    .await
    .expect("Failed to create staffpanel__authchain table");

    sqlx::query!(
        "CREATE TABLE IF NOT EXISTS staffpanel__paneldata (
            itag UUID NOT NULL UNIQUE DEFAULT uuid_generate_v4(),
            user_id TEXT PRIMARY KEY REFERENCES users(user_id) ON DELETE CASCADE,
            mfa_secret TEXT NOT NULL,
            mfa_verified BOOLEAN NOT NULL DEFAULT FALSE,
            created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
        )"
    )
    .execute(&pool)
    .await
    .expect("Failed to create staffpanel__paneldata table");

    let cdn_file_chunks_cache = Cache::<String, Vec<u8>>::builder()
    .time_to_live(Duration::from_secs(3600))
    .build();

    let shared_state = Arc::new(AppState { pool, cache_http, cdn_file_chunks_cache });

    let app = Router::new()
        .route("/openapi", get(docs))
        .route("/", post(query))
        .with_state(shared_state)
        .layer(DefaultBodyLimit::max(1048576000))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    let addr = "127.0.0.1:3010"
        .parse()
        .expect("Invalid RPC server address");

    info!("Starting PanelAPI server on {}", addr);

    if let Err(e) = axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
    {
        panic!("PanelAPI server error: {}", e);
    }
}

#[derive(Serialize, Deserialize, ToSchema, TS, Display, Clone, EnumString, EnumVariantNames)]
#[ts(export, export_to = ".generated/PanelQuery.ts")]
pub enum PanelQuery {
    /// Returns instance configuration and other important information
    Hello {
        /// Panel protocol version, should be 2
        version: u16,
    },
    /// Get Login URL
    GetLoginUrl {
        /// Panel protocol version, should be 2
        version: u16,
        /// Redirect URL
        redirect_url: String,
    },
    /// Login, returning a login token
    Login {
        /// Discord OAuth2 code
        code: String,
        /// Redirect URL
        redirect_url: String,
    },
    /// Check MFA status for a given login token
    LoginMfaCheckStatus {
        /// Login token
        login_token: String,
    },
    /// Activates a session for a given login token
    LoginActivateSession {
        /// Login token
        login_token: String,
        /// MFA code
        otp: String,
    },
    /// Resets MFA for a user identified by login token
    LoginResetMfa {
        /// Login token
        login_token: String,
        /// Old MFA code
        otp: String,
    },
    /// Logs out a session. Should be called when the user logs out of the panel
    Logout {
        /// Login token
        login_token: String,
    },
    /// Get Identity (user_id/created_at) for a given login token
    GetIdentity {
        /// Login token
        login_token: String,
    },
    /// Returns user information given a user id, returning a dovewing PartialUser
    GetUserDetails {
        /// User ID to fetch details for
        user_id: String,
    },
    /// Given a user ID, returns the permissions for that user
    GetUserPerms {
        /// User ID to fetch perms for
        user_id: String,
    },
    /// Given a login token, returns the capabilities for that user
    GetCapabilities {
        /// Login token
        login_token: String,
    },
    /// Given a login token, returns core constants for the panel for that user
    GetCoreConstants {
        /// Login token
        login_token: String,
    },
    /// Returns the bot queue
    BotQueue {
        /// Login token
        login_token: String,
    },
    /// Executes an RPC on a target
    ExecuteRpc {
        /// Login token
        login_token: String,
        /// Target Type
        target_type: TargetType,
        /// RPC Method
        method: RPCMethod,
    },
    /// Returns all RPC actions available
    ///
    /// Setting filtered will filter RPC actions to that what the user has access to
    GetRpcMethods {
        /// Login token
        login_token: String,
        /// Filtered
        filtered: bool,
    },
    /// Returns a list of the supported RPC entity types
    GetRpcTargetTypes {
        /// Login token
        login_token: String,
    },
    /// Searches for a bot based on a query
    SearchEntitys {
        /// Login token
        login_token: String,
        /// Target type
        target_type: TargetType,
        /// Query
        query: String,
    },
    /// Uploads a chunk of data returning a chunk ID
    /// 
    /// Chunks expire after 10 minutes and are stored in memory
    /// 
    /// After uploading all chunks for a file, use `AddFile` to create the file
    UploadCdnFileChunk {
        /// Login token
        login_token: String,
        /// Array of bytes of the chunk contents
        chunk: Vec<u8>,
    },
    /// Lists all available CDN scopes
    ListCdnScopes {
        /// Login token
        login_token: String,
    },
    /// Returns the main CDN scope for Infinity Bot List
    GetMainCdnScope {
        /// Login token
        login_token: String,
    },
    /// Updates/handles an asset on the CDN
    UpdateCdnAsset {
        /// Login token
        login_token: String,
        /// CDN scope
        /// 
        /// This describes a location where the CDN may be stored on disk and should be a full path to it
        /// 
        /// Currently the panel uses the following scopes:
        /// 
        /// `ibl@main`
        cdn_scope: String,
        /// Asset name
        name: String,
        /// Path
        path: String,
        /// Action to take
        action: CdnAssetAction,
    },
    /// Returns the list of all partners on the list
    GetPartnerList {
        /// Login token
        login_token: String,
    },
    /// Adds a partner
    /// 
    /// This technically only needs the PartnerManagement capability, 
    /// but also requires a CDN asset upload capability as well to upload the avatar
    /// of the partner
    AddPartner {
        /// Login token
        login_token: String,
        /// Partner Data
        partner: CreatePartner,
    },
    /// Deletes a partner
    DeletePartner {
        /// Login token
        login_token: String,
        /// Partner ID
        partner_id: String,
    },
}

/// Make Panel Query
#[utoipa::path(
    post,
    request_body = PanelQuery,
    path = "/",
    responses(
        (status = 200, description = "Content", body = String),
        (status = 204, description = "No content"),
        (status = BAD_REQUEST, description = "An error occured", body = String),
    ),
)]
#[axum_macros::debug_handler]
async fn query(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PanelQuery>,
) -> Result<impl IntoResponse, Error> {
    match req {
        PanelQuery::Hello { version } => {
            if version != 2 {
                return Ok((StatusCode::BAD_REQUEST, "Invalid version".to_string()).into_response());
            }

            Ok((
                StatusCode::OK,
                Json(InstanceConfig {
                    description: "Arcadia Production Panel Instance".to_string(),
                    warnings: vec![
                        "The panel is currently undergoing large-scale changes while it is being rewritten. Please report any bugs you find to the staff team.".to_string(),
                    ],
                }),
            )
                .into_response())        
        },
        PanelQuery::GetLoginUrl {
            version,
            redirect_url,
        } => {
            if version != 2 {
                return Ok((StatusCode::BAD_REQUEST, "Invalid version".to_string()).into_response());
            }

            Ok(
                (
                    StatusCode::OK,
                    format!(
                        "https://discord.com/api/oauth2/authorize?client_id={client_id}&redirect_uri={redirect_url}&response_type=code&scope=identify",
                        client_id = crate::config::CONFIG.panel.client_id,
                        redirect_url = redirect_url
                    )
                ).into_response()
            )
        }
        PanelQuery::Login { code, redirect_url } => {
            if !crate::config::CONFIG
                .panel
                .redirect_url
                .contains(&redirect_url)
            {
                return Ok(
                    (StatusCode::BAD_REQUEST, "Invalid redirect url".to_string()).into_response(),
                );
            }

            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .map_err(Error::new)?;

            let resp = client
                .post("https://discord.com/api/oauth2/token")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("User-Agent", "DiscordBot (arcadia v1.0)")
                .form(&[
                    (
                        "client_id",
                        crate::config::CONFIG.panel.client_id.as_str(),
                    ),
                    (
                        "client_secret",
                        crate::config::CONFIG.panel.client_secret.as_str(),
                    ),
                    ("grant_type", "authorization_code"),
                    ("code", code.as_str()),
                    ("redirect_uri", redirect_url.as_str()),
                    ("scope", "identify"),
                ])
                .send()
                .await
                .map_err(Error::new)?
                .error_for_status()
                .map_err(Error::new)?;

            #[derive(Deserialize)]
            struct Oauth2 {
                access_token: String,
            }

            let oauth2 = resp.json::<Oauth2>().await.map_err(Error::new)?;

            let user_resp = client
                .get("https://discord.com/api/users/@me")
                .header(
                    "Authorization",
                    "Bearer ".to_string() + oauth2.access_token.as_str(),
                )
                .header("Content-Type", "application/x-www-form-urlencoded")
                .header("User-Agent", "DiscordBot (arcadia v1.0)")
                .send()
                .await
                .map_err(Error::new)?
                .error_for_status()
                .map_err(Error::new)?;

            let user = user_resp.json::<User>().await.map_err(Error::new)?;

            let rec = sqlx::query!(
                "SELECT staff FROM users WHERE user_id = $1",
                user.id.to_string()
            )
            .fetch_one(&state.pool)
            .await
            .map_err(Error::new)?;

            if !rec.staff {
                return Ok((StatusCode::FORBIDDEN, "You are not staff".to_string()).into_response());
            }

            let mut tx = state.pool.begin().await.map_err(Error::new)?;

            sqlx::query!(
                "DELETE FROM staffpanel__authchain WHERE user_id = $1",
                user.id.to_string()
            )
            .execute(&mut *tx)
            .await
            .map_err(Error::new)?;

            // Create a random number between 4196 and 6000 for the token
            let tlength = rand::thread_rng().gen_range(4196..6000);

            let token = crate::impls::crypto::gen_random(tlength as usize);

            let count = sqlx::query!(
                "SELECT COUNT(*) FROM staffpanel__paneldata WHERE user_id = $1",
                user.id.to_string()
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::new)?
            .count
            .unwrap_or(0);

            let itag = if count == 0 {
                let temp_secret = thotp::generate_secret(160);

                let temp_secret_enc = thotp::encoding::encode(&temp_secret, data_encoding::BASE32);

                sqlx::query!(
                    "INSERT INTO staffpanel__paneldata (user_id, mfa_secret) VALUES ($1, $2) RETURNING itag",
                    user.id.to_string(),
                    temp_secret_enc
                )
                .fetch_one(&mut *tx)
                .await
                .map_err(Error::new)?
                .itag
            } else {
                sqlx::query!(
                    "SELECT itag FROM staffpanel__paneldata WHERE user_id = $1",
                    user.id.to_string()
                )
                .fetch_one(&mut *tx)
                .await
                .map_err(Error::new)?
                .itag
            };

            sqlx::query!(
                "INSERT INTO staffpanel__authchain (user_id, paneldata_ref, token) VALUES ($1, $2, $3)",
                user.id.to_string(),
                itag,
                token,
            )
            .execute(&mut *tx)
            .await
            .map_err(Error::new)?;

            tx.commit().await.map_err(Error::new)?;

            // Stage 1 of login is done, panel will handle MFA next
            Ok((StatusCode::OK, token).into_response())
        }
        PanelQuery::LoginMfaCheckStatus { login_token } => {
            let auth_data = super::auth::check_auth_insecure(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;
            if auth_data.state != "pending" {
                return Err(Error {
                    status: StatusCode::BAD_REQUEST,
                    message: "sessionAlreadyActive".to_string(),
                });
            }

            let mut tx = state.pool.begin().await.map_err(Error::new)?;

            // Check if user already has MFA setup
            let count = sqlx::query!(
                "SELECT COUNT(*) FROM staffpanel__paneldata WHERE user_id = $1",
                auth_data.user_id
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::new)?
            .count
            .unwrap_or(0);

            if count == 0 {
                // This should never happen, as Login creates a dummy MFA setup
                return Err(Error {
                    status: StatusCode::BAD_REQUEST,
                    message: "invalidPanelData".to_string(),
                });
            }

            // Check if user has MFA setup
            let mrec = sqlx::query!(
                "SELECT mfa_verified FROM staffpanel__paneldata WHERE user_id = $1",
                auth_data.user_id
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::new)?;

            if !mrec.mfa_verified {
                // User does not have MFA setup, generate a secret
                let secret_vec = thotp::generate_secret(160);
                let secret = thotp::encoding::encode(&secret_vec, data_encoding::BASE32);

                sqlx::query!(
                    "UPDATE staffpanel__paneldata SET mfa_secret = $2 WHERE user_id = $1",
                    auth_data.user_id,
                    secret
                )
                .execute(&mut *tx)
                .await
                .map_err(Error::new)?;

                let qr_code_uri = thotp::qr::otp_uri(
                    // Type of otp
                    "totp",
                    // The encoded secret
                    &secret,
                    // Your big corp title
                    "Infinity Bot List:staff@infinitybots.gg",
                    // Your big corp issuer
                    "Infinity Bot List",
                    // The counter (Only HOTP)
                    None,
                )
                .map_err(Error::new)?;

                let qr = thotp::qr::generate_code_svg(
                    &qr_code_uri,
                    // The qr code width (None defaults to 200)
                    None,
                    // The qr code height (None defaults to 200)
                    None,
                    // Correction level, M is the default
                    thotp::qr::EcLevel::M,
                )
                .map_err(Error::new)?;

                tx.commit().await.map_err(Error::new)?;

                Ok((
                    StatusCode::OK,
                    Json(MfaLogin {
                        info: Some(MfaLoginSecret {
                            qr_code: qr,
                            otp_url: qr_code_uri,
                            secret,
                        }),
                    }),
                )
                    .into_response())
            } else {
                tx.rollback().await.map_err(Error::new)?;

                Ok((StatusCode::OK, Json(MfaLogin { info: None })).into_response())
            }
        }
        PanelQuery::LoginActivateSession { login_token, otp } => {
            let auth_data = super::auth::check_auth_insecure(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            let mut tx = state.pool.begin().await.map_err(Error::new)?;

            let count = sqlx::query!(
                "SELECT COUNT(*) FROM staffpanel__paneldata WHERE user_id = $1",
                auth_data.user_id
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::new)?
            .count
            .unwrap_or(0);

            if count == 0 {
                return Err(Error {
                    status: StatusCode::BAD_REQUEST,
                    message: "mfaNotSetup".to_string(),
                });
            }

            let secret = sqlx::query!(
                "SELECT mfa_secret FROM staffpanel__paneldata WHERE user_id = $1",
                auth_data.user_id
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::new)?
            .mfa_secret;

            let secret =
                thotp::encoding::decode(&secret, data_encoding::BASE32).map_err(Error::new)?;

            let (result, _discrepancy) = thotp::verify_totp(&otp, &secret, 0).unwrap();

            if !result {
                return Err(Error {
                    status: StatusCode::BAD_REQUEST,
                    message: "mfaInvalidCode".to_string(),
                });
            }

            sqlx::query!(
                "UPDATE staffpanel__authchain SET state = 'active' WHERE token = $1",
                login_token
            )
            .execute(&mut *tx)
            .await
            .map_err(Error::new)?;

            sqlx::query!(
                "UPDATE staffpanel__paneldata SET mfa_verified = TRUE WHERE user_id = $1",
                auth_data.user_id
            )
            .execute(&mut *tx)
            .await
            .map_err(Error::new)?;

            tx.commit().await.map_err(Error::new)?;

            Ok((StatusCode::NO_CONTENT, "").into_response())
        }
        PanelQuery::LoginResetMfa { login_token, otp } => {
            let auth_data = super::auth::check_auth(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            let mut tx = state.pool.begin().await.map_err(Error::new)?;

            let count = sqlx::query!(
                "SELECT COUNT(*) FROM staffpanel__paneldata WHERE user_id = $1",
                auth_data.user_id
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::new)?
            .count
            .unwrap_or(0);

            if count == 0 {
                return Err(Error {
                    status: StatusCode::BAD_REQUEST,
                    message: "mfaNotSetup".to_string(),
                });
            }

            let secret = sqlx::query!(
                "SELECT mfa_secret FROM staffpanel__paneldata WHERE user_id = $1",
                auth_data.user_id
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(Error::new)?
            .mfa_secret;

            let secret =
                thotp::encoding::decode(&secret, data_encoding::BASE32).map_err(Error::new)?;

            let (result, _discrepancy) = thotp::verify_totp(&otp, &secret, 0).unwrap();

            if !result {
                return Err(Error {
                    status: StatusCode::BAD_REQUEST,
                    message: "mfaInvalidCode".to_string(),
                });
            }

            sqlx::query!(
                "UPDATE staffpanel__paneldata SET mfa_verified = FALSE WHERE user_id = $1",
                auth_data.user_id
            )
            .execute(&mut *tx)
            .await
            .map_err(Error::new)?;

            // Revoke existing session
            sqlx::query!(
                "DELETE FROM staffpanel__authchain WHERE user_id = $1",
                auth_data.user_id
            )
            .execute(&mut *tx)
            .await
            .map_err(Error::new)?;

            tx.commit().await.map_err(Error::new)?;

            Ok((StatusCode::NO_CONTENT, "").into_response())
        }
        PanelQuery::Logout { login_token } => {
            // Just delete the auth, no point in even erroring if it doesn't exist
            let row = sqlx::query!(
                "DELETE FROM staffpanel__authchain WHERE token = $1",
                login_token
            )
            .execute(&state.pool)
            .await
            .map_err(Error::new)?;

            Ok((StatusCode::OK, row.rows_affected().to_string()).into_response())
        }
        PanelQuery::GetIdentity { login_token } => {
            let auth_data = super::auth::check_auth(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            Ok((StatusCode::OK, Json(auth_data)).into_response())
        }
        PanelQuery::GetUserDetails { user_id } => {
            let user = crate::impls::dovewing::get_partial_user(&state.pool, &user_id)
                .await
                .map_err(Error::new)?;

            Ok((StatusCode::OK, Json(user)).into_response())
        }
        PanelQuery::GetUserPerms { user_id } => {
            let perms = super::auth::get_user_perms(&state.pool, &user_id)
                .await
                .map_err(Error::new)?;

            Ok((StatusCode::OK, Json(perms)).into_response())
        }
        PanelQuery::GetCapabilities { login_token } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            Ok((StatusCode::OK, Json(caps)).into_response())
        }
        PanelQuery::GetCoreConstants { login_token } => {
            // Ensure auth is valid, that's all that matters here
            super::auth::check_auth(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            Ok((
                StatusCode::OK,
                Json(CoreConstants {
                    frontend_url: crate::config::CONFIG.frontend_url.clone(),
                    infernoplex_url: crate::config::CONFIG.infernoplex_url.clone(),
                    popplio_url: crate::config::CONFIG.popplio_url.clone(),
                    cdn_url: crate::config::CONFIG.cdn_url.clone(),
                    servers: PanelServers {
                        main: crate::config::CONFIG.servers.main.to_string(),
                        staff: crate::config::CONFIG.servers.staff.to_string(),
                        testing: crate::config::CONFIG.servers.testing.to_string(),
                    },
                }),
            )
                .into_response())
        }
        PanelQuery::BotQueue { login_token } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            if !caps.contains(&Capability::ViewBotQueue) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "You do not have permission to access the bot queue right now".to_string(),
                )
                    .into_response());
            }

            let queue = sqlx::query!(
                "SELECT bot_id, client_id, claimed_by, type, approval_note, short, invite,
                votes, shards, library, invite_clicks, clicks, servers 
                FROM bots WHERE type = 'pending' OR type = 'claimed' ORDER BY created_at"
            )
            .fetch_all(&state.pool)
            .await
            .map_err(Error::new)?;

            let mut bots = Vec::new();

            for bot in queue {
                let owners = crate::impls::utils::get_entity_managers(
                    TargetType::Bot,
                    &bot.bot_id,
                    &state.pool,
                )
                .await
                .map_err(Error::new)?;

                let user = crate::impls::dovewing::get_partial_user(&state.pool, &bot.bot_id)
                    .await
                    .map_err(Error::new)?;

                bots.push(PartialEntity::Bot(PartialBot {
                    bot_id: bot.bot_id,
                    client_id: bot.client_id,
                    user,
                    claimed_by: bot.claimed_by,
                    approval_note: bot.approval_note,
                    short: bot.short,
                    r#type: bot.r#type,
                    votes: bot.votes,
                    shards: bot.shards,
                    library: bot.library,
                    invite_clicks: bot.invite_clicks,
                    clicks: bot.clicks,
                    servers: bot.servers,
                    mentionable: owners.mentionables(),
                    invite: bot.invite,
                }));
            }

            Ok((StatusCode::OK, Json(bots)).into_response())
        }
        PanelQuery::ExecuteRpc {
            login_token,
            target_type,
            method,
        } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            if !caps.contains(&Capability::Rpc) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "You do not have permission to use RPC right now".to_string(),
                )
                    .into_response());
            }

            let auth_data = super::auth::check_auth(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            let resp = method
                .handle(RPCHandle {
                    pool: state.pool.clone(),
                    cache_http: state.cache_http.clone(),
                    user_id: auth_data.user_id,
                    target_type,
                })
                .await;

            match resp {
                Ok(r) => match r {
                    crate::rpc::core::RPCSuccess::NoContent => {
                        Ok((StatusCode::NO_CONTENT, "").into_response())
                    }
                    crate::rpc::core::RPCSuccess::Content(c) => {
                        Ok((StatusCode::OK, c).into_response())
                    }
                },
                Err(e) => Ok((StatusCode::BAD_REQUEST, e.to_string()).into_response()),
            }
        }
        PanelQuery::GetRpcMethods {
            login_token,
            filtered,
        } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            if !caps.contains(&Capability::Rpc) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "You do not have permission to use RPC right now".to_string(),
                )
                    .into_response());
            }

            let auth_data = super::auth::check_auth(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            let (owner, head, admin, staff) = {
                let perms = sqlx::query!(
                    "SELECT owner, hadmin, iblhdev, admin, staff FROM users WHERE user_id = $1",
                    auth_data.user_id
                )
                .fetch_one(&state.pool)
                .await
                .map_err(Error::new)?;

                (
                    perms.owner,
                    perms.hadmin || perms.iblhdev,
                    perms.admin,
                    perms.staff,
                )
            };

            let mut rpc_methods = Vec::new();

            for method in crate::rpc::core::RPCMethod::VARIANTS {
                let variant = crate::rpc::core::RPCMethod::from_str(method).map_err(Error::new)?;

                if filtered {
                    match variant.needs_perms() {
                        crate::rpc::core::RPCPerms::Owner => {
                            if !owner {
                                continue;
                            }
                        }
                        crate::rpc::core::RPCPerms::Head => {
                            if !head {
                                continue;
                            }
                        }
                        crate::rpc::core::RPCPerms::Admin => {
                            if !admin {
                                continue;
                            }
                        }
                        crate::rpc::core::RPCPerms::Staff => {
                            if !staff {
                                continue;
                            }
                        }
                    }
                }

                let action = RPCWebAction {
                    id: method.to_string(),
                    label: variant.label(),
                    description: variant.description(),
                    needs_perms: variant.needs_perms(),
                    supported_target_types: variant.supported_target_types(),
                    fields: variant.method_fields(),
                };

                rpc_methods.push(action);
            }

            Ok((StatusCode::OK, Json(rpc_methods)).into_response())
        }
        PanelQuery::GetRpcTargetTypes { login_token } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            if !caps.contains(&Capability::Rpc) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "You do not have permission to use RPC right now?".to_string(),
                )
                    .into_response());
            }

            Ok((StatusCode::OK, Json(TargetType::VARIANTS)).into_response())
        }
        PanelQuery::SearchEntitys {
            login_token,
            target_type,
            query,
        } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            match target_type {
                TargetType::Bot => {
                    if !caps.contains(&Capability::BotManagement) {
                        return Ok((
                            StatusCode::FORBIDDEN,
                            "You do not have permission to manage bots right now?".to_string(),
                        )
                            .into_response());
                    }

                    let queue = sqlx::query!(
                        "
                        SELECT bot_id, client_id, type, votes, shards, library, invite_clicks, clicks,
                        servers, claimed_by, approval_note, short, invite FROM bots 
                        INNER JOIN internal_user_cache__discord discord_users ON bots.bot_id = discord_users.id
                        WHERE bot_id = $1 OR client_id = $1 OR discord_users.username ILIKE $2 ORDER BY bots.created_at
                        ",
                        query,
                        format!("%{}%", query)
                    )
                    .fetch_all(&state.pool)
                    .await
                    .map_err(Error::new)?;

                    let mut bots = Vec::new();

                    for bot in queue {
                        let owners = crate::impls::utils::get_entity_managers(
                            TargetType::Bot,
                            &bot.bot_id,
                            &state.pool,
                        )
                        .await
                        .map_err(Error::new)?;

                        let user =
                            crate::impls::dovewing::get_partial_user(&state.pool, &bot.bot_id)
                                .await
                                .map_err(Error::new)?;

                        bots.push(PartialEntity::Bot(PartialBot {
                            bot_id: bot.bot_id,
                            client_id: bot.client_id,
                            user,
                            r#type: bot.r#type,
                            votes: bot.votes,
                            shards: bot.shards,
                            library: bot.library,
                            invite_clicks: bot.invite_clicks,
                            clicks: bot.clicks,
                            servers: bot.servers,
                            claimed_by: bot.claimed_by,
                            approval_note: bot.approval_note,
                            short: bot.short,
                            mentionable: owners.mentionables(),
                            invite: bot.invite,
                        }));
                    }

                    Ok((StatusCode::OK, Json(bots)).into_response())
                }
                _ => Ok((
                    StatusCode::NOT_IMPLEMENTED,
                    "Searching this target type is not implemented".to_string(),
                )
                    .into_response()),
            }
        },
        PanelQuery::UploadCdnFileChunk { login_token, chunk } => {
            info!("Got chunk: {}", chunk.len());
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            if !caps.contains(&Capability::CdnManagement) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "You do not have permission to manage the CDN right now?".to_string(),
                )
                    .into_response());
            }

            // Check that length of chunk is not greater than 100MB
            if chunk.len() > 100_000_000 {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Chunk size is too large".to_string(),
                )
                    .into_response());
            }

            // Check that chunk is not empty
            if chunk.is_empty() {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Chunk is empty".to_string(),
                )
                    .into_response());
            }

            // Create chunk ID
            let chunk_id = crate::impls::crypto::gen_random(32);

            // Keep looping until we get a free chunk ID
            let mut tries = 0;

            while tries < 10 {
                if !state.cdn_file_chunks_cache.contains_key(&chunk_id) {
                    state
                        .cdn_file_chunks_cache
                        .insert(chunk_id.clone(), chunk)
                        .await;

                    return Ok((StatusCode::OK, chunk_id).into_response());
                }

                tries += 1;
            }

            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to generate a chunk ID".to_string(),
            )
                .into_response())
        },
        PanelQuery::ListCdnScopes { login_token } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            if !caps.contains(&Capability::CdnManagement) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "You do not have permission to manage the CDN right now?".to_string(),
                )
                    .into_response());
            }

            Ok((StatusCode::OK, Json(crate::config::CONFIG.panel.cdn_scopes.clone())).into_response())
        },
        PanelQuery::GetMainCdnScope {
            login_token,
        } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
            .await
            .map_err(Error::new)?;

            if !caps.contains(&Capability::CdnManagement) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "You do not have permission to manage the CDN right now?".to_string(),
                )
                    .into_response());
            }

            Ok((StatusCode::OK, crate::config::CONFIG.panel.main_scope.clone()).into_response())
        },
        PanelQuery::UpdateCdnAsset {
            login_token,
            name,
            path,
            action,
            cdn_scope,
        } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            if !caps.contains(&Capability::CdnManagement) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "You do not have permission to manage the CDN right now?".to_string(),
                )
                    .into_response());
            }

            // Get cdn path from cdn_scope hashmap
            let Some(cdn_path) = crate::config::CONFIG.panel.cdn_scopes.get(&cdn_scope) else {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Invalid CDN scope".to_string(),
                )
                    .into_response());
            };

            fn validate_name(name: &str) -> Result<(), crate::Error> {
                const ALLOWED_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.:%$[](){}$@! ";

                // 1. Ensure all chars of name are in ALLOWED_CHARS
                // 2. Ensure name does not contain a slash
                // 3. Ensure name does not contain a backslash
                // 4. Ensure name does not start with a dot
                if name.chars().any(|c| !ALLOWED_CHARS.contains(c))
                    || name.contains('/')
                    || name.contains('\\')
                    || name.starts_with('.')
                {
                    return Err(
                        "Asset name cannot contain disallowed characters, slashes or backslashes or start with a dot"
                            .into(),
                    );
                }
            
                Ok(())
            }
            
            fn validate_path(path: &str) -> Result<(), crate::Error> {
                const ALLOWED_CHARS: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_.:%$/ ";

                // 1. Ensure all chars of name are in ALLOWED_CHARS
                // 2. Ensure path does not contain a dot-dot (path escape)
                // 3. Ensure path does not contain a double slash
                // 4. Ensure path does not contain a backslash
                // 5. Ensure path does not start with a slash
                if path.chars().any(|c| !ALLOWED_CHARS.contains(c))
                    || path.contains("..")
                    || path.contains("//")
                    || path.contains('\\')
                    || path.starts_with('/')
                {
                    return Err("Asset path cannot contain non-ASCII characters, dot-dots, doubleslashes, backslashes or start with a slash".into());
                }
            
                Ok(())
            }            

            validate_name(&name).map_err(Error::new)?;
            validate_path(&path).map_err(Error::new)?;

            // Get asset path and final resolved path
            let asset_path = if path.is_empty() {
                cdn_path.path.to_string()
            } else {
                format!("{}/{}", cdn_path.path, path)
            };

            let asset_final_path = if name.is_empty() {
                asset_path.clone()
            } else {
                format!("{}/{}", asset_path, name)
            };

            match action {
                CdnAssetAction::ListPath => {
                    match std::fs::metadata(&asset_path) {
                        Ok(m) => {
                            if !m.is_dir() {
                                return Ok((
                                    StatusCode::BAD_REQUEST,
                                    "Asset path already exists and is not a directory".to_string(),
                                )
                                    .into_response());
                            }
                        }
                        Err(e) => {
                            return Ok((
                                StatusCode::BAD_REQUEST,
                                "Fetching asset metadata failed: ".to_string() + &e.to_string(),
                            )
                                .into_response());
                        }
                    }

                    let mut files = Vec::new();

                    for entry in std::fs::read_dir(&asset_path).map_err(Error::new)? {
                        let entry = entry.map_err(Error::new)?;

                        let meta = entry.metadata().map_err(Error::new)?;

                        let efn = entry.file_name();
                        let Some(name) = efn.to_str() else {
                            continue;
                        };

                        files.push(CdnAssetItem {
                            name: name.to_string(),
                            path: entry
                                .path()
                                .to_str()
                                .unwrap_or_default()
                                .to_string()
                                .replace(&cdn_path.path, ""),
                            size: meta.len(),
                            last_modified: meta
                                .modified()
                                .map_err(Error::new)?
                                .duration_since(std::time::UNIX_EPOCH)
                                .map_err(Error::new)?
                                .as_secs(),
                            is_dir: meta.is_dir(),
                            permissions: meta.permissions().mode(),
                        });
                    }

                    Ok((StatusCode::OK, Json(files)).into_response())
                }
                CdnAssetAction::ReadFile => {
                    match std::fs::metadata(&asset_final_path) {
                        Ok(m) => {
                            if !m.is_file() {
                                return Ok((
                                    StatusCode::BAD_REQUEST,
                                    "Asset path is not a file".to_string(),
                                )
                                    .into_response());
                            }
                        }
                        Err(e) => {
                            return Ok((
                                StatusCode::BAD_REQUEST,
                                "Fetching asset metadata failed: ".to_string() + &e.to_string(),
                            )
                                .into_response());
                        }
                    }

                    let file = match tokio::fs::File::open(&asset_final_path).await {
                        Ok(file) => file,
                        Err(e) => {
                            return Ok((
                                StatusCode::BAD_REQUEST,
                                "Reading file failed: ".to_string() + &e.to_string(),
                            )
                                .into_response());
                        }
                    };

                    let stream = tokio_util::io::ReaderStream::new(file);
                    let body = StreamBody::new(stream);

                    let headers = [(axum::http::header::CONTENT_TYPE, "application/octet-stream")];

                    Ok((headers, body).into_response())
                }
                CdnAssetAction::CreateFolder => {
                    match std::fs::metadata(&asset_final_path) {
                        Ok(_) => {
                            return Ok((
                                StatusCode::BAD_REQUEST,
                                "Asset path already exists".to_string(),
                            )
                                .into_response());
                        }
                        Err(e) => {
                            if e.kind() != std::io::ErrorKind::NotFound {
                                return Ok((
                                    StatusCode::BAD_REQUEST,
                                    "Fetching asset metadata failed due to unknown error: "
                                        .to_string()
                                        + &e.to_string(),
                                )
                                    .into_response());
                            }
                        }
                    }

                    // Create path
                    std::fs::DirBuilder::new()
                        .recursive(true)
                        .create(&asset_final_path)
                        .map_err(Error::new)?;

                    Ok((StatusCode::NO_CONTENT, "").into_response())
                }
                CdnAssetAction::AddFile {
                    overwrite,
                    chunks,
                    sha512
                } => {
                    if chunks.is_empty() {
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            "No chunks were provided".to_string(),
                        )
                            .into_response());
                    }

                    if chunks.len() > 100_000 {
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            "Too many chunks were provided".to_string(),
                        )
                            .into_response());
                    }

                    for chunk in &chunks {
                        if !state.cdn_file_chunks_cache.contains_key(chunk) {
                            return Ok((
                                StatusCode::BAD_REQUEST,
                                "Chunk does not exist".to_string(),
                            )
                                .into_response());
                        }
                    }

                    // Check if the asset exists
                    match std::fs::metadata(&asset_final_path) {
                        Ok(m) => {
                            if overwrite {
                                if m.is_dir() {
                                    return Ok((
                                        StatusCode::BAD_REQUEST,
                                        "Asset to be replaced is a directory".to_string(),
                                    )
                                        .into_response());
                                }
                            } else {
                                return Ok((
                                    StatusCode::BAD_REQUEST,
                                    "Asset already exists".to_string(),
                                )
                                    .into_response());
                            }
                        }
                        Err(e) => {
                            if e.kind() != std::io::ErrorKind::NotFound {
                                return Ok((
                                    StatusCode::BAD_REQUEST,
                                    "Fetching asset metadata failed due to unknown error: "
                                        .to_string()
                                        + &e.to_string(),
                                )
                                    .into_response());
                            }
                        }
                    }

                    match std::fs::metadata(&asset_path) {
                        Ok(m) => {
                            if !m.is_dir() {
                                return Ok((
                                    StatusCode::BAD_REQUEST,
                                    "Asset path already exists and is not a directory".to_string(),
                                )
                                    .into_response());
                            }
                        }
                        Err(e) => {
                            if e.kind() != std::io::ErrorKind::NotFound {
                                return Ok((
                                    StatusCode::BAD_REQUEST,
                                    "Fetching asset metadata failed due to unknown error: "
                                        .to_string()
                                        + &e.to_string(),
                                )
                                    .into_response());
                            } else {
                                // Create path
                                std::fs::DirBuilder::new()
                                    .recursive(true)
                                    .create(&asset_path)
                                    .map_err(Error::new)?;
                            }
                        }
                    }

                    {
                        let tmp_file_path = format!(
                            "/tmp/arcadia-cdn-file{}@{}", 
                            crate::impls::crypto::gen_random(32),
                            asset_final_path.replace('/', ">")
                        );

                        let mut temp_file = tokio::fs::File::create(
                            &tmp_file_path
                        )
                            .await
                            .map_err(Error::new)?;

                        // For each chunk, fetch and add to file
                        for chunk in chunks {
                            let chunk = state
                                .cdn_file_chunks_cache
                                .remove(&chunk)
                                .await
                                .ok_or_else(|| Error::new("Chunk ".to_string() + &chunk + " does not exist"))?;

                            temp_file.write_all(&chunk).await.map_err(Error::new)?;
                        }

                        // Sync file
                        temp_file.sync_all().await.map_err(Error::new)?;

                        // Close file
                        drop(temp_file);

                        // Calculate sha512 of file
                        let mut hasher = Sha512::new();

                        let mut file = tokio::fs::File::open(
                            &tmp_file_path
                        )
                            .await
                            .map_err(Error::new)?;

                        let mut file_buf = Vec::new();
                        file.read_to_end(&mut file_buf).await.map_err(Error::new)?;

                        hasher.update(&file_buf);

                        let hash = hasher.finalize();

                        let hash_expected = data_encoding::HEXLOWER.encode(&hash);

                        if sha512 != hash_expected {
                            return Ok((
                                StatusCode::BAD_REQUEST,
                                "SHA512 hash does not match".to_string(),
                            )
                                .into_response());
                        }

                        // Rename temp file to final path
                        tokio::fs::copy(&tmp_file_path, &asset_final_path).await.map_err(Error::new)?;

                        // Delete temp file
                        tokio::fs::remove_file(&tmp_file_path).await.map_err(Error::new)?;
                    }

                    Ok((StatusCode::NO_CONTENT, "").into_response())
                }
                CdnAssetAction::CopyFile {
                    overwrite,
                    delete_original,
                    copy_to,
                } => {
                    validate_path(&copy_to).map_err(Error::new)?;

                    let copy_to = if copy_to.is_empty() {
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            "copy_to location cannot be empty".to_string(),
                        )
                            .into_response());
                    } else {
                        format!("{}/{}", cdn_path.path, copy_to)
                    };

                    match std::fs::metadata(&copy_to) {
                        Ok(m) => {
                            if !m.is_dir() && !overwrite {
                                return Ok((
                                    StatusCode::BAD_REQUEST,
                                    "copy_to location already exists".to_string(),
                                )
                                    .into_response());
                            }
                        }
                        Err(e) => {
                            if e.kind() != std::io::ErrorKind::NotFound {
                                return Ok((
                                    StatusCode::BAD_REQUEST,
                                    "Fetching asset metadata failed due to unknown error: "
                                        .to_string()
                                        + &e.to_string(),
                                )
                                    .into_response());
                            }
                        }
                    }

                    match std::fs::metadata(&asset_final_path) {
                        Ok(m) => {
                            if m.is_symlink() || m.is_file() {
                                if delete_original {
                                    // This is just a rename operation
                                    std::fs::rename(&asset_final_path, &copy_to)
                                        .map_err(|e| {
                                            Error::new(format!(
                                                "Failed to rename file: {} from {} to {}",
                                                e,
                                                &asset_final_path,
                                                &copy_to
                                            ))
                                        })?;
                                } else {
                                    // This is a copy operation
                                    std::fs::copy(&asset_final_path, &copy_to)
                                        .map_err(Error::new)?;
                                }
                            } else if m.is_dir() {
                                if delete_original {
                                    // This is a rename operation
                                    fn rename_dir_all(src: impl AsRef<std::path::Path>, dst: impl AsRef<std::path::Path>) -> std::io::Result<()> {
                                        std::fs::create_dir_all(&dst)?;
                                        for entry in std::fs::read_dir(src)? {
                                            let entry = entry?;
                                            let ty = entry.file_type()?;
                                            if ty.is_dir() {
                                                rename_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
                                            } else {
                                                std::fs::rename(entry.path(), dst.as_ref().join(entry.file_name()))?;
                                            }
                                        }
                                        Ok(())
                                    }

                                    rename_dir_all(&asset_final_path, &copy_to)
                                        .map_err(Error::new)?;

                                    // Delete original directory
                                    std::fs::remove_dir_all(&asset_final_path).map_err(Error::new)?;
                                } else {
                                    // This is a recursive copy operation
                                    fn copy_dir_all(src: impl AsRef<std::path::Path>, dst: impl AsRef<std::path::Path>) -> std::io::Result<()> {
                                        std::fs::create_dir_all(&dst)?;
                                        for entry in std::fs::read_dir(src)? {
                                            let entry = entry?;
                                            let ty = entry.file_type()?;
                                            if ty.is_dir() {
                                                copy_dir_all(entry.path(), dst.as_ref().join(entry.file_name()))?;
                                            } else {
                                                std::fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
                                            }
                                        }
                                        Ok(())
                                    }

                                    copy_dir_all(&asset_final_path, &copy_to)
                                        .map_err(Error::new)?;
                                }
                            }
                        }
                        Err(e) => {
                            return Ok((
                                StatusCode::BAD_REQUEST,
                                "Could not find asset: ".to_string() + &e.to_string() + &format!(
                                    " (path: {})",
                                    &asset_final_path
                                ),
                            )
                                .into_response());
                        }
                    }

                    Ok((StatusCode::NO_CONTENT, "").into_response())
                }
                CdnAssetAction::Delete => {
                    // Check if the asset exists
                    match std::fs::metadata(&asset_final_path) {
                        Ok(m) => {
                            if m.is_symlink() || m.is_file() {
                                // Delete the symlink
                                std::fs::remove_file(asset_final_path).map_err(Error::new)?;
                            } else if m.is_dir() {
                                // Delete the directory
                                std::fs::remove_dir_all(asset_final_path).map_err(Error::new)?;
                            }

                            Ok((StatusCode::NO_CONTENT, "").into_response())
                        }
                        Err(e) => Ok((
                            StatusCode::BAD_REQUEST,
                            "Could not find asset: ".to_string() + &e.to_string(),
                        )
                            .into_response()),
                    }
                }
            }
        }
        PanelQuery::GetPartnerList { login_token } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            if !caps.contains(&Capability::PartnerManagement) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "You do not have permission to manage partners right now?".to_string(),
                )
                    .into_response());
            }

            let prec = sqlx::query!(
                "SELECT id, name, image_type, short, links, type, created_at, user_id FROM partners"
            )
            .fetch_all(&state.pool)
            .await
            .map_err(Error::new)?;
    
            let mut partners = Vec::new();
    
            for partner in prec {
                partners.push(Partner {
                    id: partner.id,
                    name: partner.name,
                    image_type: partner.image_type,
                    short: partner.short,
                    links: serde_json::from_value(partner.links).map_err(Error::new)?,
                    r#type: partner.r#type,
                    created_at: partner.created_at,
                    user_id: partner.user_id,
                })
            }
    
            let ptrec = sqlx::query!("SELECT id, name, short, icon, created_at FROM partner_types")
                .fetch_all(&state.pool)
                .await
                .map_err(Error::new)?;
    
            let mut partner_types = Vec::new();
    
            for partner_type in ptrec {
                partner_types.push(PartnerType {
                    id: partner_type.id,
                    name: partner_type.name,
                    short: partner_type.short,
                    icon: partner_type.icon,
                    created_at: partner_type.created_at,
                })
            }    

            Ok((StatusCode::OK, Json(Partners {
                partners,
                partner_types,
            })).into_response())
        },
        PanelQuery::AddPartner {
            login_token,
            partner
        } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            if !caps.contains(&Capability::PartnerManagement) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "You do not have permission to manage partners right now?".to_string(),
                )
                    .into_response());
            }

            // Check if partner type exists
            let partner_type_exists = sqlx::query!(
                "SELECT id FROM partner_types WHERE id = $1",
                partner.r#type
            )
            .fetch_optional(&state.pool)
            .await
            .map_err(Error::new)?
            .is_some();

            if !partner_type_exists {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Partner type does not exist".to_string(),
                )
                    .into_response());
            }

            // Ensure that image has been uploaded to CDN
            // Get cdn path from cdn_scope hashmap
            let Some(cdn_path) = crate::config::CONFIG.panel.cdn_scopes.get(&crate::config::CONFIG.panel.main_scope) else {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Main scope not found".to_string(),
                )
                    .into_response());
            };

            let path = format!("{}/partners/{}.{}", cdn_path.path, partner.id, partner.image_type);

            match std::fs::metadata(&path) {
                Ok(m) => {
                    if !m.is_file() {
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            "Image does not exist".to_string(),
                        )
                            .into_response());
                    }

                    if m.len() > 100_000_000 {
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            "Image is too large".to_string(),
                        )
                            .into_response());
                    }

                    if m.len() == 0 {
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            "Image is empty".to_string(),
                        )
                            .into_response());
                    }
                }
                Err(e) => {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        "Fetching image metadata failed: ".to_string() + &e.to_string(),
                    )
                        .into_response());
                }
            };

            if partner.links.is_empty() {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Links cannot be empty".to_string(),
                )
                    .into_response());
            }

            for link in &partner.links {
                if link.name.is_empty() {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        "Link name cannot be empty".to_string(),
                    )
                        .into_response());
                }

                if link.value.is_empty() {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        "Link URL cannot be empty".to_string(),
                    )
                        .into_response());
                }

                if !link.value.starts_with("https://") {
                    return Ok((
                        StatusCode::BAD_REQUEST,
                        "Link URL must start with https://".to_string(),
                    )
                        .into_response());
                }
            }

            // Check user id
            let user_exists = sqlx::query!(
                "SELECT user_id FROM users WHERE user_id = $1",
                partner.user_id
            )
            .fetch_optional(&state.pool)
            .await
            .map_err(Error::new)?
            .is_some();

            if !user_exists {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "User does not exist".to_string(),
                )
                    .into_response());
            }

            // Check if partner already exists
            let partner_exists = sqlx::query!(
                "SELECT id FROM partners WHERE id = $1",
                partner.id
            )
            .fetch_optional(&state.pool)
            .await
            .map_err(Error::new)?
            .is_some();

            if partner_exists {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Partner already exists".to_string(),
                )
                    .into_response());
            }

            // Insert partner
            sqlx::query!(
                "INSERT INTO partners (id, name, image_type, short, links, type, user_id) VALUES ($1, $2, $3, $4, $5, $6, $7)",
                partner.id,
                partner.name,
                partner.image_type,
                partner.short,
                serde_json::to_value(partner.links).map_err(Error::new)?,
                partner.r#type,
                partner.user_id
            )
            .execute(&state.pool)
            .await
            .map_err(Error::new)?;
            
            Ok((StatusCode::NO_CONTENT, "").into_response())
        },
        PanelQuery::DeletePartner {
            login_token,
            partner_id,
        } => {
            let caps = super::auth::get_capabilities(&state.pool, &login_token)
                .await
                .map_err(Error::new)?;

            if !caps.contains(&Capability::PartnerManagement) {
                return Ok((
                    StatusCode::FORBIDDEN,
                    "You do not have permission to manage partners right now?".to_string(),
                )
                    .into_response());
            }

            // Check if partner exists
            let partner_exists = sqlx::query!(
                "SELECT id FROM partners WHERE id = $1",
                partner_id
            )
            .fetch_optional(&state.pool)
            .await
            .map_err(Error::new)?
            .is_some();

            if !partner_exists {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Partner does not exist".to_string(),
                )
                    .into_response());
            }

            // Ensure that image has been uploaded to CDN
            // Get cdn path from cdn_scope hashmap
            let Some(cdn_path) = crate::config::CONFIG.panel.cdn_scopes.get(&crate::config::CONFIG.panel.main_scope) else {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    "Main scope not found".to_string(),
                )
                    .into_response());
            };

            let rec = sqlx::query!(
                "SELECT image_type FROM partners WHERE id = $1",
                partner_id
            )
            .fetch_one(&state.pool)
            .await
            .map_err(Error::new)?;

            let path = format!("{}/partners/{}.{}", cdn_path.path, partner_id, rec.image_type);

            match std::fs::metadata(&path) {
                Ok(m) => {
                    if m.is_symlink() || m.is_file() {
                        // Delete the symlink
                        std::fs::remove_file(path).map_err(Error::new)?;
                    } else if m.is_dir() {
                        // Delete the directory
                        std::fs::remove_dir_all(path).map_err(Error::new)?;
                    }
                },
                Err(e) => {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        return Ok((
                            StatusCode::BAD_REQUEST,
                            "Fetching asset metadata failed due to unknown error: "
                                .to_string()
                                + &e.to_string(),
                        )
                            .into_response());
                    }
                }
            };

            sqlx::query!("DELETE FROM partners WHERE id = $1", partner_id)
                .execute(&state.pool)
                .await
                .map_err(Error::new)?;

            Ok((StatusCode::NO_CONTENT, "").into_response())
        }
    }
}
