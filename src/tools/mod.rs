//! MCP tool registry.
//!
//! Mirrors mcp-cura's single execution choke point: authorization, approvals,
//! execution, and audit logging are centralized in [`ToolRegistry::call`].

pub mod admin;
pub mod args;
pub mod artifacts;
pub mod gcode;
pub mod geometry;
pub mod orca_config;
pub mod profiles;
pub mod slicing;

use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Instant};

use rust_mcp_sdk::schema::{
    CallToolResult, ContentBlock, TextContent, Tool, ToolAnnotations, ToolInputSchema,
};
use serde_json::{json, Value};

use crate::{
    audit::{AuditLogger, Outcome},
    config::Config,
    engine::Engine,
    error::{Error, Result},
    profiles::ProfileIndex,
    safety::{ApprovalStore, Capability, Decision, Policy, Risk},
    slicing::JobManager,
    workspace::ArtifactStore,
};

/// Boxed future returned by a tool handler.
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = Result<CallToolResult>> + Send + 'a>>;
/// A tool handler function pointer.
pub type ToolFn = for<'a> fn(&'a ToolContext, Value) -> ToolFuture<'a>;

/// Authorization path for a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gate {
    Policy,
    Internal,
    Confirm,
}

impl Gate {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Policy => "policy",
            Self::Internal => "local",
            Self::Confirm => "confirm",
        }
    }
}

/// Static description of one MCP tool.
#[derive(Clone, Copy)]
pub struct ToolSpec {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub allowed_args: &'static [&'static str],
    pub capability: Capability,
    pub risk: Risk,
    pub gate: Gate,
    pub summarize: fn(&Value) -> String,
    pub schema: fn() -> Value,
    pub handler: ToolFn,
}

impl ToolSpec {
    fn tool(&self) -> Tool {
        Tool {
            annotations: Some(ToolAnnotations {
                destructive_hint: Some(matches!(self.risk, Risk::High)),
                idempotent_hint: Some(false),
                open_world_hint: Some(false),
                read_only_hint: Some(matches!(self.capability, Capability::Read)),
                title: Some(self.title.to_string()),
            }),
            description: Some(self.description.to_string()),
            execution: None,
            icons: Vec::new(),
            input_schema: input_schema(&(self.schema)()),
            meta: None,
            name: self.name.to_string(),
            output_schema: None,
            title: Some(self.title.to_string()),
        }
    }
}

/// Shared services available to every tool.
pub struct ToolContext {
    pub config: Arc<Config>,
    pub store: Arc<ArtifactStore>,
    pub profiles: Arc<ProfileIndex>,
    pub engine: Arc<Engine>,
    pub jobs: Arc<JobManager>,
    pub policy: Arc<Policy>,
    pub approvals: Arc<ApprovalStore>,
    pub audit: Arc<AuditLogger>,
    pub process_slots: Arc<tokio::sync::Semaphore>,
}

impl ToolContext {
    pub fn new(config: Config) -> Result<Self> {
        let config = Arc::new(config);
        let store = Arc::new(ArtifactStore::open(&config.workspace)?);
        let profiles = Arc::new(ProfileIndex::open(config.clone())?);
        let engine = Arc::new(Engine::new(config.clone())?);
        let jobs = Arc::new(JobManager::new(
            store.clone(),
            engine.clone(),
            config.engine.max_concurrent_jobs,
            config.workspace.keep_jobs,
        ));
        let policy = Arc::new(Policy::new((*config).clone()));
        let approvals = Arc::new(ApprovalStore::new(
            crate::workspace_expand::workspace_path(&config.workspace.root).join("approvals"),
            config.approval.ttl_secs,
        )?);
        let audit = Arc::new(AuditLogger::new(&config));
        let process_slots = Arc::new(tokio::sync::Semaphore::new(
            config.engine.max_concurrent_jobs,
        ));
        Ok(Self {
            config,
            store,
            profiles,
            engine,
            jobs,
            policy,
            approvals,
            audit,
            process_slots,
        })
    }
}

/// Registry of all tools.
pub struct ToolRegistry {
    specs: Vec<ToolSpec>,
    index: BTreeMap<&'static str, usize>,
}

impl ToolRegistry {
    #[must_use]
    pub fn new() -> Self {
        let mut specs = Vec::new();
        specs.extend(admin::specs());
        specs.extend(artifacts::specs());
        specs.extend(geometry::specs());
        specs.extend(profiles::specs());
        specs.extend(orca_config::specs());
        specs.extend(slicing::specs());
        specs.extend(gcode::specs());

        let mut index = BTreeMap::new();
        for (i, spec) in specs.iter().enumerate() {
            index.insert(spec.name, i);
        }
        Self { specs, index }
    }

    #[must_use]
    pub fn list_tools(&self) -> Vec<Tool> {
        self.specs.iter().map(ToolSpec::tool).collect()
    }

    /// Static specs, for CLI introspection and generated documentation.
    #[must_use]
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    pub async fn call(
        &self,
        ctx: &ToolContext,
        name: &str,
        arguments: Option<Value>,
    ) -> Result<CallToolResult> {
        let Some(&idx) = self.index.get(name) else {
            return Err(Error::InvalidArguments(format!("unknown tool '{name}'")));
        };
        let spec = self.specs[idx];
        let args = arguments.unwrap_or(Value::Null);
        let start = Instant::now();
        match spec.gate {
            Gate::Confirm | Gate::Policy | Gate::Internal => {
                match ctx.policy.evaluate(spec.capability, spec.risk) {
                    Decision::Allow => {}
                    Decision::NeedsApproval => {
                        let fingerprint = action_fingerprint(spec.name, &args);
                        if ctx.approvals.consume(spec.name, &fingerprint)?.is_some() {
                            ctx.audit.record(
                                spec.name,
                                Outcome::Success,
                                Some("approval consumed"),
                            );
                        } else {
                            let request = ctx.approvals.request(
                                spec.name,
                                &fingerprint,
                                &format!("{} ({:?}/{:?})", spec.title, spec.capability, spec.risk),
                            )?;
                            ctx.audit
                                .record(spec.name, Outcome::Denied, Some("approval pending"));
                            return Err(Error::Approval(format!(
                                "tool '{}' requires approval. Approval request {} is pending; inspect it with orca_pending_approvals, decide with orca_approval_decide, then retry the identical call within the approval TTL.",
                                spec.name, request.id
                            )));
                        }
                    }
                    Decision::Deny => {
                        ctx.audit
                            .record(spec.name, Outcome::Denied, Some("policy denied"));
                        return Err(Error::Policy(format!(
                            "tool '{}' denied by policy",
                            spec.name
                        )));
                    }
                }
            }
        }

        let result = (spec.handler)(ctx, args).await;
        let elapsed_ms = start.elapsed().as_millis();
        match &result {
            Ok(_) => ctx.audit.record(
                spec.name,
                Outcome::Success,
                Some(&format!("{elapsed_ms}ms")),
            ),
            Err(err) => ctx
                .audit
                .record(spec.name, Outcome::Failed, Some(&err.public_message())),
        }
        result
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper to build a JSON result payload.
#[must_use]
pub fn json_result(value: &Value, summary: Option<String>) -> CallToolResult {
    let text = summary.unwrap_or_else(|| {
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    });
    let structured_content = match value {
        Value::Object(map) => Some(map.clone()),
        _ => None,
    };
    CallToolResult {
        content: vec![ContentBlock::TextContent(TextContent::new(
            text, None, None,
        ))],
        is_error: Some(false),
        meta: None,
        structured_content,
    }
}

/// Helper to build a text result.
#[must_use]
pub fn text_result(text: impl Into<String>) -> CallToolResult {
    CallToolResult {
        content: vec![ContentBlock::TextContent(TextContent::new(
            text.into(),
            None,
            None,
        ))],
        is_error: Some(false),
        meta: None,
        structured_content: None,
    }
}

/// Convert an error into a tool error result.
#[must_use]
pub fn tool_error(err: &Error) -> CallToolResult {
    CallToolResult {
        content: vec![ContentBlock::TextContent(TextContent::new(
            err.public_message(),
            None,
            None,
        ))],
        is_error: Some(true),
        meta: None,
        structured_content: None,
    }
}

#[must_use]
pub fn no_summary(_args: &Value) -> String {
    String::new()
}

#[must_use]
pub fn empty_schema() -> Value {
    schema_object(json!({}), &[])
}

#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn schema_object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

/// Stable fingerprint for an exact tool + argument combination.
#[must_use]
pub fn action_fingerprint(tool: &str, args: &Value) -> String {
    let canonical = serde_json::to_string(args).unwrap_or_else(|_| args.to_string());
    crate::workspace::sha256_hex(format!("{tool}\n{canonical}").as_bytes())
}

fn input_schema(schema: &Value) -> ToolInputSchema {
    let Value::Object(map) = schema else {
        return ToolInputSchema::new(Vec::new(), None, None);
    };
    let required = map
        .get("required")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let properties = map
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .iter()
                .map(|(k, v)| {
                    let schema_map = match v {
                        Value::Object(m) => m.clone(),
                        _ => serde_json::Map::new(),
                    };
                    (k.clone(), schema_map)
                })
                .collect::<BTreeMap<String, serde_json::Map<String, Value>>>()
        });
    ToolInputSchema::new(required, properties, None)
}
