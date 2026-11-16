//! Transport startup: stdio (default) and optional streamable HTTP/SSE.

use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use rust_mcp_axum::{create_axum_server, AxumServerOptions};
use rust_mcp_sdk::{
    auth::{AuthInfo, AuthProvider, AuthenticationError, OauthEndpoint},
    event_store::InMemoryEventStore,
    mcp_http::{DnsRebindingOptions, GenericBody, GenericBodyExt, McpAppState, McpHttpError},
    mcp_server::{server_runtime, McpServerOptions, ToMcpServerHandler},
    schema::InitializeResult,
    McpServer, StdioTransport, TransportOptions,
};
use secrecy::{ExposeSecret, SecretString};

use crate::{
    config::Config,
    error::{Error, Result},
    server::OrcaServerHandler,
    workspace::sha256_hex,
};

/// Static bearer-token auth provider for the optional HTTP transport.
///
/// This is intentionally simple: one configured shared secret, compared in
/// constant time. OAuth endpoints are not offered.
#[derive(Debug, Clone)]
pub struct StaticTokenAuthProvider {
    token: SecretString,
}

impl StaticTokenAuthProvider {
    #[must_use]
    pub fn new(token: SecretString) -> Self {
        Self { token }
    }
}

#[async_trait]
impl AuthProvider for StaticTokenAuthProvider {
    async fn verify_token(
        &self,
        access_token: String,
    ) -> std::result::Result<AuthInfo, AuthenticationError> {
        if constant_time_eq(
            access_token.as_bytes(),
            self.token.expose_secret().as_bytes(),
        ) {
            Ok(AuthInfo {
                token_unique_id: sha256_hex(access_token.as_bytes()),
                client_id: Some("mcp-orca-static-token".into()),
                user_id: None,
                scopes: None,
                // rust-mcp-sdk's auth middleware rejects tokens without an
                // expiration. Static operator-configured tokens do not expire,
                // so report a far-future timestamp rather than `None`.
                expires_at: Some(
                    std::time::SystemTime::now()
                        + std::time::Duration::from_secs(100 * 365 * 24 * 60 * 60),
                ),
                audience: None,
                extra: None,
            })
        } else {
            Err(AuthenticationError::InvalidToken {
                description: "invalid bearer token",
            })
        }
    }

    fn auth_endpoints(&self) -> Option<&HashMap<String, OauthEndpoint>> {
        None
    }

    async fn handle_request(
        &self,
        _request: http::Request<&str>,
        _state: Arc<McpAppState>,
    ) -> std::result::Result<http::Response<GenericBody>, McpHttpError> {
        Ok(GenericBody::create_404_response())
    }

    fn protected_resource_metadata_url(&self) -> Option<&str> {
        None
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Run the MCP server over stdio until the client disconnects.
pub async fn run_stdio(
    _config: &Config,
    handler: OrcaServerHandler,
    server_details: InitializeResult,
) -> Result<()> {
    let transport = StdioTransport::new(TransportOptions::default())
        .map_err(|err| Error::Config(format!("could not start stdio transport: {err}")))?;
    let server = server_runtime::create_server(McpServerOptions {
        server_details,
        transport,
        handler: handler.to_mcp_server_handler(),
        task_store: None,
        client_task_store: None,
        message_observer: None,
    });
    server
        .start()
        .await
        .map_err(|err| Error::Config(format!("MCP stdio server stopped: {err}")))?;
    Ok(())
}

/// Run the MCP server over streamable HTTP (with SSE compatibility).
pub async fn run_http(
    config: &Config,
    handler: OrcaServerHandler,
    server_details: InitializeResult,
) -> Result<()> {
    let http = &config.server.http;
    let auth: Option<Arc<dyn AuthProvider>> = http
        .token
        .clone()
        .map(|token| Arc::new(StaticTokenAuthProvider::new(token)) as Arc<dyn AuthProvider>);
    let authenticated = auth.is_some();

    let allowed_hosts = (!http.allowed_hosts.is_empty()).then(|| http.allowed_hosts.clone());
    let allowed_origins = (!http.allowed_origins.is_empty()).then(|| http.allowed_origins.clone());

    let options = AxumServerOptions {
        host: http.host.clone(),
        port: http.port,
        custom_streamable_http_endpoint: Some(http.path.clone()),
        custom_sse_endpoint: Some("/sse".to_string()),
        custom_messages_endpoint: Some("/messages".to_string()),
        sse_support: true,
        health_endpoint: Some("/health".to_string()),
        max_request_body_size: Some(4 * 1024 * 1024),
        enable_json_response: Some(false),
        event_store: Some(Arc::new(InMemoryEventStore::default())),
        auth,
        dns_rebinding: DnsRebindingOptions {
            allowed_hosts,
            allowed_origins,
            ..Default::default()
        },
        ..Default::default()
    };

    tracing::info!(
        target: "mcp_orca::transport",
        host = %http.host,
        port = http.port,
        path = %http.path,
        authenticated = authenticated,
        "starting streamable HTTP transport"
    );

    let server = create_axum_server(server_details, handler.to_mcp_server_handler(), options);
    server
        .start()
        .await
        .map_err(|err| Error::Config(format!("MCP HTTP server stopped: {err}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn static_token_accepts_only_exact_match() {
        let provider = StaticTokenAuthProvider::new(SecretString::from("s3cret".to_string()));
        let info = provider.verify_token("s3cret".to_string()).await.unwrap();
        assert_eq!(info.client_id.as_deref(), Some("mcp-orca-static-token"));
        assert!(provider.verify_token("wrong".to_string()).await.is_err());
        assert!(provider.verify_token("s3cre".to_string()).await.is_err());
    }

    #[test]
    fn constant_time_eq_matches_semantics() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
