// TODO: this module is functional, but it needs a good refactor for proper error handling.

use axum::body::{Body, StreamBody};
use axum::extract::{BodyStream, FromRequest, Path, Query, RequestParts, TypedHeader};
use axum::headers::authorization::Basic;
use axum::headers::{Authorization, HeaderName};
use axum::http::HeaderValue;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, head, post, put};
use axum::{async_trait, Extension, Router};
use futures::StreamExt;
use hyper::{HeaderMap, StatusCode};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::db::bots::NewBotVersion;
use crate::util::gen_alphanumeric;
use crate::{db, DatabaseConnection, GlobalConfig};

use crate::db::users::{authenticate_user, Credentials, User};

pub fn registry_service() -> Router {
    Router::new()
        // The docker API requires this trailing slash
        .nest("/v2/", registry_api_v2())
}

fn registry_api_v2() -> Router {
    Router::new()
        .route("/", get(get_root))
        .route(
            "/:name/manifests/:reference",
            get(get_manifest).put(put_manifest),
        )
        .route(
            "/:name/blobs/:digest",
            head(check_blob_exists).get(get_blob),
        )
        .route("/:name/blobs/uploads/", post(create_upload))
        .route(
            "/:name/blobs/uploads/:uuid",
            put(put_upload).patch(patch_upload),
        )
}

const ADMIN_USERNAME: &str = "admin";

type AuthorizationHeader = TypedHeader<Authorization<Basic>>;

enum RegistryAuth {
    User(User),
    Admin,
}

enum RegistryAuthError {
    NoAuthHeader,
    InvalidCredentials,
}

impl IntoResponse for RegistryAuthError {
    fn into_response(self) -> Response {
        RegistryError::Unauthorized.into_response()
    }
}

#[async_trait]
impl<B> FromRequest<B> for RegistryAuth
where
    B: Send,
{
    type Rejection = RegistryAuthError;

    async fn from_request(req: &mut RequestParts<B>) -> Result<Self, Self::Rejection> {
        let TypedHeader(Authorization(basic)) = AuthorizationHeader::from_request(req)
            .await
            .map_err(|_| RegistryAuthError::NoAuthHeader)?;

        // TODO: Into<Credentials> would be nice
        let credentials = Credentials {
            username: basic.username(),
            password: basic.password(),
        };

        let Extension(config) = Extension::<Arc<GlobalConfig>>::from_request(req)
            .await
            .unwrap();

        if credentials.username == ADMIN_USERNAME {
            if credentials.password == config.registry_admin_password {
                Ok(RegistryAuth::Admin)
            } else {
                Err(RegistryAuthError::InvalidCredentials)
            }
        } else {
            let mut db_conn = DatabaseConnection::from_request(req).await.unwrap();
            authenticate_user(&credentials, &mut db_conn)
                .map(RegistryAuth::User)
                .ok_or(RegistryAuthError::InvalidCredentials)
        }
    }
}

// Since async file io just calls spawn_blocking internally, it does not really make sense
// to make this an async function
fn file_sha256_digest(path: &std::path::Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let _n = std::io::copy(&mut file, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// Get the index of the last byte in a file
async fn last_byte_pos(file: &tokio::fs::File) -> std::io::Result<u64> {
    let n_bytes = file.metadata().await?.len();
    let pos = if n_bytes == 0 { 0 } else { n_bytes - 1 };
    Ok(pos)
}

async fn get_root(_auth: RegistryAuth) -> impl IntoResponse {
    // root should return 200 OK to confirm api compliance
    Response::builder()
        .status(StatusCode::OK)
        .header("Docker-Distribution-API-Version", "registry/2.0")
        .body(Body::empty())
        .unwrap()
}

async fn check_blob_exists(
    mut db_conn: DatabaseConnection,
    auth: RegistryAuth,
    Path((repository_name, raw_digest)): Path<(String, String)>,
    Extension(config): Extension<Arc<GlobalConfig>>,
) -> Result<impl IntoResponse, (StatusCode, HeaderMap)> {
    check_access(&repository_name, &auth, &mut db_conn).map_err(|err| err.into_headers())?;

    let digest = raw_digest.strip_prefix("sha256:").unwrap();
    let blob_path = PathBuf::from(&config.registry_directory)
        .join("sha256")
        .join(&digest);
    if blob_path.exists() {
        let metadata = std::fs::metadata(&blob_path).unwrap();
        Ok((StatusCode::OK, [("Content-Length", metadata.len())]))
    } else {
        Err(RegistryError::BlobUnknown.into_headers())
    }
}

async fn get_blob(
    mut db_conn: DatabaseConnection,
    auth: RegistryAuth,
    Path((repository_name, raw_digest)): Path<(String, String)>,
    Extension(config): Extension<Arc<GlobalConfig>>,
) -> Result<impl IntoResponse, RegistryError> {
    check_access(&repository_name, &auth, &mut db_conn)?;

    let digest = raw_digest.strip_prefix("sha256:").unwrap();
    let blob_path = PathBuf::from(&config.registry_directory)
        .join("sha256")
        .join(&digest);
    if !blob_path.exists() {
        return Err(RegistryError::BlobUnknown);
    }
    let file = tokio::fs::File::open(&blob_path).await.unwrap();
    let reader_stream = ReaderStream::new(file);
    let stream_body = StreamBody::new(reader_stream);
    Ok(stream_body)
}

async fn create_upload(
    mut db_conn: DatabaseConnection,
    auth: RegistryAuth,
    Path(repository_name): Path<String>,
    Extension(config): Extension<Arc<GlobalConfig>>,
) -> Result<impl IntoResponse, RegistryError> {
    check_access(&repository_name, &auth, &mut db_conn)?;

    let uuid = gen_alphanumeric(16);
    tokio::fs::File::create(
        PathBuf::from(&config.registry_directory)
            .join("uploads")
            .join(&uuid),
    )
    .await
    .unwrap();

    Ok(Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(
            "Location",
            format!("/v2/{}/blobs/uploads/{}", repository_name, uuid),
        )
        .header("Docker-Upload-UUID", uuid)
        .header("Range", "bytes=0-0")
        .body(Body::empty())
        .unwrap())
}

async fn patch_upload(
    mut db_conn: DatabaseConnection,
    auth: RegistryAuth,
    Path((repository_name, uuid)): Path<(String, String)>,
    mut stream: BodyStream,
    Extension(config): Extension<Arc<GlobalConfig>>,
) -> Result<impl IntoResponse, RegistryError> {
    check_access(&repository_name, &auth, &mut db_conn)?;

    // TODO: support content range header in request
    let upload_path = PathBuf::from(&config.registry_directory)
        .join("uploads")
        .join(&uuid);
    let mut file = tokio::fs::OpenOptions::new()
        .read(false)
        .write(true)
        .append(true)
        .create(false)
        .open(upload_path)
        .await
        .unwrap();
    while let Some(Ok(chunk)) = stream.next().await {
        file.write_all(&chunk).await.unwrap();
    }

    let last_byte = last_byte_pos(&file).await.unwrap();

    Ok(Response::builder()
        .status(StatusCode::ACCEPTED)
        .header(
            "Location",
            format!("/v2/{}/blobs/uploads/{}", repository_name, uuid),
        )
        .header("Docker-Upload-UUID", uuid)
        // range indicating current progress of the upload
        .header("Range", format!("0-{}", last_byte))
        .body(Body::empty())
        .unwrap())
}

use serde::Deserialize;
#[derive(Deserialize)]
struct UploadParams {
    digest: String,
}

async fn put_upload(
    mut db_conn: DatabaseConnection,
    auth: RegistryAuth,
    Path((repository_name, uuid)): Path<(String, String)>,
    Query(params): Query<UploadParams>,
    mut stream: BodyStream,
    Extension(config): Extension<Arc<GlobalConfig>>,
) -> Result<impl IntoResponse, RegistryError> {
    check_access(&repository_name, &auth, &mut db_conn)?;

    let upload_path = PathBuf::from(&config.registry_directory)
        .join("uploads")
        .join(&uuid);
    let mut file = tokio::fs::OpenOptions::new()
        .read(false)
        .write(true)
        .append(true)
        .create(false)
        .open(&upload_path)
        .await
        .unwrap();

    let range_begin = last_byte_pos(&file).await.unwrap();
    while let Some(Ok(chunk)) = stream.next().await {
        file.write_all(&chunk).await.unwrap();
    }
    let range_end = last_byte_pos(&file).await.unwrap();
    // Close the file to ensure all data has been flushed to the kernel.
    // If we don't do this, calculating the checksum can fail.
    std::mem::drop(file);

    let expected_digest = params.digest.strip_prefix("sha256:").unwrap();
    let digest = file_sha256_digest(&upload_path).unwrap();
    if digest != expected_digest {
        // TODO: return a docker error body
        return Err(RegistryError::DigestInvalid);
    }

    let target_path = PathBuf::from(&config.registry_directory)
        .join("sha256")
        .join(&digest);
    tokio::fs::rename(&upload_path, &target_path).await.unwrap();

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .header(
            "Location",
            format!("/v2/{}/blobs/{}", repository_name, digest),
        )
        .header("Docker-Upload-UUID", uuid)
        // content range for bytes that were in the body of this request
        .header("Content-Range", format!("{}-{}", range_begin, range_end))
        .header("Docker-Content-Digest", params.digest)
        .body(Body::empty())
        .unwrap())
}

async fn get_manifest(
    mut db_conn: DatabaseConnection,
    auth: RegistryAuth,
    Path((repository_name, reference)): Path<(String, String)>,
    Extension(config): Extension<Arc<GlobalConfig>>,
) -> Result<impl IntoResponse, RegistryError> {
    check_access(&repository_name, &auth, &mut db_conn)?;

    let manifest_path = PathBuf::from(&config.registry_directory)
        .join("manifests")
        .join(&repository_name)
        .join(&reference)
        .with_extension("json");
    let data = tokio::fs::read(&manifest_path).await.unwrap();

    let manifest: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&data).unwrap();
    let media_type = manifest.get("mediaType").unwrap().as_str().unwrap();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", media_type)
        .body(axum::body::Full::from(data))
        .unwrap())
}

async fn put_manifest(
    mut db_conn: DatabaseConnection,
    auth: RegistryAuth,
    Path((repository_name, reference)): Path<(String, String)>,
    mut stream: BodyStream,
    Extension(config): Extension<Arc<GlobalConfig>>,
) -> Result<impl IntoResponse, RegistryError> {
    let bot = check_access(&repository_name, &auth, &mut db_conn)?;

    let repository_dir = PathBuf::from(&config.registry_directory)
        .join("manifests")
        .join(&repository_name);

    tokio::fs::create_dir_all(&repository_dir).await.unwrap();

    let mut hasher = Sha256::new();
    let manifest_path = repository_dir.join(&reference).with_extension("json");
    {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&manifest_path)
            .await
            .unwrap();
        while let Some(Ok(chunk)) = stream.next().await {
            hasher.update(&chunk);
            file.write_all(&chunk).await.unwrap();
        }
    }
    let digest = hasher.finalize();
    // TODO: store content-adressable manifests separately
    let content_digest = format!("sha256:{:x}", digest);
    let digest_path = repository_dir.join(&content_digest).with_extension("json");
    tokio::fs::copy(manifest_path, digest_path).await.unwrap();

    // Register the new image as a bot version
    // TODO: how should tags be handled?
    let new_version = NewBotVersion {
        bot_id: Some(bot.id),
        code_bundle_path: None,
        container_digest: Some(&content_digest),
    };
    let version = db::bots::create_bot_version(&new_version, &mut db_conn)
        .expect("could not save bot version");
    db::bots::set_active_version(bot.id, Some(version.id), &mut db_conn)
        .expect("could not update bot version");

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .header(
            "Location",
            format!("/v2/{}/manifests/{}", repository_name, reference),
        )
        .header("Docker-Content-Digest", content_digest)
        .body(Body::empty())
        .unwrap())
}

/// Ensure that the accessed repository exists
/// and the user is allowed to access it.
/// Returns the associated bot.
fn check_access(
    repository_name: &str,
    auth: &RegistryAuth,
    db_conn: &mut DatabaseConnection,
) -> Result<db::bots::Bot, RegistryError> {
    use diesel::OptionalExtension;

    // TODO: it would be nice to provide the found repository
    // to the route handlers
    let bot = db::bots::find_bot_by_name(repository_name, db_conn)
        .optional()
        .expect("could not run query")
        // TODO: return an error message here
        .ok_or(RegistryError::NameUnknown)?;

    match &auth {
        RegistryAuth::Admin => Ok(bot),
        RegistryAuth::User(user) => {
            if bot.owner_id == Some(user.id) {
                Ok(bot)
            } else {
                Err(RegistryError::Denied)
            }
        }
    }
}

enum RegistryError {
    Denied,
    Unauthorized,

    DigestInvalid,

    BlobUnknown,
    NameUnknown,
}

impl RegistryError {
    fn into_headers(self) -> (StatusCode, HeaderMap) {
        let raw = self.into_raw();
        (raw.status_code, raw.headers)
    }

    fn into_raw(self) -> RawRegistryError {
        match self {
            RegistryError::Unauthorized => RawRegistryError {
                status_code: StatusCode::UNAUTHORIZED,
                error_code: "UNAUTHORIZED",
                message: "Authenticate to continue",
                headers: HeaderMap::from_iter([(
                    HeaderName::from_static("www-authenticate"),
                    HeaderValue::from_static("Basic"),
                )]),
            },
            RegistryError::Denied => RawRegistryError {
                status_code: StatusCode::FORBIDDEN,
                error_code: "DENIED",
                message: "Access denied",
                headers: HeaderMap::new(),
            },
            RegistryError::BlobUnknown => RawRegistryError {
                status_code: StatusCode::FORBIDDEN,
                error_code: "BLOB_UNKNOWN",
                message: "Blob does not exist",
                headers: HeaderMap::new(),
            },
            RegistryError::NameUnknown => RawRegistryError {
                status_code: StatusCode::NOT_FOUND,
                error_code: "NAME_UNKNOWN",
                message: "Repository does not exist",
                headers: HeaderMap::new(),
            },
            RegistryError::DigestInvalid => RawRegistryError {
                status_code: StatusCode::UNPROCESSABLE_ENTITY,
                error_code: "DIGEST_INVALID",
                message: "Layer digest did not match provided value",
                headers: HeaderMap::new(),
            },
        }
    }
}

impl IntoResponse for RegistryError {
    fn into_response(self) -> Response {
        self.into_raw().into_response()
    }
}

pub struct RawRegistryError {
    status_code: StatusCode,
    error_code: &'static str,
    message: &'static str,
    headers: HeaderMap,
    // currently not used
    // detail: serde_json::Value,
}

impl IntoResponse for RawRegistryError {
    fn into_response(self) -> Response {
        let json_body = json!({
            "errors": [{
                "code": self.error_code,
                "message": self.message,
                "detail": serde_json::Value::Null,
            }],
        });

        let body = serde_json::to_vec(&json_body).unwrap();

        (self.status_code, self.headers, body).into_response()
    }
}
