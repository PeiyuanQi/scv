//! `scv channels`: sign chat accounts in, enable or disable them under the
//! daemon, and remove them.

use anyhow::{Context, Result, bail};
use scv_channels::Channel as _;
use scv_channels::feishu::{self, Feishu};
use scv_channels::wechat::{self, WeChat};
use scv_client::Layout;
use scv_protocol::{ComponentHealth, DaemonCommand, RemoteTools, Senders};
use std::path::Path;

use super::args::{ChannelArg, ChannelsCommand};
use super::control;
use super::prompt::read_secret;
use super::status::show_status;

pub(crate) async fn channels(layout: &Layout, command: ChannelsCommand) -> Result<()> {
    match command {
        ChannelsCommand::Login {
            channel,
            account,
            login_url,
            app_id,
            owner_open_id,
        } => {
            match channel {
                ChannelArg::Wechat => {
                    if app_id.is_some() {
                        bail!("--app-id and --owner-open-id are Feishu options");
                    }
                    let base_url =
                        login_url.unwrap_or_else(|| "https://ilinkai.weixin.qq.com".into());
                    WeChat::login(layout, &account, wechat::Login { base_url }).await?;
                }
                ChannelArg::Feishu | ChannelArg::Lark => {
                    if login_url.is_some() {
                        bail!("--login-url is a WeChat option");
                    }
                    let brand = match channel {
                        ChannelArg::Lark => feishu::Brand::Lark,
                        _ => feishu::Brand::Feishu,
                    };
                    let login = match app_id {
                        Some(app_id) => {
                            let secret = read_secret(&format!(
                                "{} app secret (input hidden)",
                                brand.title()
                            ))?;
                            feishu::Login::Existing {
                                brand,
                                app_id,
                                app_secret: secret.into(),
                                owner_open_id,
                            }
                        }
                        None => feishu::Login::Scan { brand },
                    };
                    Feishu::login(layout, &account, login).await?;
                }
            }
            reload_after_login(layout).await
        }
        ChannelsCommand::Run {
            channel,
            account,
            workspace,
            remote_tools,
            senders,
        } => {
            channel_run(
                layout,
                channel,
                &account,
                &workspace,
                remote_tools.map(Into::into),
                senders.map(Into::into),
            )
            .await
        }
        ChannelsCommand::Stop { channel, account } => {
            control(
                layout,
                DaemonCommand::ChannelSet {
                    channel: channel.name().into(),
                    account,
                    enabled: false,
                    workspace: None,
                    remote_tools: None,
                    senders: None,
                },
            )
            .await?;
            println!("{} account disabled and stopped.", channel.title());
            Ok(())
        }
        ChannelsCommand::Status { channel, account } => {
            show_status(layout, channel.map(ChannelArg::name), account.as_deref()).await
        }
        ChannelsCommand::Logout { channel, account } => {
            control(
                layout,
                DaemonCommand::ChannelLogout {
                    channel: channel.name().into(),
                    account,
                },
            )
            .await?;
            println!(
                "{} account stopped; local credentials and delivery state removed.",
                channel.title()
            );
            Ok(())
        }
    }
}

async fn channel_run(
    layout: &Layout,
    channel: ChannelArg,
    account: &str,
    workspace: &Path,
    remote_tools: Option<RemoteTools>,
    senders: Option<Senders>,
) -> Result<()> {
    let workspace = std::fs::canonicalize(workspace).context("resolve channel workspace")?;
    let status = control(
        layout,
        DaemonCommand::ChannelSet {
            channel: channel.name().into(),
            account: account.into(),
            enabled: true,
            workspace: Some(workspace.display().to_string()),
            remote_tools,
            senders,
        },
    )
    .await?;
    println!(
        "{} account enabled under the SCV daemon; use `scv channels status {}` for live connection state.",
        channel.title(),
        channel.name()
    );
    let health = status
        .components
        .iter()
        .find(|health| health.channel == channel.name() && health.account == account);
    if remote_tools == Some(RemoteTools::Owner) {
        // Report what the daemon applied: credentials without an owner ID
        // grant tools to nobody.
        let effective = health.is_some_and(|health| health.remote_tools == RemoteTools::Owner);
        if effective {
            println!(
                "Remote tools: the account's own {} owner now runs every SCV tool without approval prompts.",
                channel.title()
            );
        } else {
            println!(
                "Warning: owner remote tools are saved but not active; the login lacks an owner ID. Remote sessions stay tool-free."
            );
        }
    }
    // Report whose messages the daemon answers now.
    let applied = health.and_then(|health| health.senders);
    if let Some(health) = health.filter(|health| answers_nobody(health)) {
        println!("Warning: {}", nobody_note(health));
    } else {
        match (senders, applied) {
            (Some(_), None) => println!(
                "Warning: the running daemon predates --senders and did not save it; restart it into this release and run this again."
            ),
            (Some(Senders::Owner), Some(Senders::Owner)) => println!(
                "Senders: only the account's own {} owner is answered; everyone else's messages are dropped unanswered.",
                channel.title()
            ),
            (Some(Senders::Anyone), Some(Senders::Anyone)) => println!(
                "Senders: anyone who can reach the bot is answered; everyone but the owner stays tool-free."
            ),
            _ => {}
        }
    }
    Ok(())
}

/// Whether an account answers only its owner but has none recorded.
pub(crate) fn answers_nobody(health: &ComponentHealth) -> bool {
    health.senders == Some(Senders::Owner) && health.user_id.as_deref().is_none_or(str::is_empty)
}

/// Why [`answers_nobody`] holds, and what changes it.
pub(crate) fn nobody_note(health: &ComponentHealth) -> String {
    format!(
        "{:?} answers nobody: it answers only its owner (senders = \"owner\"), and its sign-in records no owner ID. Sign in again naming the owner, or set senders = \"anyone\" in [channels.{}.{}] to answer every sender tool-free.",
        health.id,
        health.channel.escape_debug(),
        health.account.escape_debug()
    )
}

async fn reload_after_login(layout: &Layout) -> Result<()> {
    match control(layout, DaemonCommand::Reload).await {
        Ok(_) => println!("Daemon refreshed; enabled accounts start automatically."),
        Err(_) => println!(
            "Credentials saved. The daemon will load enabled accounts at startup or its next refresh."
        ),
    }
    Ok(())
}
