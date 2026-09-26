//! Signing a Feishu account in: scanning a QR code creates a bot app for the
//! user (the device flow Lark's own CLI uses), or an existing app is added
//! with its ID and secret.

use crate::feishu::{
    api::{self, Api, Endpoints},
    credentials::{self, Account, Brand, Store},
};
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::time::Duration;

const REGISTRATION_PATH: &str = "/oauth/v1/app/registration";
const DEFAULT_INTERVAL: u64 = 5;
const MAX_INTERVAL: u64 = 60;
const DEFAULT_EXPIRY: u64 = 600;

/// A registration waiting for the user's scan.
pub struct Pending {
    pub device_code: String,
    /// The page to scan or open, on the brand's own host.
    pub url: String,
    pub interval: Duration,
    pub expires_in: Duration,
}

/// What a completed registration returns.
pub struct Registered {
    pub account: Account,
}

/// Sign in by creating a bot app: show a QR code, wait for the scan, and
/// save the app with its creator as the owner.
pub(crate) async fn login(store: &Store, account: &str, brand: Brand) -> Result<()> {
    crate::state::validate_name(account)?;
    if store.account(account)?.is_some() {
        bail!(
            "Feishu account {account:?} is already signed in; run `scv channels logout feishu --account {account}` first"
        )
    }
    let client = api::http_client()?;
    // Registration always starts on Feishu; the scan reveals a Lark tenant.
    let feishu = Endpoints::for_brand(Brand::Feishu);
    let pending = begin(&client, &feishu, &Endpoints::for_brand(brand)).await?;
    println!(
        "Scan this QR code with the {} app to create your SCV bot, or open the link:\n\n{}\n{}\n",
        brand.title(),
        qr_code(&pending.url),
        pending.url
    );
    println!(
        "Waiting for confirmation (the code expires in {} minutes)...",
        pending.expires_in.as_secs().div_ceil(60)
    );
    let registered = poll(&client, &feishu, &pending).await?;
    let created = registered.account;
    credentials::save(store, account, &created)?;
    if created.owner_open_id.is_some() {
        println!(
            "{} bot {} created and signed in; its creator is the owner.",
            created.brand.title(),
            created.app_id
        );
    } else {
        println!(
            "{} bot {} created and signed in.",
            created.brand.title(),
            created.app_id
        );
        println!(
            "{}; log out and scan again to record one.",
            no_owner_note(store, account)
        );
    }
    println!("{}", rename_hint(&created));
    Ok(())
}

/// What an account whose sign-in names no owner does: remote tools stay
/// off, and with the default `senders = "owner"` it answers nobody.
fn no_owner_note(store: &Store, account: &str) -> &'static str {
    let anyone = store
        .settings(account)
        .is_ok_and(|settings| settings.senders == crate::state::Senders::Anyone);
    if anyone {
        "No owner is recorded, so remote tools stay off for everyone"
    } else {
        "No owner is recorded, so this account, which answers only its owner, answers nobody and remote tools stay off"
    }
}

/// Sign in with an existing app. The secret is checked with Feishu before
/// anything is saved.
pub(crate) async fn login_existing(
    store: &Store,
    account: &str,
    app_id: &str,
    app_secret: &str,
    owner_open_id: Option<&str>,
    brand: Brand,
) -> Result<()> {
    crate::state::validate_name(account)?;
    let credentials = Account {
        app_id: app_id.into(),
        app_secret: app_secret.into(),
        brand,
        owner_open_id: owner_open_id.map(str::to_owned),
    };
    credentials.validate()?;
    let api = Api::new(Endpoints::for_brand(brand), app_id, app_secret)?;
    api.validate().await.map_err(|error| {
        anyhow!(
            "{} did not accept the app ID and secret: {error:#}",
            brand.title()
        )
    })?;
    credentials::save(store, account, &credentials)?;
    println!("{} app {app_id} signed in.", brand.title());
    if owner_open_id.is_none() {
        println!(
            "{}; sign in again with --owner-open-id to name one.",
            no_owner_note(store, account)
        );
    }
    Ok(())
}

/// Start a registration: check the server supports secret-based apps, then
/// ask for a device code. `display` is the brand whose page the user opens.
pub async fn begin(
    client: &reqwest::Client,
    registration: &Endpoints,
    display: &Endpoints,
) -> Result<Pending> {
    let init = post(client, registration, &[("action", "init")]).await?;
    let methods = init
        .get("supported_auth_methods")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Feishu app registration is unavailable"))?;
    if !methods.iter().any(|m| m.as_str() == Some("client_secret")) {
        bail!("Feishu app registration no longer offers secret-based apps; sign in with --app-id")
    }
    let begun = post(
        client,
        registration,
        &[
            ("action", "begin"),
            ("archetype", "PersonalAgent"),
            ("auth_method", "client_secret"),
            ("request_user_info", "open_id tenant_brand"),
        ],
    )
    .await?;
    if let Some(error) = begun.get("error").and_then(Value::as_str) {
        bail!("Feishu app registration failed: {}", safe_code(error))
    }
    let device_code = begun
        .get("device_code")
        .and_then(Value::as_str)
        .filter(|code| !code.is_empty())
        .ok_or_else(|| anyhow!("Feishu app registration omitted its device code"))?;
    let user_code = begun
        .get("user_code")
        .and_then(Value::as_str)
        .filter(|code| {
            !code.is_empty()
                && code.len() <= 64
                && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        .ok_or_else(|| anyhow!("Feishu app registration omitted its user code"))?;
    let seconds = |key: &str| begun.get(key).and_then(Value::as_u64).filter(|s| *s > 0);
    Ok(Pending {
        device_code: device_code.into(),
        url: format!("{}/page/cli?user_code={user_code}", display.open),
        interval: Duration::from_secs(
            seconds("interval")
                .unwrap_or(DEFAULT_INTERVAL)
                .min(MAX_INTERVAL),
        ),
        expires_in: Duration::from_secs(
            seconds("expire_in")
                .or_else(|| seconds("expires_in"))
                .unwrap_or(DEFAULT_EXPIRY)
                .min(24 * 60 * 60),
        ),
    })
}

/// Poll until the user confirms, denies, or the code expires. A Lark user's
/// scan moves polling to Lark once.
pub async fn poll(
    client: &reqwest::Client,
    registration: &Endpoints,
    pending: &Pending,
) -> Result<Registered> {
    let deadline = tokio::time::Instant::now() + pending.expires_in;
    let mut endpoints = registration.clone();
    let mut brand = Brand::Feishu;
    let mut switched = false;
    let mut interval = pending.interval;
    let mut wait = false;
    loop {
        if wait {
            tokio::time::sleep(interval).await;
        }
        wait = true;
        if tokio::time::Instant::now() >= deadline {
            bail!("The Feishu QR code expired; run `scv channels login feishu` again")
        }
        let answer = match post(
            client,
            &endpoints,
            &[("action", "poll"), ("device_code", &pending.device_code)],
        )
        .await
        {
            Ok(answer) => answer,
            Err(error) => {
                tracing::warn!("Feishu registration poll failed: {error:#}");
                interval =
                    (interval + Duration::from_secs(1)).min(Duration::from_secs(MAX_INTERVAL));
                continue;
            }
        };
        let tenant = answer
            .pointer("/user_info/tenant_brand")
            .and_then(Value::as_str)
            .and_then(Brand::parse);
        if !switched && let Some(tenant) = tenant.filter(|tenant| *tenant != brand) {
            // Credentials are issued by the tenant's own brand.
            brand = tenant;
            endpoints = redirect_accounts(registration, tenant);
            switched = true;
            wait = false;
            continue;
        }
        match answer.get("error").and_then(Value::as_str) {
            None | Some("") => {}
            Some("authorization_pending") => continue,
            Some("slow_down") => {
                interval =
                    (interval + Duration::from_secs(5)).min(Duration::from_secs(MAX_INTERVAL));
                continue;
            }
            Some("access_denied") => bail!("The Feishu app creation was declined"),
            Some("expired_token" | "invalid_grant") => {
                bail!("The Feishu QR code expired; run `scv channels login feishu` again")
            }
            Some(other) => bail!("Feishu app registration failed: {}", safe_code(other)),
        }
        let text = |key: &str| {
            answer
                .get(key)
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        };
        let (Some(app_id), Some(app_secret)) = (text("client_id"), text("client_secret")) else {
            continue;
        };
        if tenant.is_some_and(|tenant| tenant != brand) {
            bail!("Feishu app registration reported two different tenants")
        }
        let owner = answer
            .pointer("/user_info/open_id")
            .and_then(Value::as_str)
            .filter(|open_id| credentials::validate_open_id(open_id).is_ok())
            .map(str::to_owned);
        if owner.is_none() {
            tracing::warn!(
                "Feishu app registration returned no owner; an owner-only account answers nobody, and remote tools stay off"
            );
        }
        let account = Account {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            brand,
            owner_open_id: owner,
        };
        account.validate()?;
        return Ok(Registered { account });
    }
}

/// The same registration service on the tenant brand's accounts host.
fn redirect_accounts(registration: &Endpoints, brand: Brand) -> Endpoints {
    let production = Endpoints::for_brand(Brand::Feishu);
    if registration.accounts == production.accounts {
        Endpoints::for_brand(brand)
    } else {
        // Local fakes serve every brand on one origin.
        registration.clone()
    }
}

async fn post(
    client: &reqwest::Client,
    endpoints: &Endpoints,
    form: &[(&str, &str)],
) -> Result<Value> {
    let response = client
        .post(format!("{}{REGISTRATION_PATH}", endpoints.accounts))
        .form(form)
        .timeout(Duration::from_secs(30))
        .send()
        .await?;
    let status = response.status();
    let value = api::read_json(response).await?;
    // Registration explains pending and failed states in 4xx bodies.
    if status.is_server_error() {
        bail!(
            "Feishu app registration failed with status {}",
            status.as_u16()
        )
    }
    Ok(value)
}

/// An error code from the server, kept to a safe shape for display.
fn safe_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect()
}

/// Where to rename the bot, which registration names after its creator.
pub fn rename_hint(account: &Account) -> String {
    let console = Endpoints::for_brand(account.brand).open;
    format!(
        "The bot is named after you (\"...的飞书 CLI\"). To rename it, open {console}/app/{}, edit the name under 凭证与基础信息 (Credentials & Basic Info), then publish a new version under 版本管理与发布 (Version Management & Release).",
        account.app_id
    )
}

/// The URL as a terminal QR code, light on dark so phones read it from a
/// dark terminal.
pub fn qr_code(url: &str) -> String {
    use qrcode::render::unicode::Dense1x2;
    match qrcode::QrCode::new(url.as_bytes()) {
        Ok(code) => code
            .render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .quiet_zone(true)
            .build(),
        Err(_) => String::new(),
    }
}
