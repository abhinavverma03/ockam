use clap::Args;

use anyhow::anyhow;
use ockam_api::cloud::ORCHESTRATOR_RESTART_TIMEOUT;

use std::borrow::Borrow;
use std::io::stdin;

use colorful::Colorful;
use reqwest::StatusCode;
use tokio::time::{sleep, Duration};
use tokio_retry::{strategy::ExponentialBackoff, Retry};
use tracing::{debug, info};

use ockam::Context;
use ockam_api::cloud::enroll::auth0::*;
use ockam_api::cloud::project::{OktaAuth0, Project};
use ockam_api::cloud::space::Space;
use ockam_core::api::Status;

use crate::node::util::{delete_embedded_node, start_embedded_node};
use crate::project::util::check_project_readiness;

use crate::space::util::config;
use crate::terminal::OckamColor;
use crate::util::api::CloudOpts;

use crate::util::{api, node_rpc, RpcBuilder};
use crate::{docs, fmt_err, fmt_info, fmt_log, fmt_ok, CommandGlobalOpts, Result};

const LONG_ABOUT: &str = include_str!("./static/long_about.txt");
const AFTER_LONG_HELP: &str = include_str!("./static/after_long_help.txt");

/// Enroll with Ockam Orchestrator
#[derive(Clone, Debug, Args)]
#[command(
long_about = docs::about(LONG_ABOUT),
after_long_help = docs::after_help(AFTER_LONG_HELP)
)]
pub struct EnrollCommand {
    #[command(flatten)]
    pub cloud_opts: CloudOpts,
}

impl EnrollCommand {
    pub fn run(self, options: CommandGlobalOpts) {
        node_rpc(rpc, (options, self));
    }
}

async fn rpc(ctx: Context, (opts, cmd): (CommandGlobalOpts, EnrollCommand)) -> Result<()> {
    run_impl(&ctx, opts, cmd).await
}

async fn run_impl(ctx: &Context, opts: CommandGlobalOpts, cmd: EnrollCommand) -> Result<()> {
    let ockam_header = include_str!("../../static/ockam_ascii.txt").trim();
    let colored_header = ockam_header.gradient_with_color(
        OckamColor::OckamBlue.color(),
        OckamColor::PrimaryGradient.color(),
    );

    opts.terminal
        .write_line(&format!("\n{}\n", colored_header))?;

    let node_name = start_embedded_node(ctx, &opts, None).await?;

    enroll(ctx, &opts, &cmd, &node_name).await?;

    let cloud_opts = cmd.cloud_opts.clone();
    let space = default_space(ctx, &opts, &cloud_opts, &node_name).await?;
    default_project(ctx, &opts, &cloud_opts, &node_name, &space).await?;
    update_enrolled_identity(&opts, &node_name).await?;
    delete_embedded_node(&opts, &node_name).await;

    opts.terminal
        .write_line(&fmt_ok!("Enrolled successfully!"))?;
    Ok(())
}

async fn enroll(
    ctx: &Context,
    opts: &CommandGlobalOpts,
    cmd: &EnrollCommand,
    node_name: &str,
) -> Result<()> {
    let auth0 = Auth0Service::new(Auth0Provider::Auth0);
    let token = auth0.token(opts).await?;
    let mut rpc = RpcBuilder::new(ctx, opts, node_name).build();
    rpc.request(api::enroll::auth0(cmd.clone(), token)).await?;
    let (res, dec) = rpc.check_response()?;
    if res.status() == Some(Status::Ok) {
        info!("Enrolled successfully");
        Ok(())
    } else if res.status() == Some(Status::BadRequest) {
        info!("Already enrolled");
        Ok(())
    } else {
        eprintln!("{}", rpc.parse_err_msg(res, dec));
        Err(anyhow!("Failed to enroll").into())
    }
}

async fn default_space<'a>(
    ctx: &Context,
    opts: &CommandGlobalOpts,
    cloud_opts: &CloudOpts,
    node_name: &str,
) -> Result<Space<'a>> {
    // Get available spaces for node's identity
    let mut rpc = RpcBuilder::new(ctx, opts, node_name).build();
    let mut available_spaces = {
        rpc.request(api::space::list(&cloud_opts.route())).await?;
        rpc.parse_response::<Vec<Space>>()?
    };
    // If the identity has no spaces, create one
    let default_space = if available_spaces.is_empty() {
        let cmd = crate::space::CreateCommand {
            cloud_opts: cloud_opts.clone(),
            name: crate::util::random_name(),
            admins: vec![],
        };
        opts.terminal.write_line(&fmt_info!(
            "{}",
            "Creating a trial space for you (everything in it will be deleted in 15 days) ..."
                .light_magenta()
        ))?
        .write_line(&fmt_log!(
            "{}",
            "To learn more about production ready spaces in Ockam Orchestrator, contact us at: hello@ockam.io".light_magenta()
        ))?;

        let mut rpc = RpcBuilder::new(ctx, opts, node_name).build();
        rpc.request(api::space::create(&cmd)).await?;
        rpc.parse_response::<Space>()?.to_owned()
    }
    // If it has, return the first one on the list
    else {
        available_spaces
            .drain(..1)
            .next()
            .expect("already checked that is not empty")
            .to_owned()
    };
    config::set_space(&opts.config, &default_space)?;

    opts.terminal.write_line(&fmt_info!(
        "Space {} is set as default",
        default_space.name.to_string().light_magenta()
    ))?;
    Ok(default_space)
}

async fn default_project<'a>(
    ctx: &Context,
    opts: &CommandGlobalOpts,
    cloud_opts: &CloudOpts,
    node_name: &str,
    space: &Space<'_>,
) -> Result<Project<'a>> {
    // Get available project for the given space
    let mut rpc = RpcBuilder::new(ctx, opts, node_name).build();
    let mut available_projects: Vec<Project> = {
        rpc.request(api::project::list(&cloud_opts.route())).await?;
        rpc.parse_response::<Vec<Project>>()?
    };
    // If the space has no projects, create one
    let default_project = if available_projects.is_empty() {
        let mut rpc = RpcBuilder::new(ctx, opts, node_name).build();
        rpc.request_with_timeout(
            api::project::create("default", &space.id, &cloud_opts.route()),
            Duration::from_secs(ORCHESTRATOR_RESTART_TIMEOUT),
        )
        .await?;
        rpc.parse_response::<Project>()?.to_owned()
    }
    // If it has, return the "default" project or first one on the list
    else {
        match available_projects.iter().find(|ns| ns.name == "default") {
            None => available_projects
                .drain(..1)
                .next()
                .expect("already checked that is not empty")
                .to_owned(),
            Some(p) => p.to_owned(),
        }
    };
    let project =
        check_project_readiness(ctx, opts, cloud_opts, node_name, None, default_project).await?;

    opts.terminal.write_line(&fmt_info!(
        "Project {} is set as default",
        project.name.to_string().light_magenta()
    ))?;

    opts.state.projects.create(&project.name, project.clone())?;
    opts.state
        .trust_contexts
        .create(&project.name, project.clone().try_into()?)?;
    Ok(project)
}

pub enum Auth0Provider {
    Auth0,
    Okta(OktaAuth0),
}

impl Auth0Provider {
    fn client_id(&self) -> &str {
        match self {
            Self::Auth0 => "c1SAhEjrJAqEk6ArWjGjuWX11BD2gK8X",
            Self::Okta(d) => &d.client_id,
        }
    }

    const fn scopes(&self) -> &'static str {
        "profile openid email"
    }

    fn device_code_url(&self) -> String {
        match self {
            Self::Auth0 => "https://account.ockam.io/oauth/device/code".to_string(),
            // See https://developer.okta.com/docs/reference/api/oidc/#composing-your-base-url
            Self::Okta(d) => format!("{}/v1/device/authorize", &d.tenant_base_url),
        }
    }

    fn token_request_url(&self) -> String {
        match self {
            Self::Auth0 => "https://account.ockam.io/oauth/token".to_string(),
            Self::Okta(d) => format!("{}/v1/token", &d.tenant_base_url),
        }
    }

    fn build_http_client(&self) -> Result<reqwest::Client> {
        let client = match self {
            Self::Auth0 => reqwest::Client::new(),
            Self::Okta(d) => {
                let certificate = reqwest::Certificate::from_pem(d.certificate.as_bytes())
                    .map_err(|e| anyhow!("Error parsing certificate: {e}"))?;
                reqwest::ClientBuilder::new()
                    .tls_built_in_root_certs(false)
                    .add_root_certificate(certificate)
                    .build()
                    .map_err(|e| anyhow!("Error building http client: {e}"))?
            }
        };
        Ok(client)
    }
}

pub struct Auth0Service(Auth0Provider);

impl Auth0Service {
    pub fn new(provider: Auth0Provider) -> Self {
        Self(provider)
    }

    fn provider(&self) -> &Auth0Provider {
        &self.0
    }

    pub(crate) async fn token(&self, opts: &CommandGlobalOpts) -> Result<Auth0Token> {
        let dc = self.device_code().await?;

        opts.terminal
            .write_line(&fmt_log!(
                "Enroll Ockam Command's default identity with Ockam Orchestrator"
            ))?
            .write_line(&fmt_info!(
                "First copy your one-time code: {}",
                format!(" {} ", dc.user_code).bg_white().black()
            ))?
            .write(&fmt_log!(
                "Then press enter to open {} in your browser...",
                dc.verification_uri.to_string().light_green()
            ))?;

        let mut input = String::new();
        match stdin().read_line(&mut input) {
            Ok(_) => {
                opts.terminal
                    .write_line(&fmt_log!("Opening: {}", dc.verification_uri))?;
            }
            Err(_e) => {
                return Err(anyhow!("couldn't read enter from stdin").into());
            }
        }

        // Request device activation
        // Note that we try to open the verification uri **without** the code.
        // After the code is entered, if the user closes the tab (because they
        // want to open it on another browser, for example), the uri gets
        // invalidated and the user would have to restart the process (i.e.
        // rerun the command).
        let uri: &str = dc.verification_uri.borrow();
        if open::that(uri).is_err() {
            opts.terminal.write_line(&fmt_err!(
                "Couldn't open activation url automatically [url={}]",
                uri.to_string().light_green()
            ))?;
        }

        self.poll_token(dc, opts).await
    }

    /// Request device code
    async fn device_code(&self) -> Result<DeviceCode<'_>> {
        // More on how to use scope and audience in https://auth0.com/docs/quickstart/native/device#device-code-parameters
        let client = self.provider().build_http_client()?;
        let req = || {
            client
                .post(self.provider().device_code_url())
                .header("content-type", "application/x-www-form-urlencoded")
                .form(&[
                    ("client_id", self.provider().client_id()),
                    ("scope", self.provider().scopes()),
                ])
        };
        let retry_strategy = ExponentialBackoff::from_millis(10).take(3);
        let res = Retry::spawn(retry_strategy, move || req().send())
            .await
            .map_err(|e| anyhow!(e.to_string()))?;
        match res.status() {
            StatusCode::OK => {
                let res = res
                    .json::<DeviceCode>()
                    .await
                    .map_err(|e| anyhow!(e.to_string()))?;
                debug!(?res, "device code received: {res:#?}");
                Ok(res)
            }
            _ => {
                let res = res.text().await.map_err(|e| anyhow!(e.to_string()))?;
                let err_msg = "couldn't get device code";
                debug!(?res, err_msg);
                Err(anyhow!(err_msg).into())
            }
        }
    }

    /// Poll for token until it's ready
    async fn poll_token<'a>(
        &'a self,
        dc: DeviceCode<'a>,
        opts: &CommandGlobalOpts,
    ) -> Result<Auth0Token> {
        let client = self.provider().build_http_client()?;
        let token;
        let spinner_option = opts.terminal.progress_spinner();
        if let Some(spinner) = spinner_option.as_ref() {
            spinner.set_message("Waiting for token...");
        }
        loop {
            let res = client
                .post(self.provider().token_request_url())
                .header("content-type", "application/x-www-form-urlencoded")
                .form(&[
                    ("client_id", self.provider().client_id()),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                    ("device_code", &dc.device_code),
                ])
                .send()
                .await
                .map_err(|e| anyhow!(e.to_string()))?;
            match res.status() {
                StatusCode::OK => {
                    token = res
                        .json::<Auth0Token>()
                        .await
                        .map_err(|e| anyhow!(e.to_string()))?;
                    debug!(?token, "token response received");
                    if let Some(spinner) = spinner_option.as_ref() {
                        spinner.finish_and_clear();
                    }
                    opts.terminal.write_line(&fmt_log!("Token received!"))?;
                    return Ok(token);
                }
                _ => {
                    let err = res
                        .json::<TokensError>()
                        .await
                        .map_err(|e| anyhow!(e.to_string()))?;
                    match err.error.borrow() {
                        "authorization_pending" | "invalid_request" | "slow_down" => {
                            debug!(?err, "tokens not yet received");
                            sleep(Duration::from_secs(dc.interval as u64)).await;
                            continue;
                        }
                        _ => {
                            let err_msg = "failed to receive tokens";
                            debug!(?err, "{err_msg}");
                            return Err(anyhow!(err_msg).into());
                        }
                    }
                }
            }
        }
    }

    pub(crate) async fn validate_provider_config(&self) -> Result<()> {
        if let Err(e) = self.device_code().await {
            return Err(anyhow!("Invalid OIDC configuration: {e}").into());
        }
        Ok(())
    }
}

async fn update_enrolled_identity(opts: &CommandGlobalOpts, node_name: &str) -> Result<()> {
    let identities = opts.state.identities.list()?;

    let node_state = opts.state.nodes.get(node_name)?;
    let node_identity = node_state.config.default_identity().await?;

    for mut identity in identities {
        if node_identity.identifier() == identity.config.identifier {
            identity.set_enrollment_status()?;
        }
    }

    Ok(())
}