use axum::http::request::Parts;
use base64::prelude::*;
use chrono::Duration;
use cookie::SameSite;
use lazy_static::lazy_static;
use libsql::{de, params, Connection};
use sha2::{Digest, Sha256};
use tower_cookies::{Cookie, Cookies};
use trailbase_sqlite::{query_one_row, query_row};

use crate::auth::user::{DbUser, User};
use crate::auth::AuthError;
use crate::constants::{
  COOKIE_AUTH_TOKEN, COOKIE_OAUTH_STATE, COOKIE_REFRESH_TOKEN, SESSION_TABLE, USER_TABLE,
};
use crate::AppState;

pub(crate) fn validate_redirects(
  state: &AppState,
  first: &Option<String>,
  second: &Option<String>,
) -> Result<Option<String>, AuthError> {
  let dev = state.dev_mode();
  let site = state.access_config(|c| c.server.site_url.clone());

  let valid = |redirect: &String| -> bool {
    if redirect.starts_with("/") {
      return true;
    }
    if dev && redirect.starts_with("http://localhost") {
      return true;
    }

    // TODO: add a configurable white list.
    if let Some(site) = site {
      return redirect.starts_with(&site);
    }
    return false;
  };

  #[allow(clippy::manual_flatten)]
  for r in [first, second] {
    if let Some(ref r) = r {
      if valid(r) {
        return Ok(Some(r.to_owned()));
      }
      return Err(AuthError::BadRequest("Invalid redirect"));
    }
  }

  return Ok(None);
}

pub(crate) fn new_cookie(
  key: &'static str,
  value: String,
  ttl: Duration,
  dev: bool,
) -> Cookie<'static> {
  return Cookie::build((key, value))
    .path("/")
    // Not available to client-side JS.
    .http_only(true)
    // Only send cookie over HTTPs.
    .secure(!dev)
    // Only include cookie if request originates from origin site.
    .same_site(if dev { SameSite::Lax } else { SameSite::Strict })
    .max_age(cookie::time::Duration::seconds(ttl.num_seconds()))
    .build();
}

pub(crate) fn new_cookie_opts(
  key: &'static str,
  value: String,
  ttl: Duration,
  tls_only: bool,
  same_site: bool,
) -> Cookie<'static> {
  return Cookie::build((key, value))
    .path("/")
    // Not available to client-side JS.
    .http_only(true)
    // Only send cookie over HTTPs.
    .secure(tls_only)
    // Only include cookie if request originates from origin site.
    .same_site(if same_site {
      SameSite::Strict
    } else {
      SameSite::Lax
    })
    .max_age(cookie::time::Duration::seconds(ttl.num_seconds()))
    .build();
}

/// Removes cookie with the given `key`.
///
/// NOTE: Removing a cookie from the jar doesn't reliably force the browser to remove the cookie,
/// thus override them.
pub(crate) fn remove_cookie(cookies: &Cookies, key: &'static str) {
  if cookies.get(key).is_some() {
    cookies.add(new_cookie(key, "".to_string(), Duration::seconds(1), false));
  }
}

pub(crate) fn remove_all_cookies(cookies: &Cookies) {
  for cookie in [COOKIE_AUTH_TOKEN, COOKIE_REFRESH_TOKEN, COOKIE_OAUTH_STATE] {
    remove_cookie(cookies, cookie);
  }
}

#[cfg(test)]
pub(crate) fn extract_cookies_from_parts(parts: &mut Parts) -> Result<Cookies, AuthError> {
  let cookies = Cookies::default();

  for ref header in parts.headers.get_all(axum::http::header::COOKIE) {
    cookies.add(Cookie::parse(header.to_str().unwrap().to_string()).unwrap());
  }

  return Ok(cookies);
}

#[cfg(not(test))]
pub(crate) fn extract_cookies_from_parts(parts: &mut Parts) -> Result<Cookies, AuthError> {
  if let Some(cookies) = parts.extensions.get::<Cookies>() {
    return Ok(cookies.clone());
  };
  log::error!("Failed to get Cookies");
  return Err(AuthError::Internal("cookie error".into()));
}

pub async fn user_by_email(state: &AppState, email: &str) -> Result<DbUser, AuthError> {
  return get_user_by_email(state.user_conn(), email).await;
}

pub async fn get_user_by_email(user_conn: &Connection, email: &str) -> Result<DbUser, AuthError> {
  lazy_static! {
    static ref QUERY: String = format!("SELECT * FROM {USER_TABLE} WHERE email = $1");
  };
  let row = query_one_row(user_conn, &QUERY, params!(email))
    .await
    .map_err(|_err| AuthError::UnauthorizedExt("user not found by email".into()))?;

  return de::from_row(&row).map_err(|_err| AuthError::UnauthorizedExt("invalid user".into()));
}

pub async fn user_by_id(state: &AppState, id: &uuid::Uuid) -> Result<DbUser, AuthError> {
  return get_user_by_id(state.user_conn(), id).await;
}

pub(crate) async fn get_user_by_id(
  user_conn: &Connection,
  id: &uuid::Uuid,
) -> Result<DbUser, AuthError> {
  lazy_static! {
    static ref QUERY: String = format!("SELECT * FROM {USER_TABLE} WHERE id = $1");
  };
  let row = query_one_row(user_conn, &QUERY, params!(id.into_bytes()))
    .await
    .map_err(|_err| AuthError::UnauthorizedExt("User not found by id".into()))?;

  return de::from_row(&row).map_err(|_err| AuthError::UnauthorizedExt("Invalid user".into()));
}

pub async fn user_exists(state: &AppState, email: &str) -> Result<bool, libsql::Error> {
  lazy_static! {
    static ref EXISTS_QUERY: String =
      format!("SELECT EXISTS(SELECT 1 FROM '{USER_TABLE}' WHERE email = $1)");
  };
  let row = query_one_row(state.user_conn(), &EXISTS_QUERY, params!(email)).await?;
  return row.get::<bool>(0);
}

pub(crate) async fn is_admin(state: &AppState, user: &User) -> bool {
  let Ok(Some(row)) = query_row(
    state.user_conn(),
    &format!("SELECT admin FROM {USER_TABLE} WHERE id = $1"),
    params!(user.uuid.as_bytes().to_vec()),
  )
  .await
  else {
    return false;
  };

  return row.get::<bool>(0).unwrap_or(false);
}

pub(crate) async fn delete_all_sessions_for_user(
  state: &AppState,
  user_id: uuid::Uuid,
) -> Result<u64, libsql::Error> {
  lazy_static! {
    static ref QUERY: String = format!("DELETE FROM '{SESSION_TABLE}' WHERE user = $1");
  };

  return state
    .user_conn()
    .execute(&QUERY, [user_id.into_bytes().to_vec()])
    .await;
}

pub(crate) async fn delete_session(
  state: &AppState,
  refresh_token: String,
) -> Result<u64, libsql::Error> {
  lazy_static! {
    static ref QUERY: String = format!("DELETE FROM '{SESSION_TABLE}' WHERE refresh_token = $1");
  };

  return state
    .user_conn()
    .execute(&QUERY, params!(refresh_token))
    .await;
}

/// Derives the code challenge given the verifier as base64UrlNoPad(sha256([codeVerifier])).
///
/// NOTE: We could also use oauth2::PkceCodeChallenge.
pub(crate) fn derive_pkce_code_challenge(pkce_code_verifier: &str) -> String {
  let mut sha = Sha256::new();
  sha.update(pkce_code_verifier);
  // NOTE: This is NO_PAD as per the spec.
  return BASE64_URL_SAFE_NO_PAD.encode(sha.finalize());
}
