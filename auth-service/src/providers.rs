//! The three supported identity providers: endpoints, authorize URL and profile fetching.
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::config::ProviderConfig;

/// Fixed order, as listed by `/api/providers`.
pub const PROVIDERS: [&str; 3] = ["google", "github", "microsoft"];

pub struct Profile {
    pub subject: String,
    pub email: Option<String>,
    pub email_verified: bool,
    pub name: Option<String>,
}

struct Endpoints {
    auth: &'static str,
    token: &'static str,
    userinfo: &'static str,
    scope: &'static str,
}

fn endpoints(provider: &str) -> Option<Endpoints> {
    Some(match provider {
        "google" => Endpoints {
            auth: "https://accounts.google.com/o/oauth2/v2/auth",
            token: "https://oauth2.googleapis.com/token",
            userinfo: "https://openidconnect.googleapis.com/v1/userinfo",
            scope: "openid email profile",
        },
        "github" => Endpoints {
            auth: "https://github.com/login/oauth/authorize",
            token: "https://github.com/login/oauth/access_token",
            userinfo: "https://api.github.com/user",
            scope: "read:user user:email",
        },
        "microsoft" => Endpoints {
            auth: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize",
            token: "https://login.microsoftonline.com/common/oauth2/v2.0/token",
            userinfo: "https://graph.microsoft.com/oidc/userinfo",
            scope: "openid email profile",
        },
        _ => return None,
    })
}

const GITHUB_EMAILS: &str = "https://api.github.com/user/emails";

/// The provider's authorize URL with state and the S256 PKCE challenge, or None for an unknown provider.
pub fn authorize_url(
    provider: &str,
    pc: &ProviderConfig,
    redirect_uri: &str,
    state: &str,
    code_challenge: &str,
) -> Option<String> {
    let e = endpoints(provider)?;
    let mut u = Url::parse(pc.auth_url.as_deref().unwrap_or(e.auth)).ok()?;
    u.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", &pc.client_id),
        ("redirect_uri", redirect_uri),
        ("scope", e.scope),
        ("state", state),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256"),
    ]);
    Some(u.into())
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct GithubEmail {
    email: String,
    #[serde(default)]
    primary: bool,
    #[serde(default)]
    verified: bool,
}

/// Exchanges the code and reads the profile. The error is a short reason safe to log (no codes or tokens).
pub async fn fetch_profile(
    http: &reqwest::Client,
    provider: &str,
    pc: &ProviderConfig,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<Profile, String> {
    let e = endpoints(provider).ok_or("unknown provider")?;
    let status_err = |what: &str, s: reqwest::StatusCode| format!("{what} returned {}", s.as_u16());
    let send_err = |what: &str, _: reqwest::Error| format!("{what} request failed");

    let resp = http
        .post(pc.token_url.as_deref().unwrap_or(e.token))
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", &pc.client_id),
            ("client_secret", &pc.client_secret),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .map_err(|x| send_err("token", x))?;
    if !resp.status().is_success() {
        return Err(status_err("token", resp.status()));
    }
    let token = resp
        .json::<TokenResponse>()
        .await
        .ok()
        .and_then(|t| t.access_token)
        .ok_or("token response had no access_token")?;

    let get = |url: String| {
        http.get(url)
            .bearer_auth(&token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "me-auth-service")
    };
    let resp = get(pc.userinfo_url.clone().unwrap_or_else(|| e.userinfo.into()))
        .send()
        .await
        .map_err(|x| send_err("userinfo", x))?;
    if !resp.status().is_success() {
        return Err(status_err("userinfo", resp.status()));
    }
    let info: Value = resp.json().await.map_err(|_| "userinfo was not JSON")?;
    let text = |k: &str| {
        info[k]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };

    match provider {
        "github" => {
            let subject = info["id"]
                .as_u64()
                .ok_or("github profile had no id")?
                .to_string();
            let resp = get(pc
                .emails_url
                .clone()
                .unwrap_or_else(|| GITHUB_EMAILS.into()))
            .send()
            .await
            .map_err(|x| send_err("emails", x))?;
            if !resp.status().is_success() {
                return Err(status_err("emails", resp.status()));
            }
            let emails: Vec<GithubEmail> = resp.json().await.map_err(|_| "emails was not JSON")?;
            // Only the primary address counts; its own `verified` flag decides.
            let primary = emails.into_iter().find(|m| m.primary);
            Ok(Profile {
                subject,
                email_verified: primary.as_ref().is_some_and(|m| m.verified),
                email: primary.map(|m| m.email),
                name: text("name").or_else(|| text("login")),
            })
        }
        _ => Ok(Profile {
            subject: text("sub").ok_or("profile had no sub")?,
            email: text("email"),
            // ponytail: Microsoft does not assert email verification; link from the account page
            email_verified: provider == "google" && info["email_verified"].as_bool() == Some(true),
            name: text("name"),
        }),
    }
}
