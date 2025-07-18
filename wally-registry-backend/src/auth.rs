use std::{collections::HashMap, fmt};

use anyhow::{anyhow, format_err};
use constant_time_eq::constant_time_eq;
use libwally::{package_id::PackageId, package_index::PackageIndex};
use reqwest::{Client, StatusCode};
use rocket::{
    http::Status,
    request::{FromRequest, Outcome},
    Request, State,
};
use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::{config::Config, error::ApiErrorStatus};

#[derive(Deserialize, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "kebab-case")]
pub enum AuthMode {
    ApiKey(String),
    DoubleApiKey {
        read: Option<String>,
        write: String,
    },
    GithubOAuth {
        #[serde(rename = "client-id")]
        client_id: String,
        #[serde(rename = "client-secret")]
        client_secret: String,
    },
    GithubOAuthPrivate {
        #[serde(rename = "client-id")]
        client_id: String,
        #[serde(rename = "client-secret")]
        client_secret: String,
    },
    Unauthenticated,
}

#[derive(Deserialize)]
pub struct GithubInfo {
    login: String,
    id: u64,
}

impl GithubInfo {
    pub fn login(&self) -> &str {
        &self.login
    }

    pub fn id(&self) -> &u64 {
        &self.id
    }
}

#[derive(Deserialize)]
#[allow(unused)] // Variables are (currently) not accessed but ensure they are present during json parsing
struct ValidatedGithubApp {
    client_id: String,
}

#[derive(Deserialize)]
#[allow(unused)] // Variables are (currently) not accessed but ensure they are present during json parsing
struct ValidatedGithubInfo {
    id: u64,
    app: ValidatedGithubApp,
}

#[derive(Deserialize)]
struct GithubPermissionInfo {
    permission: String,
}

impl GithubPermissionInfo {
    pub fn permission(&self) -> &str {
        &self.permission
    }
}

impl fmt::Debug for AuthMode {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        match self {
            AuthMode::ApiKey(_) => write!(formatter, "API key"),
            AuthMode::DoubleApiKey { .. } => write!(formatter, "double API key"),
            AuthMode::GithubOAuth { .. } => write!(formatter, "Github OAuth"),
            AuthMode::GithubOAuthPrivate { .. } => write!(formatter, "Github OAuth (private)"),
            AuthMode::Unauthenticated => write!(formatter, "no authentication"),
        }
    }
}

fn match_api_key<T>(request: &Request<'_>, key: &str, result: T) -> Outcome<T, Error> {
    let input_api_key: String = match request.headers().get_one("authorization") {
        Some(key) if key.starts_with("Bearer ") => (key[6..].trim()).to_owned(),
        _ => {
            return format_err!("API key required")
                .status(Status::Unauthorized)
                .into();
        }
    };

    if constant_time_eq(key.as_bytes(), input_api_key.as_bytes()) {
        Outcome::Success(result)
    } else {
        format_err!("Invalid API key for read access")
            .status(Status::Unauthorized)
            .into()
    }
}

fn extract_github_owner_repo(url: &str) -> Option<(String, String)> {
    // Remove "https://" or "http://"
    let url = url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);

    // Remove trailing ".git" or "/"
    let url = url.trim_end_matches(".git").trim_end_matches('/');

    // Now expect: github.com/org/repo
    let parts: Vec<&str> = url.split('/').collect();
    if parts.len() >= 3 && parts[0] == "github.com" {
        let org = parts[1].to_string();
        let repo = parts[2].to_string();
        Some((org, repo))
    } else {
        None
    }
}

trait GithubAccessor {
    fn construct(info: GithubInfo) -> Self;
}

#[derive(PartialEq, Eq)]
enum IndexAccessPolicy {
    Optional,
    Required,
}

async fn verify_github<AccessType: GithubAccessor>(
    request: &Request<'_>,
    client_id: &str,
    client_secret: &str,
    index_access_policy: IndexAccessPolicy,
) -> Outcome<AccessType, Error> {
    let token: String = match request.headers().get_one("authorization") {
        Some(key) if key.starts_with("Bearer ") => (key[6..].trim()).to_owned(),
        _ => {
            return format_err!("Github auth required")
                .status(Status::Unauthorized)
                .into();
        }
    };

    let client = Client::new();
    let response = client
        .get("https://api.github.com/user")
        .header("accept", "application/json")
        .header("user-agent", "wally")
        .bearer_auth(&token)
        .send()
        .await;

    let github_info = match response {
        Err(err) => {
            return format_err!(err).status(Status::InternalServerError).into();
        }
        Ok(response) => match response.json::<GithubInfo>().await {
            Err(err) => {
                return format_err!("Github auth failed: {}", err)
                    .status(Status::Unauthorized)
                    .into();
            }
            Ok(github_info) => github_info,
        },
    };

    let mut body = HashMap::new();
    body.insert("access_token", &token);

    let response = client
        .post(format!(
            "https://api.github.com/applications/{}/token",
            client_id
        ))
        .header("accept", "application/json")
        .header("user-agent", "wally")
        .basic_auth(client_id, Some(client_secret))
        .json(&body)
        .send()
        .await;

    let validated_github_info = match response {
        Err(err) => {
            return format_err!(err).status(Status::InternalServerError).into();
        }
        Ok(response) => {
            // If a code 422 (unprocessable entity) is returned, it's a sign of
            // auth failure. Otherwise, we don't know what happened!
            // https://docs.github.com/en/rest/apps/oauth-applications#check-a-token--status-codes
            match response.status() {
                StatusCode::OK => response.json::<ValidatedGithubInfo>().await,
                StatusCode::UNPROCESSABLE_ENTITY => {
                    return anyhow!("GitHub auth was invalid")
                        .status(Status::Unauthorized)
                        .into();
                }
                status => {
                    return format_err!("Github auth failed because: {}", status)
                        .status(Status::UnprocessableEntity)
                        .into()
                }
            }
        }
    };

    if let Err(err) = validated_github_info {
        return format_err!("Github auth failed: {}", err)
            .status(Status::Unauthorized)
            .into()
    }

    if index_access_policy == IndexAccessPolicy::Required {
        let config = request
            .guard::<&State<Config>>()
            .await
            .expect("Failed to load config");

        let username = github_info.login();

        // These two lines will panic if the backend config isn't setup correctly
        let (owner, repo) = extract_github_owner_repo(config.index_url.as_str()).unwrap();
        let token = config.github_token.clone().unwrap();

        let response = client
            .get(format!(
                "https://api.github.com/repos/{owner}/{repo}/collaborators/{username}/permission"
            ))
            .header("accept", "application/json")
            .header("user-agent", "wally")
            .bearer_auth(token)
            .send()
            .await;

        let permission_info = match response {
            Err(err) => {
                return format_err!(err).status(Status::InternalServerError).into();
            }
            Ok(response) => match response.json::<GithubPermissionInfo>().await {
                Err(err) => {
                    return format_err!("Github auth failed: {}", err)
                        .status(Status::Unauthorized)
                        .into();
                }
                Ok(permission_info) => permission_info,
            },
        };

        match permission_info.permission() {
            "admin" | "write" | "read" => {}
            _ => {
                return anyhow!("GitHub auth was invalid")
                    .status(Status::Unauthorized)
                    .into();
            }
        }
    }

    Outcome::Success(AccessType::construct(github_info))
}


pub enum ReadAccess {
    Public,
    ApiKey,
    #[allow(dead_code)]
    Github(GithubInfo),
}

impl GithubAccessor for ReadAccess {
    fn construct(info: GithubInfo) -> Self {
        ReadAccess::Github(info)
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for ReadAccess {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Error> {
        let config = request
            .guard::<&State<Config>>()
            .await
            .expect("AuthMode was not configured");

        match &config.auth {
            AuthMode::Unauthenticated => Outcome::Success(ReadAccess::Public),
            AuthMode::GithubOAuth { .. } => Outcome::Success(ReadAccess::Public),
            AuthMode::GithubOAuthPrivate {
                client_id,
                client_secret,
            } => verify_github::<ReadAccess>(request, client_id, client_secret, IndexAccessPolicy::Required).await,
            AuthMode::ApiKey(key) => match_api_key(request, key, ReadAccess::ApiKey),
            AuthMode::DoubleApiKey { read, .. } => match read {
                None => Outcome::Success(ReadAccess::Public),
                Some(key) => match_api_key(request, key, ReadAccess::ApiKey),
            },
        }
    }
}

pub enum WriteAccess {
    ApiKey,
    Github(GithubInfo),
}

impl GithubAccessor for WriteAccess {
    fn construct(info: GithubInfo) -> Self {
        WriteAccess::Github(info)
    }
}

impl WriteAccess {
    pub fn can_write_package(
        &self,
        package_id: &PackageId,
        index: &PackageIndex,
    ) -> anyhow::Result<bool> {
        let scope = package_id.name().scope();

        let has_permission = match self {
            WriteAccess::ApiKey => true,
            WriteAccess::Github(github_info) => {
                match index.is_scope_owner(scope, github_info.id())? {
                    true => true,
                    // Only grant write access if the username matches the scope AND the scope has no existing owners
                    false => {
                        github_info.login().to_lowercase() == scope
                            && index.get_scope_owners(scope)?.is_empty()
                    }
                }
            }
        };

        Ok(has_permission)
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for WriteAccess {
    type Error = Error;

    async fn from_request(request: &'r Request<'_>) -> Outcome<Self, Error> {
        let config = request
            .guard::<&State<Config>>()
            .await
            .expect("AuthMode was not configured");

        match &config.auth {
            AuthMode::Unauthenticated => format_err!("Invalid API key for write access")
                .status(Status::Unauthorized)
                .into(),
            AuthMode::ApiKey(key) => match_api_key(request, key, WriteAccess::ApiKey),
            AuthMode::DoubleApiKey { write, .. } => {
                match_api_key(request, write, WriteAccess::ApiKey)
            }
            AuthMode::GithubOAuth {
                client_id,
                client_secret,
            } => verify_github::<WriteAccess>(request, client_id, client_secret, IndexAccessPolicy::Optional).await,
            AuthMode::GithubOAuthPrivate {
                client_id,
                client_secret,
            } => verify_github::<WriteAccess>(request, client_id, client_secret, IndexAccessPolicy::Required).await,
        }
    }
}
