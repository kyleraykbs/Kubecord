//! Signing in with an email (or phone number) and password, as Discord's web
//! client does it: `POST /auth/login`, then, for accounts with two-factor
//! authentication, a code against the ticket it returns
//! (`/auth/mfa/totp`, `/auth/mfa/backup`, or `/auth/mfa/sms` after
//! `/auth/mfa/sms/send`).
//!
//! Requests carry no `authorization`; they carry the browser headers and
//! super properties every other request does, and the `X-Fingerprint` the
//! logged-out web client gets from `GET /experiments`.
//!
//! The password and codes never reach a log or an error: nothing here logs a
//! request or response body, [`Password`] and [`MfaChallenge`] print
//! redacted, request bodies are wiped once sent, and errors are built from
//! Discord's own messages rather than from the body text. Responses are
//! interpreted by [`parse_response`], which is pure, so every branch is
//! tested with canned JSON.

use std::fmt;
use std::sync::Arc;

use kc_core::Token;
use parking_lot::Mutex;
use reqwest::StatusCode;
use serde_json::{Value, json};
use zeroize::{Zeroize, Zeroizing};

use crate::API_BASE;
use crate::client::{CaptchaAnswer, browser_headers};
use crate::error::{Error, Result};

/// The page the login requests come from.
const LOGIN_REFERER: &str = "https://discord.com/login";

/// A password (or two-factor code) held only as long as it is needed: wiped
/// from memory when dropped, never shown by `Debug`.
#[derive(Clone)]
pub struct Password(String);

impl Password {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for Password {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// Discord wants a second factor: which kinds this account offers, and the
/// ticket the code is checked against (short-lived).
#[derive(Clone, PartialEq, Eq)]
pub struct MfaChallenge {
    pub ticket: String,
    pub totp: bool,
    pub sms: bool,
    pub backup: bool,
    /// A security key: not supported here (it needs a browser).
    pub webauthn: bool,
}

impl fmt::Debug for MfaChallenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MfaChallenge")
            .field("ticket", &"<redacted>")
            .field("totp", &self.totp)
            .field("sms", &self.sms)
            .field("backup", &self.backup)
            .field("webauthn", &self.webauthn)
            .finish()
    }
}

/// How a two-factor code is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MfaMethod {
    /// A 6-digit code from an authenticator app.
    Totp,
    /// One of the account's 8-character backup codes.
    Backup,
    /// A code Discord sent by text message.
    Sms,
}

impl MfaMethod {
    pub fn path(self) -> &'static str {
        match self {
            Self::Totp => "/auth/mfa/totp",
            Self::Backup => "/auth/mfa/backup",
            Self::Sms => "/auth/mfa/sms",
        }
    }
}

/// Why Discord turned a login or a code down.
#[derive(Clone, Debug, PartialEq)]
pub enum Rejection {
    /// Discord's messages for the fields it objected to.
    Fields {
        login: Option<String>,
        password: Option<String>,
        code: Option<String>,
    },
    /// `ACCOUNT_LOGIN_VERIFICATION_EMAIL`: a login from a new place; Discord
    /// emailed a link to approve it.
    NewLocation,
    /// The account is disabled or scheduled for deletion (Discord's text).
    Disabled(String),
    /// Too many attempts: wait this many seconds.
    RateLimited { retry_after: f64 },
    /// The two-factor ticket is no longer valid (it expires after a few
    /// minutes): the password must be entered again.
    TicketExpired,
    /// The two-factor code was wrong.
    InvalidCode,
    /// Anything else, as Discord put it (never the raw body).
    Other(String),
}

/// What a login or code request came to.
#[derive(Clone, Debug, PartialEq)]
pub enum AuthResponse {
    Token(Token),
    Mfa(MfaChallenge),
    Captcha {
        service: String,
        sitekey: String,
        rqdata: Option<String>,
        rqtoken: Option<String>,
    },
    Rejected(Rejection),
}

/// The body of `POST /auth/login`, wiped when dropped.
pub fn login_body(
    login: &str,
    password: &Password,
    captcha: Option<&CaptchaAnswer>,
) -> Zeroizing<Vec<u8>> {
    let mut body = json!({
        "login": login,
        "password": password.expose(),
        "undelete": false,
        "login_source": null,
        "gift_code_sku_id": null,
    });
    add_captcha(&mut body, captcha);
    to_bytes(body)
}

/// The body of a two-factor code check.
pub fn mfa_body(
    code: &Password,
    ticket: &str,
    captcha: Option<&CaptchaAnswer>,
) -> Zeroizing<Vec<u8>> {
    let mut body = json!({
        "code": code.expose(),
        "ticket": ticket,
        "login_source": null,
        "gift_code_sku_id": null,
    });
    add_captcha(&mut body, captcha);
    to_bytes(body)
}

/// The body of `POST /auth/mfa/sms/send`.
pub fn sms_send_body(ticket: &str, captcha: Option<&CaptchaAnswer>) -> Zeroizing<Vec<u8>> {
    let mut body = json!({ "ticket": ticket });
    add_captcha(&mut body, captcha);
    to_bytes(body)
}

fn add_captcha(body: &mut Value, captcha: Option<&CaptchaAnswer>) {
    if let Some(captcha) = captcha {
        body["captcha_key"] = json!(captcha.key);
        if let Some(rqtoken) = &captcha.rqtoken {
            body["captcha_rqtoken"] = json!(rqtoken);
        }
    }
}

/// Serialises a body and wipes the strings in the `Value` it came from.
fn to_bytes(mut body: Value) -> Zeroizing<Vec<u8>> {
    let bytes = Zeroizing::new(serde_json::to_vec(&body).unwrap_or_default());
    wipe(&mut body);
    bytes
}

fn wipe(value: &mut Value) {
    match value {
        Value::String(text) => text.zeroize(),
        Value::Array(items) => items.iter_mut().for_each(wipe),
        Value::Object(map) => map.values_mut().for_each(wipe),
        _ => {}
    }
}

/// Reads a response to `/auth/login` or a code check. `retry_after_header`
/// is the `Retry-After` header, if any.
pub fn parse_response(
    status: StatusCode,
    retry_after_header: Option<f64>,
    body: &[u8],
) -> AuthResponse {
    let value = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);

    if status.is_success() {
        if let Some(token) = text("token").filter(|token| !token.is_empty()) {
            return AuthResponse::Token(Token::new(token));
        }
        if value.get("mfa").and_then(Value::as_bool) == Some(true)
            && let Some(ticket) = text("ticket")
        {
            let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
            return AuthResponse::Mfa(MfaChallenge {
                ticket,
                totp: flag("totp"),
                sms: flag("sms"),
                backup: flag("backup"),
                // `webauthn` is a JSON string of options when offered.
                webauthn: value.get("webauthn").is_some_and(|value| !value.is_null()),
            });
        }
        return AuthResponse::Rejected(Rejection::Other(
            "Discord accepted the login but sent no token. Log in with a token instead.".into(),
        ));
    }

    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after = retry_after_header
            .or_else(|| value.get("retry_after").and_then(Value::as_f64))
            .unwrap_or(5.0);
        return AuthResponse::Rejected(Rejection::RateLimited { retry_after });
    }

    if value.get("captcha_key").is_some() {
        if let Some(sitekey) = text("captcha_sitekey").filter(|key| !key.is_empty()) {
            return AuthResponse::Captcha {
                service: text("captcha_service").unwrap_or_else(|| "hcaptcha".to_owned()),
                sitekey,
                rqdata: text("captcha_rqdata"),
                rqtoken: text("captcha_rqtoken"),
            };
        }
        return AuthResponse::Rejected(Rejection::Other(
            "Discord did not accept the captcha. Try again.".into(),
        ));
    }

    let code = value.get("code").and_then(Value::as_i64);
    let message = text("message").unwrap_or_default();
    // 60006: "Invalid two-factor auth ticket"; 60008: "Invalid two-factor
    // code".
    if code == Some(60006) {
        return AuthResponse::Rejected(Rejection::TicketExpired);
    }
    if code == Some(60008) {
        return AuthResponse::Rejected(Rejection::InvalidCode);
    }

    let errors = value.get("errors");
    let field = |name: &str| -> Option<(String, String)> {
        let first = errors?.get(name)?.get("_errors")?.get(0)?;
        Some((
            first.get("code")?.as_str().unwrap_or_default().to_owned(),
            first
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        ))
    };
    let (login, password, code_field, ticket) = (
        field("login"),
        field("password"),
        field("code"),
        field("ticket"),
    );
    let all = [&login, &password, &code_field, &ticket];
    let has = |wanted: &str| {
        all.iter()
            .any(|f| f.as_ref().is_some_and(|(c, _)| c == wanted))
    };
    if has("ACCOUNT_LOGIN_VERIFICATION_EMAIL") {
        return AuthResponse::Rejected(Rejection::NewLocation);
    }
    if let Some((_, text)) = all.iter().find_map(|f| {
        f.as_ref().filter(|(c, _)| {
            matches!(
                c.as_str(),
                "ACCOUNT_DISABLED" | "ACCOUNT_SCHEDULED_FOR_DELETION"
            )
        })
    }) {
        return AuthResponse::Rejected(Rejection::Disabled(if text.is_empty() {
            "This account is disabled.".into()
        } else {
            text.clone()
        }));
    }
    if ticket.is_some() {
        return AuthResponse::Rejected(Rejection::TicketExpired);
    }
    if login.is_some() || password.is_some() || code_field.is_some() {
        let message = |f: Option<(String, String)>| {
            f.map(|(code, text)| if text.is_empty() { code } else { text })
        };
        return AuthResponse::Rejected(Rejection::Fields {
            login: message(login),
            password: message(password),
            code: message(code_field),
        });
    }
    if message.to_lowercase().contains("two-factor") && message.to_lowercase().contains("ticket") {
        return AuthResponse::Rejected(Rejection::TicketExpired);
    }
    AuthResponse::Rejected(Rejection::Other(if message.is_empty() {
        format!("Discord refused the login ({status}).")
    } else {
        message
    }))
}

/// What `POST /auth/mfa/sms/send` reports: the masked phone number.
pub fn parse_sms_sent(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("phone")?
        .as_str()
        .map(str::to_owned)
}

/// A logged-out client for the login requests. Clones share the fingerprint,
/// which is kept for the whole sign-in (the code check uses the same one).
#[derive(Clone)]
pub struct AuthClient {
    http: reqwest::Client,
    base: String,
    super_properties: String,
    locale: String,
    timezone: String,
    fingerprint: Arc<Mutex<Option<String>>>,
}

impl Default for AuthClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AuthClient {
    pub fn new() -> Self {
        Self::with_base(API_BASE)
    }

    /// A client for another API base. Only a loopback address may be plain
    /// HTTP (for tests against a local server).
    pub fn with_base(base: impl Into<String>) -> Self {
        let base = base.into().trim_end_matches('/').to_owned();
        let loopback = base.starts_with("http://127.0.0.1:");
        let http = reqwest::Client::builder()
            .user_agent(kc_core::super_properties::USER_AGENT)
            .https_only(!loopback)
            .build()
            .expect("reqwest client");
        Self {
            http,
            base,
            super_properties: kc_core::super_properties::SuperProperties::new().as_header(),
            locale: "en-US".to_owned(),
            timezone: std::env::var("TZ").unwrap_or_else(|_| "UTC".to_owned()),
            fingerprint: Arc::new(Mutex::new(None)),
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        browser_headers(
            self.http.request(method, format!("{}{}", self.base, path)),
            LOGIN_REFERER,
            &self.locale,
            &self.timezone,
            &self.super_properties,
        )
    }

    /// The fingerprint from `GET /experiments`, fetched once. A failure is
    /// not fatal: the login is sent without one.
    pub async fn fingerprint(&self) -> Option<String> {
        if let Some(fingerprint) = self.fingerprint.lock().clone() {
            return Some(fingerprint);
        }
        let response = self
            .request(reqwest::Method::GET, "/experiments")
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            tracing::info!(status = %response.status(), "no login fingerprint");
            return None;
        }
        let value = response.json::<Value>().await.ok()?;
        let fingerprint = value.get("fingerprint")?.as_str()?.to_owned();
        *self.fingerprint.lock() = Some(fingerprint.clone());
        Some(fingerprint)
    }

    /// `POST /auth/login`. A captcha challenge comes back as
    /// [`Error::Captcha`], so it can go through the usual captcha flow and
    /// be retried with the answer.
    pub async fn login(
        &self,
        login: &str,
        password: &Password,
        captcha: Option<&CaptchaAnswer>,
    ) -> Result<AuthResponse> {
        let body = login_body(login, password, captcha);
        self.post("/auth/login", body, captcha).await
    }

    /// Checks a two-factor code against the login's ticket.
    pub async fn submit_code(
        &self,
        method: MfaMethod,
        code: &Password,
        ticket: &str,
        captcha: Option<&CaptchaAnswer>,
    ) -> Result<AuthResponse> {
        // Diagnostics for a rejected code: the code's shape and the ticket's
        // length, never their values, so a mangled/empty code or a missing
        // ticket is visible without leaking the secret.
        let digits = code.expose().chars().filter(char::is_ascii_digit).count();
        tracing::info!(
            ?method,
            code_len = code.expose().chars().count(),
            code_digits = digits,
            ticket_len = ticket.len(),
            has_captcha = captcha.is_some(),
            "submitting a two-factor code"
        );
        let body = mfa_body(code, ticket, captcha);
        self.post(method.path(), body, captcha).await
    }

    /// Asks Discord to text a code. Gives the masked phone number on success,
    /// or why not.
    pub async fn send_sms(
        &self,
        ticket: &str,
        captcha: Option<&CaptchaAnswer>,
    ) -> Result<std::result::Result<Option<String>, Rejection>> {
        let (status, retry_after, bytes) = self
            .send(
                "/auth/mfa/sms/send",
                sms_send_body(ticket, captcha),
                captcha,
            )
            .await?;
        if status.is_success() {
            return Ok(Ok(parse_sms_sent(&bytes)));
        }
        match parse_response(status, retry_after, &bytes) {
            // Surface a captcha as the error the caller's captcha-solve flow
            // (`with_captcha`) understands, so it can be solved and retried.
            AuthResponse::Captcha {
                service,
                sitekey,
                rqdata,
                rqtoken,
            } => Err(Error::Captcha {
                service,
                sitekey,
                rqdata,
                rqtoken,
            }),
            AuthResponse::Rejected(rejection) => Ok(Err(rejection)),
            _ => Ok(Err(Rejection::Other(
                "Discord could not send a text message (unexpected answer).".to_owned(),
            ))),
        }
    }

    async fn post(
        &self,
        path: &str,
        body: Zeroizing<Vec<u8>>,
        captcha: Option<&CaptchaAnswer>,
    ) -> Result<AuthResponse> {
        let (status, retry_after, bytes) = self.send(path, body, captcha).await?;
        match parse_response(status, retry_after, &bytes) {
            AuthResponse::Captcha {
                service,
                sitekey,
                rqdata,
                rqtoken,
            } => Err(Error::Captcha {
                service,
                sitekey,
                rqdata,
                rqtoken,
            }),
            other => Ok(other),
        }
    }

    /// Sends one POST, never retried (a rate limit is reported, not waited
    /// out). The response body is returned wiped-on-drop: a success holds
    /// the token.
    async fn send(
        &self,
        path: &str,
        body: Zeroizing<Vec<u8>>,
        captcha: Option<&CaptchaAnswer>,
    ) -> Result<(StatusCode, Option<f64>, Zeroizing<Vec<u8>>)> {
        let mut request = self
            .request(reqwest::Method::POST, path)
            .header("content-type", "application/json");
        if let Some(fingerprint) = self.fingerprint().await {
            request = request.header("x-fingerprint", fingerprint);
        }
        if let Some(captcha) = captcha {
            request = request.header("x-captcha-key", &captcha.key);
            if let Some(rqtoken) = &captcha.rqtoken {
                request = request.header("x-captcha-rqtoken", rqtoken);
            }
        }
        // reqwest takes its own copy; ours is wiped when `body` drops.
        let response = request.body(body.to_vec()).send().await?;
        drop(body);
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<f64>().ok());
        let bytes = Zeroizing::new(response.bytes().await?.to_vec());
        tracing::info!(path, %status, "login request answered");
        // On a failure, log Discord's own error code and message (never the
        // request, which holds the password/code). This is how Discord
        // describes the problem, so a rejected login can be diagnosed.
        if !status.is_success() {
            let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
            let code = value.get("code").and_then(Value::as_i64);
            let message = value.get("message").and_then(Value::as_str).unwrap_or("");
            let fields: Vec<&str> = value
                .get("errors")
                .and_then(Value::as_object)
                .map(|map| map.keys().map(String::as_str).collect())
                .unwrap_or_default();
            tracing::warn!(path, ?code, message, ?fields, "login request refused");
        }
        Ok((status, retry_after, bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(status: u16, body: &str) -> AuthResponse {
        parse_response(StatusCode::from_u16(status).unwrap(), None, body.as_bytes())
    }

    fn rejected(status: u16, body: &str) -> Rejection {
        match parse(status, body) {
            AuthResponse::Rejected(rejection) => rejection,
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn login_body_is_the_web_clients() {
        let body = login_body("me@example.com", &Password::new("hunter2"), None);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            value,
            json!({"login": "me@example.com", "password": "hunter2", "undelete": false,
                   "login_source": null, "gift_code_sku_id": null})
        );
        let captcha = CaptchaAnswer {
            key: "answer".into(),
            rqtoken: Some("rq".into()),
        };
        let body = login_body("me", &Password::new("pw"), Some(&captcha));
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["captcha_key"], "answer");
        assert_eq!(value["captcha_rqtoken"], "rq");
    }

    #[test]
    fn mfa_bodies() {
        let body = mfa_body(&Password::new("123456"), "ticket", None);
        let value: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            value,
            json!({"code": "123456", "ticket": "ticket", "login_source": null,
                   "gift_code_sku_id": null})
        );
        let value: Value = serde_json::from_slice(&sms_send_body("t", None)).unwrap();
        assert_eq!(value, json!({"ticket": "t"}));
        // With a captcha answer the key rides along in the body.
        let answer = CaptchaAnswer {
            key: "k".to_owned(),
            rqtoken: Some("r".to_owned()),
        };
        let with_captcha: Value =
            serde_json::from_slice(&sms_send_body("t", Some(&answer))).unwrap();
        assert_eq!(
            with_captcha,
            json!({"ticket": "t", "captcha_key": "k", "captcha_rqtoken": "r"})
        );
        assert_eq!(MfaMethod::Totp.path(), "/auth/mfa/totp");
        assert_eq!(MfaMethod::Backup.path(), "/auth/mfa/backup");
        assert_eq!(MfaMethod::Sms.path(), "/auth/mfa/sms");
    }

    #[test]
    fn secrets_never_print() {
        let password = Password::new("hunter2");
        assert!(!format!("{password:?}").contains("hunter2"));
        let challenge = MfaChallenge {
            ticket: "secret-ticket".into(),
            totp: true,
            sms: false,
            backup: true,
            webauthn: false,
        };
        assert!(!format!("{challenge:?}").contains("secret-ticket"));
        let token = parse(200, r#"{"token":"mfa.abcdefghijklmnopqrstuvwxyz"}"#);
        assert!(!format!("{token:?}").contains("abcdefghijklmnopqrstuvwxyz"));
    }

    #[test]
    fn a_token_signs_in() {
        assert_eq!(
            parse(
                200,
                r#"{"user_id":"1","token":"tok","user_settings":{"locale":"en-US","theme":"dark"}}"#
            ),
            AuthResponse::Token(Token::new("tok"))
        );
    }

    #[test]
    fn two_factor_is_asked_for() {
        let response = parse(
            200,
            r#"{"user_id":"1","mfa":true,"sms":true,"ticket":"t1","backup":true,"totp":true,"webauthn":null}"#,
        );
        assert_eq!(
            response,
            AuthResponse::Mfa(MfaChallenge {
                ticket: "t1".into(),
                totp: true,
                sms: true,
                backup: true,
                webauthn: false,
            })
        );
        let response = parse(
            200,
            r#"{"mfa":true,"ticket":"t2","totp":false,"sms":false,"backup":false,"webauthn":"{\"publicKey\":{}}"}"#,
        );
        let AuthResponse::Mfa(challenge) = response else {
            panic!()
        };
        assert!(challenge.webauthn && !challenge.totp);
    }

    #[test]
    fn a_success_without_token_is_explained() {
        assert!(matches!(
            rejected(
                200,
                r#"{"user_id":"1","required_actions":["UPDATE_PASSWORD"]}"#
            ),
            Rejection::Other(_)
        ));
    }

    #[test]
    fn a_captcha_is_asked_for() {
        assert_eq!(
            parse(
                400,
                r#"{"captcha_key":["captcha-required"],"captcha_sitekey":"site","captcha_service":"hcaptcha","captcha_rqdata":"data","captcha_rqtoken":"rq"}"#
            ),
            AuthResponse::Captcha {
                service: "hcaptcha".into(),
                sitekey: "site".into(),
                rqdata: Some("data".into()),
                rqtoken: Some("rq".into()),
            }
        );
        assert!(matches!(
            rejected(400, r#"{"captcha_key":["invalid-response"]}"#),
            Rejection::Other(_)
        ));
    }

    #[test]
    fn field_errors_go_to_their_fields() {
        let body = r#"{"code":50035,"message":"Invalid Form Body","errors":{
            "login":{"_errors":[{"code":"INVALID_LOGIN","message":"Login or password is invalid."}]},
            "password":{"_errors":[{"code":"INVALID_LOGIN","message":"Login or password is invalid."}]}}}"#;
        assert_eq!(
            rejected(400, body),
            Rejection::Fields {
                login: Some("Login or password is invalid.".into()),
                password: Some("Login or password is invalid.".into()),
                code: None,
            }
        );
        let body = r#"{"code":50035,"errors":{"password":{"_errors":[{"code":"BASE_TYPE_REQUIRED","message":"This field is required"}]}}}"#;
        assert_eq!(
            rejected(400, body),
            Rejection::Fields {
                login: None,
                password: Some("This field is required".into()),
                code: None,
            }
        );
    }

    #[test]
    fn a_new_location_wants_the_email() {
        let body = r#"{"code":50035,"message":"Invalid Form Body","errors":{"login":{"_errors":[{"code":"ACCOUNT_LOGIN_VERIFICATION_EMAIL","message":"New login location detected, please check your e-mail."}]}}}"#;
        assert_eq!(rejected(400, body), Rejection::NewLocation);
    }

    #[test]
    fn a_disabled_account_says_so() {
        let body = r#"{"code":50035,"errors":{"login":{"_errors":[{"code":"ACCOUNT_DISABLED","message":"This account has been disabled."}]}}}"#;
        assert_eq!(
            rejected(400, body),
            Rejection::Disabled("This account has been disabled.".into())
        );
        let body = r#"{"code":50035,"errors":{"login":{"_errors":[{"code":"ACCOUNT_SCHEDULED_FOR_DELETION","message":""}]}}}"#;
        assert!(matches!(rejected(400, body), Rejection::Disabled(_)));
    }

    #[test]
    fn a_rate_limit_gives_the_wait() {
        assert_eq!(
            rejected(
                429,
                r#"{"message":"You are being rate limited.","retry_after":12.5,"global":false}"#
            ),
            Rejection::RateLimited { retry_after: 12.5 }
        );
        assert_eq!(
            parse_response(StatusCode::TOO_MANY_REQUESTS, Some(3.0), b""),
            AuthResponse::Rejected(Rejection::RateLimited { retry_after: 3.0 })
        );
    }

    #[test]
    fn code_errors() {
        assert_eq!(
            rejected(400, r#"{"message":"Invalid two-factor code","code":60008}"#),
            Rejection::InvalidCode
        );
        assert_eq!(
            rejected(
                400,
                r#"{"message":"Invalid two-factor auth ticket","code":60006}"#
            ),
            Rejection::TicketExpired
        );
        assert_eq!(
            rejected(
                400,
                r#"{"code":50035,"errors":{"ticket":{"_errors":[{"code":"BASE_TYPE_INVALID","message":"Invalid ticket"}]}}}"#
            ),
            Rejection::TicketExpired
        );
        assert_eq!(
            rejected(
                400,
                r#"{"code":50035,"errors":{"code":{"_errors":[{"code":"BASE_TYPE_MAX_LENGTH","message":"Must be 8 or fewer in length."}]}}}"#
            ),
            Rejection::Fields {
                login: None,
                password: None,
                code: Some("Must be 8 or fewer in length.".into()),
            }
        );
    }

    #[test]
    fn anything_else_uses_discords_message_never_the_body() {
        assert_eq!(
            rejected(403, r#"{"message":"Nope","code":1}"#),
            Rejection::Other("Nope".into())
        );
        let Rejection::Other(text) = rejected(500, "<html>password=hunter2</html>") else {
            panic!()
        };
        assert!(!text.contains("hunter2"), "{text}");
        assert_eq!(
            parse_sms_sent(br#"{"phone":"+*******1234"}"#).as_deref(),
            Some("+*******1234")
        );
    }

    // -- against a local server -------------------------------------------

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A request as the server saw it: request line + headers, and the body.
    struct Seen {
        head: String,
        body: Value,
    }

    /// Serves `replies` (status line, JSON body) in order, one connection
    /// each, and returns what was asked.
    async fn serve(
        replies: Vec<(&'static str, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<Seen>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/api/v9", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, reply) in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                let mut buffer = [0u8; 4096];
                let (head, body) = loop {
                    let read = socket.read(&mut buffer).await.unwrap();
                    data.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&data).to_string();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let head = text[..end].to_owned();
                        let length = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if data.len() >= end + 4 + length {
                            break (head, data[end + 4..end + 4 + length].to_vec());
                        }
                    }
                    if read == 0 {
                        panic!("connection closed early");
                    }
                };
                seen.push(Seen {
                    head,
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                });
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                    reply.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.ok();
            }
            seen
        });
        (base, task)
    }

    fn header<'a>(seen: &'a Seen, name: &str) -> Option<&'a str> {
        seen.head.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name).then(|| value.trim())
        })
    }

    #[tokio::test]
    async fn the_whole_flow_against_a_local_server() {
        let (base, server) = serve(vec![
            ("200 OK", r#"{"fingerprint":"111.fp","assignments":[]}"#),
            (
                "400 Bad Request",
                r#"{"captcha_key":["captcha-required"],"captcha_sitekey":"site","captcha_service":"hcaptcha","captcha_rqtoken":"rq"}"#,
            ),
            (
                "200 OK",
                r#"{"user_id":"1","mfa":true,"sms":false,"ticket":"tick","backup":true,"totp":true,"webauthn":null}"#,
            ),
            ("200 OK", r#"{"token":"the-token","user_settings":{}}"#),
        ])
        .await;
        let client = AuthClient::with_base(base);
        let password = Password::new("hunter2");

        let Err(Error::Captcha { rqtoken, .. }) = client.login("me", &password, None).await else {
            panic!("expected a captcha");
        };
        let answer = CaptchaAnswer {
            key: "solved".into(),
            rqtoken,
        };
        let AuthResponse::Mfa(challenge) =
            client.login("me", &password, Some(&answer)).await.unwrap()
        else {
            panic!("expected two-factor");
        };
        let response = client
            .submit_code(
                MfaMethod::Totp,
                &Password::new("123456"),
                &challenge.ticket,
                None,
            )
            .await
            .unwrap();
        assert_eq!(response, AuthResponse::Token(Token::new("the-token")));

        let seen = server.await.unwrap();
        assert!(seen[0].head.starts_with("GET /api/v9/experiments "));
        for request in &seen[1..] {
            assert_eq!(header(request, "authorization"), None);
            assert_eq!(header(request, "x-fingerprint"), Some("111.fp"));
            assert!(header(request, "x-super-properties").is_some());
            assert_eq!(header(request, "referer"), Some(LOGIN_REFERER));
            assert_eq!(header(request, "content-type"), Some("application/json"));
        }
        assert!(seen[1].head.starts_with("POST /api/v9/auth/login "));
        assert_eq!(seen[1].body["password"], "hunter2");
        assert_eq!(seen[1].body.get("captcha_key"), None);
        assert_eq!(seen[2].body["captcha_key"], "solved");
        assert_eq!(seen[2].body["captcha_rqtoken"], "rq");
        assert_eq!(header(&seen[2], "x-captcha-key"), Some("solved"));
        assert!(seen[3].head.starts_with("POST /api/v9/auth/mfa/totp "));
        assert_eq!(
            seen[3].body,
            json!({"code": "123456", "ticket": "tick", "login_source": null,
                   "gift_code_sku_id": null})
        );
    }

    #[tokio::test]
    async fn sms_is_sent_and_a_rate_limit_is_not_retried() {
        let (base, server) = serve(vec![
            ("403 Forbidden", r#"{"message":"no"}"#),
            ("200 OK", r#"{"phone":"+*******1234"}"#),
            (
                "429 Too Many Requests",
                r#"{"message":"You are being rate limited.","retry_after":30.0,"global":false}"#,
            ),
        ])
        .await;
        let client = AuthClient::with_base(base);
        // The fingerprint request failed (403): the login goes without one,
        // and asks again next time, so this test sends no further GET.
        *client.fingerprint.lock() = None;
        assert_eq!(client.fingerprint().await, None);
        *client.fingerprint.lock() = Some("fp".into());
        assert_eq!(
            client.send_sms("tick", None).await.unwrap(),
            Ok(Some("+*******1234".to_owned()))
        );
        let response = client
            .login("me", &Password::new("pw"), None)
            .await
            .unwrap();
        assert_eq!(
            response,
            AuthResponse::Rejected(Rejection::RateLimited { retry_after: 30.0 })
        );
        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 3);
        assert!(seen[1].head.starts_with("POST /api/v9/auth/mfa/sms/send "));
        assert_eq!(seen[1].body, json!({"ticket": "tick"}));
    }
}
