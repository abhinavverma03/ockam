//! Node Manager (Node Man, the superhero that we deserve)

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use minicbor::Decoder;

use ockam::{Address, Context, ForwardingService, Result, Routed, TcpTransport, Worker};
use ockam_core::api::{Method, Request, Response, Status};
use ockam_core::compat::{boxed::Box, string::String};
use ockam_core::errcode::{Kind, Origin};
use ockam_identity::{Identity, IdentityIdentifier, PublicIdentity};
use ockam_multiaddr::MultiAddr;
use ockam_vault::storage::FileStorage;
use ockam_vault::Vault;

use super::registry::Registry;
use crate::config::{cli::AuthoritiesConfig, Config};
use crate::error::ApiError;
use crate::lmdb::LmdbStorage;
use crate::nodes::config::NodeManConfig;
use crate::nodes::models::base::NodeStatus;
use crate::nodes::models::transport::{TransportMode, TransportType};
use crate::DefaultAddress;

pub mod message;

mod credentials;
mod forwarder;
mod identity;
mod portals;
mod secure_channel;
mod services;
mod transport;
mod vault;

const TARGET: &str = "ockam_api::nodemanager::service";

pub(crate) type Alias = String;

/// Generate a new alias for some user created extension
#[inline]
fn random_alias() -> String {
    Address::random_local().without_type().to_owned()
}

// TODO: Move to multiaddr implementation
pub(crate) fn invalid_multiaddr_error() -> ockam_core::Error {
    ockam_core::Error::new(Origin::Core, Kind::Invalid, "Invalid multiaddr")
}

// TODO: Move to multiaddr implementation
pub(crate) fn map_multiaddr_err(_err: ockam_multiaddr::Error) -> ockam_core::Error {
    invalid_multiaddr_error()
}

fn map_anyhow_err(err: anyhow::Error) -> ockam_core::Error {
    ockam_core::Error::new(Origin::Application, Kind::Internal, err)
}

pub(crate) struct Authorities(Vec<AuthorityInfo>);

impl Authorities {
    pub fn new(authorities: Vec<AuthorityInfo>) -> Self {
        Self(authorities)
    }

    pub fn public_identities(&self) -> Vec<PublicIdentity> {
        self.0.iter().map(|x| x.identity.clone()).collect()
    }
}

impl AsRef<[AuthorityInfo]> for Authorities {
    fn as_ref(&self) -> &[AuthorityInfo] {
        self.0.as_ref()
    }
}

pub(crate) struct AuthorityInfo {
    identity: PublicIdentity,
    addr: MultiAddr,
}

/// Node manager provides a messaging API to interact with the current node
pub struct NodeManager {
    node_name: String,
    node_dir: PathBuf,
    config: Config<NodeManConfig>,
    api_transport_id: Alias,
    transports: BTreeMap<Alias, (TransportType, TransportMode, String)>,
    tcp_transport: TcpTransport,
    pub(crate) controller_identity_id: IdentityIdentifier,
    skip_defaults: bool,
    enable_credential_checks: bool,
    vault: Option<Vault>,
    identity: Option<Identity<Vault>>,
    project_id: Option<Vec<u8>>,
    authorities: Option<Authorities>,
    pub(crate) authenticated_storage: LmdbStorage,
    pub(crate) registry: Registry,
}

pub struct IdentityOverride {
    pub identity: Vec<u8>,
    pub vault_path: PathBuf,
}

impl NodeManager {
    pub(crate) fn identity(&self) -> Result<&Identity<Vault>> {
        self.identity
            .as_ref()
            .ok_or_else(|| ApiError::generic("Identity doesn't exist"))
    }

    pub(crate) fn vault(&self) -> Result<&Vault> {
        self.vault
            .as_ref()
            .ok_or_else(|| ApiError::generic("Vault doesn't exist"))
    }

    pub(crate) fn authorities(&self) -> Result<&Authorities> {
        self.authorities
            .as_ref()
            .ok_or_else(|| ApiError::generic("Authorities don't exist"))
    }

    /// Available only for member nodes
    pub(crate) fn project_id(&self) -> Result<&Vec<u8>> {
        self.project_id
            .as_ref()
            .ok_or_else(|| ApiError::generic("Project id is not set"))
    }
}

impl NodeManager {
    /// Create a new NodeManager with the node name from the ockam CLI
    #[allow(clippy::too_many_arguments)]
    pub async fn create(
        ctx: &Context,
        node_name: String,
        node_dir: PathBuf,
        // Should be passed only when creating fresh node and we want it to get default root Identity
        identity_override: Option<IdentityOverride>,
        skip_defaults: bool,
        enable_credential_checks: bool,
        ac: Option<&AuthoritiesConfig>,
        project_id: Option<Vec<u8>>,
        api_transport: (TransportType, TransportMode, String),
        tcp_transport: TcpTransport,
    ) -> Result<Self> {
        let api_transport_id = random_alias();
        let mut transports = BTreeMap::new();
        transports.insert(api_transport_id.clone(), api_transport);

        let config = Config::<NodeManConfig>::load(&node_dir, "config");

        // Check if we had existing AuthenticatedStorage, create with default location otherwise
        let authenticated_storage_path = config.readlock_inner().authenticated_storage_path.clone();
        let authenticated_storage = {
            let authenticated_storage_path = match authenticated_storage_path {
                Some(p) => p,
                None => {
                    let default_location = node_dir.join("authenticated_storage.lmdb");

                    config.writelock_inner().authenticated_storage_path =
                        Some(default_location.clone());
                    config.persist_config_updates().map_err(map_anyhow_err)?;

                    default_location
                }
            };
            LmdbStorage::new(&authenticated_storage_path).await?
        };

        // Skip override if we already had vault
        if config.readlock_inner().vault_path.is_none() {
            if let Some(identity_override) = identity_override {
                // Copy vault file, update config
                let vault_path = Self::default_vault_path(&node_dir);
                std::fs::copy(&identity_override.vault_path, &vault_path)
                    .map_err(|_| ApiError::generic("Error while copying default node"))?;

                config.writelock_inner().vault_path = Some(vault_path);
                config.writelock_inner().identity = Some(identity_override.identity);
                config.writelock_inner().identity_was_overridden = true;

                config.persist_config_updates().map_err(map_anyhow_err)?;
            }
        }

        // Check if we had existing Vault
        let vault_path = config.readlock_inner().vault_path.clone();
        let vault = match vault_path {
            Some(vault_path) => {
                let vault_storage = FileStorage::create(vault_path).await?;
                let vault = Vault::new(Some(Arc::new(vault_storage)));

                Some(vault)
            }
            None => None,
        };

        // Check if we had existing Identity
        let identity_info = config.readlock_inner().identity.clone();
        let identity = match identity_info {
            Some(identity) => match vault.as_ref() {
                Some(vault) => Some(Identity::import(ctx, &identity, vault).await?),
                None => None,
            },
            None => None,
        };

        if enable_credential_checks && (ac.is_none() || project_id.is_none()) {
            error!("Invalid NodeManager options: enable_credential_checks was provided, while not enough \
                information was provided to enforce the checks");
            return Err(ockam_core::Error::new(
                Origin::Ockam,
                Kind::Invalid,
                "Invalid NodeManager options",
            ));
        }

        let mut s = Self {
            node_name,
            node_dir,
            config,
            api_transport_id,
            transports,
            tcp_transport,
            controller_identity_id: Self::load_controller_identity_id()?,
            skip_defaults,
            enable_credential_checks,
            vault,
            identity,
            project_id,
            authorities: None,
            authenticated_storage,
            registry: Default::default(),
        };

        if !skip_defaults {
            s.create_defaults(ctx).await?;

            if let Some(ac) = ac {
                s.configure_authorities(ac).await?;
            }
        }

        Ok(s)
    }

    async fn configure_authorities(&mut self, ac: &AuthoritiesConfig) -> Result<()> {
        let vault = self.vault()?;

        let mut v = Vec::new();

        for a in ac.authorities() {
            v.push(AuthorityInfo {
                identity: PublicIdentity::import(a.1.identity(), vault).await?,
                addr: a.1.access_route().clone(),
            })
        }

        self.authorities = Some(Authorities::new(v));

        Ok(())
    }

    async fn create_defaults(&mut self, ctx: &Context) -> Result<()> {
        // Create default vault and identity, if they don't exists already
        self.create_vault_impl(None, true).await?;
        self.create_identity_impl(ctx, true).await?;

        Ok(())
    }

    async fn initialize_defaults(&mut self, ctx: &Context) -> Result<()> {
        // Start services
        self.start_vault_service_impl(ctx, DefaultAddress::VAULT_SERVICE.into())
            .await?;
        self.start_identity_service_impl(ctx, DefaultAddress::IDENTITY_SERVICE.into())
            .await?;
        self.start_authenticated_service_impl(ctx, DefaultAddress::AUTHENTICATED_SERVICE.into())
            .await?;
        self.start_uppercase_service_impl(ctx, DefaultAddress::UPPERCASE_SERVICE.into())
            .await?;
        self.start_echoer_service_impl(ctx, DefaultAddress::ECHO_SERVICE.into())
            .await?;

        ForwardingService::create(ctx).await?;

        self.create_secure_channel_listener_impl(
            DefaultAddress::SECURE_CHANNEL_LISTENER.into(),
            None, // Not checking identifiers here in favor of credentials check
        )
        .await?;

        // If we've been configured with authorities, we can start Credentials Exchange service
        if self.authorities().is_ok() {
            self.start_credentials_service_impl(DefaultAddress::CREDENTIAL_SERVICE.into(), false)
                .await?;
        }

        Ok(())
    }
}

impl NodeManager {
    //////// Request matching and response handling ////////

    async fn handle_request(
        &mut self,
        ctx: &mut Context,
        req: &Request<'_>,
        dec: &mut Decoder<'_>,
    ) -> Result<Vec<u8>> {
        trace! {
            target: TARGET,
            id     = %req.id(),
            method = ?req.method(),
            path   = %req.path(),
            body   = %req.has_body(),
            "request"
        }

        use Method::*;
        let path = req.path();
        let path_segments = req.path_segments::<5>();
        let method = match req.method() {
            Some(m) => m,
            None => todo!(),
        };

        let r = match (method, path_segments.as_slice()) {
            // ==*== Basic node information ==*==
            // TODO: create, delete, destroy remote nodes
            (Get, ["node"]) => Response::ok(req.id())
                .body(NodeStatus::new(
                    self.node_name.as_str(),
                    "Running",
                    ctx.list_workers().await?.len() as u32,
                    std::process::id() as i32,
                    self.transports.len() as u32,
                ))
                .to_vec()?,

            // ==*== Tcp Connection ==*==
            // TODO: Get all tcp connections
            (Get, ["node", "tcp", "connection"]) => self
                .get_tcp_con_or_list(req, TransportMode::Connect)
                .to_vec()?,
            (Post, ["node", "tcp", "connection"]) => {
                self.add_transport(req, dec).await?.to_vec()?
            }
            (Delete, ["node", "tcp", "connection"]) => {
                self.delete_transport(req, dec).await?.to_vec()?
            }

            // ==*== Tcp Listeners ==*==
            (Get, ["node", "tcp", "listener"]) => self
                .get_tcp_con_or_list(req, TransportMode::Listen)
                .to_vec()?,
            (Post, ["node", "tcp", "listener"]) => self.add_transport(req, dec).await?.to_vec()?,
            (Delete, ["node", "tcp", "listener"]) => {
                self.delete_transport(req, dec).await?.to_vec()?
            }

            // ==*== Vault ==*==
            (Post, ["node", "vault"]) => self.create_vault(req, dec).await?.to_vec()?,

            // ==*== Identity ==*==
            (Post, ["node", "identity"]) => self.create_identity(ctx, req).await?.to_vec()?,
            (Post, ["node", "identity", "actions", "show", "short"]) => {
                self.short_identity(req).await?.to_vec()?
            }
            (Post, ["node", "identity", "actions", "show", "long"]) => {
                self.long_identity(req).await?.to_vec()?
            }

            // ==*== Credentials ==*==
            (Post, ["node", "credentials", "actions", "get"]) => {
                self.get_credential(req, dec).await?.to_vec()?
            }
            (Post, ["node", "credentials", "actions", "present"]) => {
                self.present_credential(req, dec).await?.to_vec()?
            }

            // ==*== Secure channels ==*==
            // TODO: Change to RequestBuilder format
            (Get, ["node", "secure_channel"]) => self.list_secure_channels(req).to_vec()?,
            (Get, ["node", "secure_channel_listener"]) => {
                self.list_secure_channel_listener(req).to_vec()?
            }
            (Post, ["node", "secure_channel"]) => {
                self.create_secure_channel(req, dec).await?.to_vec()?
            }
            (Delete, ["node", "secure_channel"]) => {
                self.delete_secure_channel(req, dec).await?.to_vec()?
            }
            (Get, ["node", "show_secure_channel"]) => {
                self.show_secure_channel(req, dec).await?.to_vec()?
            }
            (Post, ["node", "secure_channel_listener"]) => self
                .create_secure_channel_listener(req, dec)
                .await?
                .to_vec()?,

            // ==*== Services ==*==
            (Post, ["node", "services", "vault"]) => {
                self.start_vault_service(ctx, req, dec).await?.to_vec()?
            }
            (Post, ["node", "services", "identity"]) => {
                self.start_identity_service(ctx, req, dec).await?.to_vec()?
            }
            (Post, ["node", "services", "authenticated"]) => self
                .start_authenticated_service(ctx, req, dec)
                .await?
                .to_vec()?,
            (Post, ["node", "services", "uppercase"]) => self
                .start_uppercase_service(ctx, req, dec)
                .await?
                .to_vec()?,
            (Post, ["node", "services", "echo"]) => {
                self.start_echoer_service(ctx, req, dec).await?.to_vec()?
            }
            (Post, ["node", "services", "authenticator"]) => self
                .start_authenticator_service(ctx, req, dec)
                .await?
                .to_vec()?,
            (Post, ["node", "services", "verifier"]) => {
                self.start_verifier_service(ctx, req, dec).await?.to_vec()?
            }
            (Post, ["node", "services", "credentials"]) => self
                .start_credentials_service(ctx, req, dec)
                .await?
                .to_vec()?,

            // ==*== Forwarder commands ==*==
            (Post, ["node", "forwarder"]) => self.create_forwarder(ctx, req, dec).await?,

            // ==*== Inlets & Outlets ==*==
            (Get, ["node", "inlet"]) => self.get_inlets(req).to_vec()?,
            (Get, ["node", "outlet"]) => self.get_outlets(req).to_vec()?,
            (Post, ["node", "inlet"]) => self.create_inlet(req, dec).await?.to_vec()?,
            (Post, ["node", "outlet"]) => self.create_outlet(req, dec).await?.to_vec()?,
            (Delete, ["node", "portal"]) => todo!(),

            // ==*== Spaces ==*==
            (Post, ["v0", "spaces"]) => self.create_space(ctx, dec).await?,
            (Get, ["v0", "spaces"]) => self.list_spaces(ctx, dec).await?,
            (Get, ["v0", "spaces", id]) => self.get_space(ctx, dec, id).await?,
            (Delete, ["v0", "spaces", id]) => self.delete_space(ctx, dec, id).await?,

            // ==*== Project' enrollers ==*==
            (Post, ["v0", "project-enrollers", project_id]) => {
                self.add_project_enroller(ctx, dec, project_id).await?
            }
            (Get, ["v0", "project-enrollers", project_id]) => {
                self.list_project_enrollers(ctx, dec, project_id).await?
            }
            (Delete, ["v0", "project-enrollers", project_id, identity_id]) => {
                self.delete_project_enroller(ctx, dec, project_id, identity_id)
                    .await?
            }

            // ==*== Projects ==*==
            (Post, ["v0", "projects", space_id]) => self.create_project(ctx, dec, space_id).await?,
            (Get, ["v0", "projects"]) => self.list_projects(ctx, dec).await?,
            (Get, ["v0", "projects", project_id]) => self.get_project(ctx, dec, project_id).await?,
            (Delete, ["v0", "projects", space_id, project_id]) => {
                self.delete_project(ctx, dec, space_id, project_id).await?
            }

            // ==*== Enroll ==*==
            (Post, ["v0", "enroll", "auth0"]) => self.enroll_auth0(ctx, dec).await?,
            (Get, ["v0", "enroll", "token"]) => self.generate_enrollment_token(ctx, dec).await?,
            (Put, ["v0", "enroll", "token"]) => {
                self.authenticate_enrollment_token(ctx, dec).await?
            }

            // ==*== Subscriptions ==*==
            (Post, ["subscription"]) => self.activate_subscription(ctx, dec).await?,
            (Get, ["subscription", id]) => self.get_subscription(ctx, dec, id).await?,
            (Get, ["subscription"]) => self.list_subscriptions(ctx, dec).await?,
            (Put, ["subscription", id, "contact_info"]) => {
                self.update_subscription_contact_info(ctx, dec, id).await?
            }
            (Put, ["subscription", id, "space_id"]) => {
                self.update_subscription_space(ctx, dec, id).await?
            }
            (Put, ["subscription", id, "unsubscribe"]) => self.unsubscribe(ctx, dec, id).await?,

            // ==*== Messages ==*==
            (Post, ["v0", "message"]) => self.send_message(ctx, req, dec).await?,

            // ==*== Catch-all for Unimplemented APIs ==*==
            _ => {
                warn!(%method, %path, "Called invalid endpoint");
                Response::bad_request(req.id())
                    .body(format!("Invalid endpoint: {}", path))
                    .to_vec()?
            }
        };
        Ok(r)
    }
}

#[ockam::worker]
impl Worker for NodeManager {
    type Message = Vec<u8>;
    type Context = Context;

    async fn initialize(&mut self, ctx: &mut Context) -> Result<()> {
        if !self.skip_defaults {
            self.initialize_defaults(ctx).await?;
        }

        Ok(())
    }

    async fn handle_message(&mut self, ctx: &mut Context, msg: Routed<Vec<u8>>) -> Result<()> {
        let mut dec = Decoder::new(msg.as_body());
        let req: Request = match dec.decode() {
            Ok(r) => r,
            Err(e) => {
                error!("Failed to decode request: {:?}", e);
                return Ok(());
            }
        };

        let r = match self.handle_request(ctx, &req, &mut dec).await {
            Ok(r) => r,
            // If an error occurs, send a response with the error code so the listener can
            // fail fast instead of failing silently here and force the listener to timeout.
            Err(err) => {
                error!(?err, "Failed to handle request");
                Response::builder(req.id(), Status::InternalServerError)
                    .body(format!("Failed to handle request: {err}"))
                    .to_vec()?
            }
        };
        trace!("** sending response");
        ctx.send(msg.return_route(), r).await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::nodes::NodeManager;
    use ockam::{route, Route};

    use super::*;

    impl NodeManager {
        pub(crate) async fn test_create(ctx: &Context) -> Result<Route> {
            let node_dir = tempfile::tempdir().unwrap();
            let node_manager = "manager";
            let transport = TcpTransport::create(ctx).await?;
            let node_address = transport.listen("127.0.0.1:0").await?;
            let mut node_man = NodeManager::create(
                ctx,
                "node".to_string(),
                node_dir.into_path(),
                None,
                true,
                false,
                None,
                None,
                (
                    TransportType::Tcp,
                    TransportMode::Listen,
                    node_address.to_string(),
                ),
                transport,
            )
            .await?;

            // Initialize identity
            node_man.create_vault_impl(None, false).await?;
            node_man.create_identity_impl(ctx, false).await?;

            // Initialize node_man worker and return its route
            ctx.start_worker(node_manager, node_man).await?;
            Ok(route![node_manager])
        }
    }
}
