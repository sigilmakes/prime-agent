//! Host side of MCP integrations. The protocol itself runs Python-side in the
//! kernel; the host only gates integration skills by auth and serves `mcp.*`
//! host requests (the OAuth flow lives in the `oauth*` submodules).

mod login;
mod oauth;
mod oauth_callback;
mod oauth_discovery;
mod oauth_http;

pub use login::{wire_begin_login, McpLoginContext, McpOAuth};
pub use oauth::{mcp_login, mcp_refresh_token, McpLoginUi, McpOAuthConfig};
pub use oauth_http::{OAuthHttp, OAuthHttpRequest, OAuthHttpResponse, ReqwestOAuthHttp};

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::auth::manager::AuthStorage;
use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// A built-in MCP integration we ship a skill package for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCatalogEntry {
    /// Matches the skill package import name and the `mcp:<server>` auth key.
    pub server: String,
    pub label: String,
    pub url: String,
}

/// The built-in MCP catalog (user servers go in `mcpServers` instead).
pub const BUILTIN_MCP_CATALOG: &[(&str, &str, &str)] = &[
    ("linear", "Linear", "https://mcp.linear.app/mcp"),
    ("notion", "Notion", "https://mcp.notion.com/mcp"),
];

#[must_use]
pub fn get_catalog_entry(server: &str) -> Option<McpCatalogEntry> {
    BUILTIN_MCP_CATALOG
        .iter()
        .find(|(name, _, _)| *name == server)
        .map(|(server, label, url)| McpCatalogEntry {
            server: server.to_string(),
            label: label.to_string(),
            url: url.to_string(),
        })
}

/// A user-declared MCP server config (the `mcpServers` setting).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum McpServerConfig {
    #[serde(rename_all = "camelCase")]
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,
        /// Env var holding a static bearer token (skips OAuth).
        #[serde(
            rename = "bearerTokenEnvVar",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        bearer_token_env_var: Option<String>,
        /// Use the generic OAuth login flow for this server.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        oauth: Option<bool>,
        /// Force-disable even when credentials exist.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        #[serde(
            rename = "enabledTools",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        enabled_tools: Option<Vec<String>>,
        #[serde(
            rename = "disabledTools",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        disabled_tools: Option<Vec<String>>,
        #[serde(
            rename = "startupTimeoutMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        startup_timeout_ms: Option<u64>,
        #[serde(
            rename = "callTimeoutMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        call_timeout_ms: Option<u64>,
        #[serde(rename = "clientId", default, skip_serializing_if = "Option::is_none")]
        client_id: Option<String>,
        #[serde(
            rename = "callbackPort",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        callback_port: Option<u16>,
    },
    #[serde(rename_all = "camelCase")]
    Stdio {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// Environment variables resolved from the kernel environment.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<HashMap<String, EnvRef>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        #[serde(
            rename = "enabledTools",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        enabled_tools: Option<Vec<String>>,
        #[serde(
            rename = "disabledTools",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        disabled_tools: Option<Vec<String>>,
        #[serde(
            rename = "startupTimeoutMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        startup_timeout_ms: Option<u64>,
        #[serde(
            rename = "callTimeoutMs",
            default,
            skip_serializing_if = "Option::is_none"
        )]
        call_timeout_ms: Option<u64>,
    },
}

impl McpServerConfig {
    #[must_use]
    pub fn server_type(&self) -> &'static str {
        match self {
            McpServerConfig::Http { .. } => "http",
            McpServerConfig::Stdio { .. } => "stdio",
        }
    }

    fn is_enabled(&self) -> bool {
        match self {
            McpServerConfig::Http { enabled, .. } | McpServerConfig::Stdio { enabled, .. } => {
                *enabled != Some(false)
            }
        }
    }
}

/// `{ "env": "<name>" }` stdio env reference.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
}

/// Session-scoped server supplied by the active ACP client: literal
/// environment values and headers, no settings-only fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AcpMcpServerConfig {
    Stdio {
        name: String,
        command: String,
        #[serde(default)]
        args: Vec<String>,
        cwd: String,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    Http {
        name: String,
        url: String,
        #[serde(default)]
        headers: HashMap<String, String>,
    },
}

impl AcpMcpServerConfig {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            AcpMcpServerConfig::Stdio { name, .. } | AcpMcpServerConfig::Http { name, .. } => name,
        }
    }
}

/// A resolved integration: built-in or user entry plus auth state.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedIntegration {
    pub(crate) server: String,
    pub(crate) label: String,
    pub(crate) config: McpServerConfig,
    pub(crate) uses_oauth: bool,
    /// True when this came from the `mcpServers` setting.
    pub(crate) user_declared: bool,
}

/// Options for constructing an [`McpManager`].
pub type BeginLoginFuture = Box<dyn Future<Output = anyhow::Result<()>> + Send>;
pub type BeginLoginFn = Arc<dyn Fn(String) -> BeginLoginFuture + Send + Sync>;

pub struct McpManagerOptions {
    pub auth_storage: AuthStorage,
    /// Reads the current `mcpServers` setting; re-read on refresh.
    pub get_user_servers: Box<dyn Fn() -> Option<HashMap<String, McpServerConfig>> + Send + Sync>,
    /// Start an interactive host-side login for a server (UI mode supplies it).
    pub begin_login: Option<BeginLoginFn>,
}

/// Host-side MCP manager: auth gating, config resolution, `mcp.*` host
/// requests, and ACP session servers.
pub struct McpManager {
    auth_storage: Arc<tokio::sync::Mutex<AuthStorage>>,
    get_user_servers: Box<dyn Fn() -> Option<HashMap<String, McpServerConfig>> + Send + Sync>,
    begin_login: Option<BeginLoginFn>,
    integrations: HashMap<String, ResolvedIntegration>,
    acp_servers: std::sync::Arc<std::sync::Mutex<HashMap<String, AcpMcpServerConfig>>>,
    acp_owner_id: std::sync::Mutex<Option<String>>,
}

fn provider_id(server: &str) -> String {
    format!("mcp:{server}")
}

fn uses_oauth(config: &McpServerConfig) -> bool {
    matches!(
        config,
        McpServerConfig::Http {
            oauth: Some(true),
            ..
        }
    )
}

fn oauth_credential_usable(
    credential: Option<&crate::auth::types::AuthCredential>,
    endpoint: &str,
) -> bool {
    let Some(crate::auth::types::AuthCredential::Oauth {
        access,
        refresh,
        expires,
        endpoint: Some(bound),
        ..
    }) = credential
    else {
        return false;
    };
    if access.is_empty() || bound != endpoint {
        return false;
    }
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or_default();
    *expires > now_ms || refresh.as_deref().is_some_and(|token| !token.is_empty())
}

fn http_config(url: &str, uses_oauth: bool) -> McpServerConfig {
    McpServerConfig::Http {
        url: url.to_string(),
        headers: None,
        bearer_token_env_var: None,
        oauth: uses_oauth.then_some(true),
        enabled: None,
        enabled_tools: None,
        disabled_tools: None,
        startup_timeout_ms: None,
        call_timeout_ms: None,
        client_id: None,
        callback_port: None,
    }
}

impl McpManager {
    pub fn new(options: McpManagerOptions) -> Self {
        let mut manager = Self {
            auth_storage: Arc::new(tokio::sync::Mutex::new(options.auth_storage)),
            get_user_servers: options.get_user_servers,
            begin_login: options.begin_login,
            integrations: HashMap::new(),
            acp_servers: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            acp_owner_id: std::sync::Mutex::new(None),
        };
        manager.resolve_integrations();
        manager
    }

    /// Re-read settings and re-resolve integrations; call after a reload.
    pub fn refresh(&mut self) {
        self.resolve_integrations();
    }

    /// Reload the shared auth store (blocking lock, never on the async
    /// runtime): a credential another process wrote — the interactive
    /// client's `/mcp` key flow stores through its own storage instance —
    /// becomes visible to the next view read, like the settings re-read
    /// above. The reload's failure surfaces to the caller: a malformed
    /// document or a lock that could not be acquired leaves the manager
    /// serving its previous credentials, and a successful reload must
    /// not be reported over that.
    ///
    /// # Errors
    ///
    /// Returns the storage reload's error when the document cannot be
    /// read or parsed (a malformed `auth.json`, an unacquirable lock):
    /// the manager keeps its previous credentials in that case.
    pub fn reload_auth_storage(&mut self) -> Result<(), String> {
        let storage = self.auth_storage.clone();
        let mut handle = storage.blocking_lock();
        handle.reload();
        match handle.load_error() {
            Some(error) => Err(error.to_string()),
            None => Ok(()),
        }
    }

    /// The resolved integrations (login resolution and status displays).
    pub(crate) fn integrations(&self) -> &HashMap<String, ResolvedIntegration> {
        &self.integrations
    }

    /// The shared auth store the login flow persists into.
    pub(crate) fn auth_storage_handle(&self) -> Arc<tokio::sync::Mutex<AuthStorage>> {
        Arc::clone(&self.auth_storage)
    }

    /// Wire (or clear) the interactive login behind the `mcp.begin_login`
    /// host request; hosts with a login UI set it before host handlers register.
    pub fn set_begin_login(&mut self, begin_login: Option<BeginLoginFn>) {
        self.begin_login = begin_login;
    }

    fn resolve_integrations(&mut self) {
        let mut integrations: HashMap<String, ResolvedIntegration> = HashMap::new();
        for (server, label, url) in BUILTIN_MCP_CATALOG {
            integrations.insert(
                (*server).to_string(),
                ResolvedIntegration {
                    server: (*server).to_string(),
                    label: (*label).to_string(),
                    config: http_config(url, true),
                    uses_oauth: true,
                    user_declared: false,
                },
            );
        }
        let user_servers = (self.get_user_servers)().unwrap_or_default();
        for (server, config) in user_servers {
            if get_catalog_entry(&server).is_some() {
                continue;
            }
            integrations.insert(
                server.clone(),
                ResolvedIntegration {
                    server: server.clone(),
                    label: server,
                    uses_oauth: uses_oauth(&config),
                    user_declared: true,
                    config,
                },
            );
        }
        self.integrations = integrations;
    }

    /// Whether ACP server configs owned by `owner_id` may be released.
    ///
    /// # Panics
    ///
    /// Panics if the ACP owner mutex is poisoned.
    pub fn can_release_acp_servers(&self, owner_id: &str) -> bool {
        self.acp_owner_id
            .lock()
            .unwrap()
            .as_deref()
            .is_none_or(|owner| owner == owner_id)
    }

    /// Replace the ACP-supplied session servers, fenced by owner. `false`
    /// when nothing changed (or the owner does not own what is cleared).
    ///
    /// # Errors
    ///
    /// Returns an error when `owner_id` is empty, or two servers share a name.
    ///
    /// # Panics
    ///
    /// Panics if the ACP owner or ACP server mutexes are poisoned.
    pub fn replace_acp_servers(
        &self,
        servers: &[AcpMcpServerConfig],
        owner_id: &str,
    ) -> anyhow::Result<bool> {
        if owner_id.is_empty() {
            anyhow::bail!("ACP MCP owner id is required");
        }
        let owner_fence = self.acp_owner_id.lock().unwrap().clone();
        if servers.is_empty() && owner_fence.as_deref() != Some(owner_id) {
            return Ok(false);
        }
        if !servers.is_empty()
            && owner_fence
                .as_deref()
                .is_some_and(|owner| owner != owner_id)
        {
            anyhow::bail!("ACP MCP configuration is owned by another client");
        }
        let mut next: HashMap<String, AcpMcpServerConfig> = HashMap::new();
        for server in servers {
            if next.contains_key(server.name()) {
                anyhow::bail!("Duplicate ACP MCP server: {}", server.name());
            }
            next.insert(server.name().to_string(), server.clone());
        }
        let mut acp_servers = self.acp_servers.lock().unwrap();
        let unchanged = next.len() == acp_servers.len()
            && next.iter().all(|(name, config)| {
                acp_servers.get(name).is_some_and(|current| {
                    serde_json::to_value(current).ok() == serde_json::to_value(config).ok()
                })
            });
        if unchanged {
            return Ok(false);
        }
        *acp_servers = next;
        *self.acp_owner_id.lock().unwrap() = (!servers.is_empty()).then(|| owner_id.to_string());
        Ok(true)
    }

    /// True when valid credentials exist for the integration (drives enablement).
    fn is_authed(&self, integration: &ResolvedIntegration) -> bool {
        if !integration.config.is_enabled() {
            return false;
        }
        if matches!(integration.config, McpServerConfig::Stdio { .. }) {
            return true;
        }
        let McpServerConfig::Http {
            url,
            bearer_token_env_var,
            ..
        } = &integration.config
        else {
            unreachable!("stdio handled above");
        };
        if let Some(env_var) = bearer_token_env_var {
            return std::env::var(env_var).is_ok_and(|value| !value.trim().is_empty());
        }
        let all = self.auth_storage_blocking_snapshot();
        let credential = all
            .get(&provider_id(&integration.server))
            .and_then(|value| {
                serde_json::from_value::<crate::auth::types::AuthCredential>(value.clone()).ok()
            });
        oauth_credential_usable(credential.as_ref(), url)
    }

    fn auth_storage_blocking_snapshot(&self) -> serde_json::Map<String, Value> {
        // The auth storage data map, read without refresh side effects.
        let storage = self.auth_storage.clone();
        let handle = storage.blocking_lock();
        let all = handle.get_all();
        match serde_json::to_value(&all) {
            Ok(Value::Object(map)) => map,
            _ => serde_json::Map::default(),
        }
    }

    /// Auth gating the system prompt and resource loader need: `-<server>/SKILL.md` overrides for
    /// built-ins the user is not logged into, plus the enabled persistent generic servers.
    #[must_use]
    pub fn prompt_gating(
        user_servers: std::collections::HashMap<String, McpServerConfig>,
        agent_dir: &std::path::Path,
    ) -> (Vec<String>, Vec<String>, McpManager) {
        let manager = McpManager::new(McpManagerOptions {
            auth_storage: crate::auth::AuthStorage::create_with_oauth(
                agent_dir,
                std::sync::Arc::new(McpOAuth::new()),
            ),
            get_user_servers: Box::new(move || Some(user_servers.clone())),
            begin_login: None,
        });
        (
            manager.get_disabled_builtin_skill_overrides(),
            manager.get_enabled_persistent_generic_servers(),
            manager,
        )
    }

    /// `-<server>/SKILL.md` overrides for every built-in integration the user
    /// is not logged into.
    pub fn get_disabled_builtin_skill_overrides(&self) -> Vec<String> {
        BUILTIN_MCP_CATALOG
            .iter()
            .filter_map(|(server, _, _)| {
                let integration = self.integrations.get(*server)?;
                (!self.is_authed(integration)).then(|| format!("-{server}/SKILL.md"))
            })
            .collect()
    }

    /// Register the `mcp.*` host-request handlers: `refresh`/`config`/`begin_login` dispatch
    /// plumbing plus the inventory surface the kernel's generic `mcp` module reaches for.
    ///
    /// The `config` and inventory handlers capture this manager so each request serves
    /// live views; inventory view computation reads the blocking auth-store snapshot.
    ///
    /// # Panics
    ///
    /// The registered handlers panic at request time if the manager or ACP server mutex
    /// is poisoned.
    pub fn register_host_handlers(
        manager: &Arc<std::sync::Mutex<Self>>,
        handlers: &mut HostRequestHandlers,
    ) {
        let (auth, acp_servers, acp_servers_for_config, begin_login) = {
            let manager = manager.lock().unwrap();
            (
                manager.auth_storage.clone(),
                manager.acp_servers.clone(),
                manager.acp_servers.clone(),
                manager.begin_login.clone(),
            )
        };
        handlers.register(
            "mcp.refresh",
            host_handler(move |payload| {
                let auth = auth.clone();
                let acp_servers = acp_servers.clone();
                Box::pin(async move {
                    let server = payload
                        .data
                        .get("server")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if server.is_empty() {
                        return Err(anyhow::anyhow!("mcp.refresh requires a server"));
                    }
                    if acp_servers.lock().unwrap().contains_key(&server) {
                        return Err(anyhow::anyhow!(
                            "ACP MCP server {server} does not use host OAuth"
                        ));
                    }
                    // Re-read the file first: a login from another process wrote the credential
                    // after this manager cached its store; the daemon worker's store must not serve
                    // the stale snapshot.
                    let mut store = auth.lock().await;
                    store.reload();
                    let key = store.get_api_key(&provider_id(&server));
                    drop(store);
                    if key.is_none() {
                        return Err(anyhow::anyhow!(
                            "Could not refresh credentials for {server}"
                        ));
                    }
                    Ok(json!({}))
                })
            }),
        );
        let manager_for_config = std::sync::Arc::clone(manager);
        handlers.register(
            "mcp.config",
            host_handler(move |payload| {
                let manager = std::sync::Arc::clone(&manager_for_config);
                let acp_servers = acp_servers_for_config.clone();
                Box::pin(async move {
                    let server = payload
                        .data
                        .get("server")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if server.is_empty() {
                        return Err(anyhow::anyhow!("mcp.config requires a server"));
                    }
                    let acp = acp_servers.lock().unwrap().get(&server).cloned();
                    if let Some(acp) = acp {
                        let mut config = serde_json::to_value(acp).unwrap_or(Value::Null);
                        if let Value::Object(map) = &mut config {
                            map.insert("credentialSource".to_string(), json!("acp"));
                        }
                        return Ok(config);
                    }
                    let integration = {
                        let manager = manager.lock().unwrap();
                        manager.integrations.get(&server).cloned()
                    };
                    let Some(integration) = integration else {
                        return Ok(json!({}));
                    };
                    serde_json::to_value(&integration.config).map_err(anyhow::Error::new)
                })
            }),
        );
        // Only expose begin_login when an interactive login is actually wired.
        if let Some(begin_login) = begin_login {
            handlers.register(
                "mcp.begin_login",
                host_handler(move |payload| {
                    let begin_login = begin_login.clone();
                    Box::pin(async move {
                        let server = payload
                            .data
                            .get("server")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if server.is_empty() {
                            return Err(anyhow::anyhow!("mcp.begin_login requires a server"));
                        }
                        let mut future = std::pin::Pin::from(begin_login(server));
                        future.as_mut().await?;
                        Ok(json!({}))
                    })
                }),
            );
        }
    }

    /// Session-scoped servers supplied by the active ACP client.
    ///
    /// # Panics
    ///
    /// Panics if the ACP server mutex is poisoned.
    pub fn get_acp_servers(&self) -> Vec<AcpMcpServerConfig> {
        self.acp_servers.lock().unwrap().values().cloned().collect()
    }

    /// Enabled user-declared servers available through the generic kernel API.
    pub fn get_enabled_persistent_generic_servers(&self) -> Vec<String> {
        let mut servers: Vec<String> = self
            .integrations
            .values()
            .filter(|integration| {
                integration.user_declared
                    && is_generic_server_name(&integration.server)
                    && self.is_authed(integration)
            })
            .map(|integration| integration.server.clone())
            .collect();
        servers.sort();
        servers
    }

    /// The connections roster for the `/mcp` connections view. Sorted by label.
    pub fn connection_roster(&self) -> Vec<McpConnectionEntry> {
        let generic: std::collections::HashSet<String> = self
            .get_enabled_persistent_generic_servers()
            .into_iter()
            .collect();
        let mut entries: Vec<McpConnectionEntry> = self
            .integrations
            .values()
            .map(|integration| {
                let transport = integration.config.server_type().to_string();
                let auth_kind = if integration.uses_oauth {
                    "subscription"
                } else if matches!(
                    &integration.config,
                    McpServerConfig::Http {
                        bearer_token_env_var: Some(_),
                        ..
                    }
                ) {
                    "api key"
                } else {
                    transport.as_str()
                };
                McpConnectionEntry {
                    server: integration.server.clone(),
                    label: integration.label.clone(),
                    connected: self.is_authed(integration),
                    uses_oauth: integration.uses_oauth,
                    auth_kind: auth_kind.to_string(),
                    transport,
                    user_declared: integration.user_declared,
                    generic: generic.contains(&integration.server),
                }
            })
            .collect();
        entries.sort_by_key(|entry| entry.label.to_lowercase());
        entries
    }

    /// Status for the `/mcp list` command.
    pub fn list_status(&self) -> Vec<McpServerStatus> {
        self.integrations
            .values()
            .map(|integration| McpServerStatus {
                server: integration.server.clone(),
                label: integration.label.clone(),
                enabled: self.is_authed(integration),
                uses_oauth: integration.uses_oauth,
            })
            .collect()
    }
}

/// One `/mcp list` row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub server: String,
    pub label: String,
    pub enabled: bool,
    pub uses_oauth: bool,
}

/// One `/mcp` connections-view row (the daemon's `get_mcp_connections` response).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
// The mirrored TS API shape is deliberate (the booleans are the
// product's own surface, not a refactor target).
#[allow(clippy::struct_excessive_bools)]
pub struct McpConnectionEntry {
    pub server: String,
    pub label: String,
    /// Connected: credentials present and the server enabled.
    pub connected: bool,
    pub uses_oauth: bool,
    /// The auth kind the view shows (`subscription` / `api key`), or the
    /// transport for credential-less stdio/http servers.
    pub auth_kind: String,
    /// The transport (`http` / `stdio`).
    pub transport: String,
    /// True for a user-declared `mcpServers` entry (false: built-in).
    pub user_declared: bool,
    /// True when the server surfaces through the generic kernel API (its
    /// tools are listable through `mcp.list_tools`).
    pub generic: bool,
}

fn is_generic_server_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut chars = name.chars();
    let first = chars.next().expect("checked non-empty");
    if !(first.is_ascii_alphanumeric()) {
        return false;
    }
    chars.all(|char| char.is_ascii_alphanumeric() || char == '_' || char == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_auth_storage() -> AuthStorage {
        AuthStorage::in_memory(
            &crate::auth::types::AuthStorageData::default(),
            std::sync::Arc::new(crate::auth::manager::NoOAuth),
        )
    }

    fn manager_with(user_servers: Option<HashMap<String, McpServerConfig>>) -> McpManager {
        McpManager::new(McpManagerOptions {
            auth_storage: test_auth_storage(),
            get_user_servers: Box::new(move || user_servers.clone()),
            begin_login: None,
        })
    }

    fn http_config(url: &str, oauth: Option<bool>, env_var: Option<&str>) -> McpServerConfig {
        McpServerConfig::Http {
            url: url.to_string(),
            headers: None,
            bearer_token_env_var: env_var.map(std::string::ToString::to_string),
            oauth,
            enabled: None,
            enabled_tools: None,
            disabled_tools: None,
            startup_timeout_ms: None,
            call_timeout_ms: None,
            client_id: None,
            callback_port: None,
        }
    }

    fn stdio_config(command: &str) -> McpServerConfig {
        McpServerConfig::Stdio {
            command: command.to_string(),
            args: None,
            cwd: None,
            env: None,
            enabled: None,
            enabled_tools: None,
            disabled_tools: None,
            startup_timeout_ms: None,
            call_timeout_ms: None,
        }
    }

    #[test]
    fn connection_roster_covers_builtins_and_user_servers() {
        let mut user_servers = HashMap::new();
        user_servers.insert("fixture-echo".to_string(), stdio_config("python3"));
        let manager = manager_with(Some(user_servers));
        let roster = manager.connection_roster();
        let names: Vec<&str> = roster.iter().map(|entry| entry.server.as_str()).collect();
        assert_eq!(names, vec!["fixture-echo", "linear", "notion"]);
        let fixture = &roster[0];
        assert!(fixture.connected);
        assert!(fixture.generic);
        assert!(fixture.user_declared);
        assert_eq!(fixture.auth_kind, "stdio");
        assert_eq!(fixture.transport, "stdio");
        let linear = roster
            .iter()
            .find(|entry| entry.server == "linear")
            .expect("linear");
        assert!(!linear.connected);
        assert!(!linear.generic);
        assert!(!linear.user_declared);
        assert_eq!(linear.auth_kind, "subscription");
        assert_eq!(linear.transport, "http");
    }

    #[test]
    fn builtin_catalog_and_skill_overrides() {
        let manager = manager_with(None);
        let overrides = manager.get_disabled_builtin_skill_overrides();
        assert_eq!(
            overrides,
            vec![
                "-linear/SKILL.md".to_string(),
                "-notion/SKILL.md".to_string()
            ]
        );
        let status = manager.list_status();
        assert_eq!(status.len(), 2);
        let linear = status.iter().find(|row| row.server == "linear").unwrap();
        assert!(!linear.enabled);
        assert!(linear.uses_oauth);
        assert_eq!(
            manager.get_enabled_persistent_generic_servers(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn stdio_and_env_token_servers_are_enabled() {
        let mut user_servers = HashMap::new();
        user_servers.insert("toolchain".to_string(), stdio_config("npx"));
        user_servers.insert(
            "search".to_string(),
            http_config("https://search.example/mcp", None, Some("SEARCH_TOKEN")),
        );
        let manager = manager_with(Some(user_servers));
        let generic = manager.get_enabled_persistent_generic_servers();
        assert!(generic.contains(&"toolchain".to_string()));
        assert!(!generic.contains(&"search".to_string()));
    }

    #[test]
    fn user_servers_cannot_reserve_builtin_names() {
        let mut user_servers = HashMap::new();
        user_servers.insert(
            "linear".to_string(),
            http_config("https://evil.example/mcp", Some(true), None),
        );
        let manager = manager_with(Some(user_servers));
        let linear = manager
            .connection_roster()
            .into_iter()
            .find(|entry| entry.server == "linear")
            .expect("linear");
        assert!(!linear.user_declared);
        assert_eq!(linear.label, "Linear");
        assert!(!manager
            .get_enabled_persistent_generic_servers()
            .contains(&"linear".to_string()));
    }

    #[test]
    fn generic_name_pattern() {
        assert!(is_generic_server_name("a"));
        assert!(is_generic_server_name("Tool_2-go"));
        assert!(!is_generic_server_name(""));
        assert!(!is_generic_server_name("-leading"));
        assert!(!is_generic_server_name("has space"));
        assert!(!is_generic_server_name(&"x".repeat(65)));
    }

    #[test]
    fn acp_server_ownership() {
        let manager = manager_with(None);
        assert!(manager.can_release_acp_servers("client-a"));
        let servers = vec![AcpMcpServerConfig::Stdio {
            name: "session-tool".to_string(),
            command: "run".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: HashMap::new(),
        }];
        assert!(manager.replace_acp_servers(&servers, "client-a").unwrap());
        assert!(!manager.replace_acp_servers(&servers, "client-a").unwrap());
        assert!(manager.replace_acp_servers(&servers, "client-b").is_err());
        assert!(!manager.can_release_acp_servers("client-b"));
        assert!(manager.can_release_acp_servers("client-a"));
        let duplicate = vec![servers[0].clone(), servers[0].clone()];
        assert!(manager.replace_acp_servers(&duplicate, "client-a").is_err());
        assert!(manager.replace_acp_servers(&[], "client-a").unwrap());
        assert_eq!(manager.get_acp_servers().len(), 0);
        assert!(manager.replace_acp_servers(&servers, "").is_err());
    }

    #[tokio::test]
    async fn config_host_handler_returns_user_stdio_server_config() {
        let mut user_servers = HashMap::new();
        user_servers.insert(
            "fixture-echo".to_string(),
            McpServerConfig::Stdio {
                command: "python3".to_string(),
                args: Some(vec!["fixtures/mcp_echo_server.py".to_string()]),
                cwd: None,
                env: None,
                enabled: None,
                enabled_tools: Some(vec!["echo".to_string()]),
                disabled_tools: None,
                startup_timeout_ms: None,
                call_timeout_ms: None,
            },
        );
        let manager = std::sync::Arc::new(std::sync::Mutex::new(manager_with(Some(user_servers))));
        let mut handlers = HostRequestHandlers::default();
        McpManager::register_host_handlers(&manager, &mut handlers);
        let config = handlers.get("mcp.config").unwrap().clone();
        let result = config(crate::kernel::shared::HostRequestPayload {
            data: json!({ "server": "fixture-echo" }),
        })
        .await
        .unwrap();
        assert_eq!(result["type"], "stdio");
        assert_eq!(result["command"], "python3");
        assert_eq!(result["args"], json!(["fixtures/mcp_echo_server.py"]));
        assert_eq!(result["enabledTools"], json!(["echo"]));
    }

    #[tokio::test]
    async fn config_host_handler_resolves_user_and_acp_servers() {
        let manager = std::sync::Arc::new(std::sync::Mutex::new(manager_with(None)));
        let mut handlers = HostRequestHandlers::default();
        McpManager::register_host_handlers(&manager, &mut handlers);
        let config = handlers.get("mcp.config").unwrap().clone();
        let result = config(crate::kernel::shared::HostRequestPayload {
            data: json!({ "server": "nope" }),
        })
        .await
        .unwrap();
        assert!(result.as_object().unwrap().is_empty());
        let servers = vec![AcpMcpServerConfig::Stdio {
            name: "session-tool".to_string(),
            command: "run".to_string(),
            args: vec![],
            cwd: "/tmp".to_string(),
            env: HashMap::new(),
        }];
        manager
            .lock()
            .unwrap()
            .replace_acp_servers(&servers, "client-a")
            .unwrap();
        let mut handlers = HostRequestHandlers::default();
        McpManager::register_host_handlers(&manager, &mut handlers);
        let config = handlers.get("mcp.config").unwrap().clone();
        let result = config(crate::kernel::shared::HostRequestPayload {
            data: json!({ "server": "session-tool" }),
        })
        .await
        .unwrap();
        assert_eq!(result["type"], "stdio");
        assert_eq!(result["credentialSource"], "acp");
        let error = config(crate::kernel::shared::HostRequestPayload { data: json!({}) })
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "mcp.config requires a server");
        assert!(handlers.get("mcp.begin_login").is_none());
    }
}
