//! Transport-neutral operations shared by the HTTP API and MCP tools.
use axum::{
    Json, Router,
    extract::{ConnectInfo, Extension, State},
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::{
    execution::{PrivilegeLevel, RunOptions},
    mcp_text::{INVALID_TIMEOUT, SERIALIZATION_FAILED, missing_string_argument, tool_definitions},
    server::{ApiError, AppState},
};

mod llms;

#[derive(Clone, Copy)]
pub(crate) enum Operation {
    SecretsList,
    SecretsGet,
    EnterInteractive,
    Run,
    Destroy,
}

impl Operation {
    pub(crate) fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "secrets_list" => Self::SecretsList,
            "secrets_get" => Self::SecretsGet,
            "enter_interactive" | "localshelld_enter_interactive" => Self::EnterInteractive,
            "run" | "localshelld_run" => Self::Run,
            "destroy" | "localshelld_destroy_session" => Self::Destroy,
            _ => return None,
        })
    }
    fn path(self) -> &'static str {
        match self {
            Self::SecretsList => "/v1/secrets/keys",
            Self::SecretsGet => "/v1/secrets/get",
            Self::EnterInteractive => "/v1/sessions/enter/interactive",
            Self::Run => "/v1/commands/run",
            Self::Destroy => "/v1/sessions/destroy",
        }
    }
    fn fields(self) -> &'static [&'static str] {
        match self {
            Self::SecretsList => &["query"],
            Self::SecretsGet => &["handle", "key"],
            Self::EnterInteractive => &[
                "agent_name",
                "request_reason",
                "confirm_text",
                "secret_keys",
            ],
            // The platform-specific validator rejects the other platform's
            // identity argument with an explicit error, even if both are sent.
            Self::Run => &[
                "handle",
                "command",
                "privilege_level",
                "sudo",
                "stdin",
                "timeout_seconds",
            ],
            Self::Destroy => &["handle"],
        }
    }
}

pub(crate) fn routes() -> Router<AppState> {
    let mut routes = Router::new()
        .route("/llms.txt", get(llms::document))
        .route("/v1/capabilities", get(capabilities));
    for operation in [
        Operation::SecretsList,
        Operation::SecretsGet,
        Operation::EnterInteractive,
        Operation::Run,
        Operation::Destroy,
    ] {
        routes = routes.route(
            operation.path(),
            post(
                move |State(state): State<AppState>,
                      peer: Option<Extension<ConnectInfo<std::net::SocketAddr>>>,
                      Json(arguments): Json<Value>| async move {
                    execute_from(&state, operation, arguments, peer.map(|p| p.0.0))
                        .await
                        .map(Json)
                },
            ),
        );
    }
    routes
}

fn operation_definitions() -> Value {
    let mut operations = tool_definitions();
    for entry in operations.as_array_mut().expect("static tool definitions") {
        let operation = Operation::from_name(entry["name"].as_str().expect("tool name"))
            .expect("every advertised tool has an HTTP operation");
        entry["method"] = json!("POST");
        entry["path"] = json!(operation.path());
    }
    operations
}

async fn capabilities(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "protocol":1,"service":"localshelld","product":"trustd","version":crate::VERSION,"commit":crate::COMMIT,
        "daemon":state.mode,"platform":std::env::consts::OS,
        "operations":operation_definitions(),
        "cancellation":{"method":"POST","path":Operation::Destroy.path(),"scope":"session","disconnect_cancels":false},
        "interactive_timeout_ms":crate::consent::CONSENT_TIMEOUT_MS
    }))
}

pub(crate) async fn execute_from(
    state: &AppState,
    operation: Operation,
    arguments: Value,
    peer: Option<std::net::SocketAddr>,
) -> Result<Value, ApiError> {
    let object = arguments
        .as_object()
        .ok_or_else(|| ApiError::bad_request("arguments must be a JSON object"))?;
    if object
        .keys()
        .any(|key| !operation.fields().contains(&key.as_str()))
    {
        // Do not echo supplied names or values: they may contain credentials.
        return Err(ApiError::bad_request(
            "unknown argument; check the operation input schema",
        ));
    }
    match operation {
        Operation::SecretsList => {
            // Validate before reading the local vault or calling its paired owner.
            let query = secret_query_arg(&arguments)?;
            let keys: Vec<_> = state
                .secret_keys()
                .await?
                .into_iter()
                .filter(|key| key.to_ascii_lowercase().contains(&query))
                .collect();
            Ok(json!({"keys":keys}))
        }
        Operation::SecretsGet => {
            let handle = string_arg(&arguments, "handle")?;
            let key = string_arg(&arguments, "key")?;
            Ok(json!({"key":key,"value":state.secret_value(handle, key).await?}))
        }
        Operation::EnterInteractive => {
            confirmation(&arguments)?;
            state
                .enter_interactive_for_agent(
                    string_arg(&arguments, "agent_name")?,
                    string_arg(&arguments, "request_reason")?,
                    &secret_keys_arg(&arguments)?,
                    state.caller_identity(peer).await,
                )
                .await
        }
        Operation::Run => {
            let handle = string_arg(&arguments, "handle")?;
            let command = string_arg(&arguments, "command")?;
            let privilege_level = privilege_arg(&arguments)?;
            let stdin = match arguments.get("stdin") {
                None => String::new(),
                Some(Value::String(value)) => value.clone(),
                _ => {
                    return Err(ApiError::bad_request(
                        "stdin must be a string; omit it for EOF",
                    ));
                }
            };
            let result = state
                .run_command(
                    handle,
                    command,
                    privilege_level,
                    RunOptions {
                        stdin,
                        timeout_seconds: timeout_arg(&arguments)?,
                    },
                )
                .await?;
            serde_json::to_value(result).map_err(|_| ApiError::internal(SERIALIZATION_FAILED))
        }
        Operation::Destroy => {
            state
                .destroy_session(string_arg(&arguments, "handle")?)
                .await?;
            Ok(json!({"destroyed":true}))
        }
    }
}

fn confirmation(arguments: &Value) -> Result<(), ApiError> {
    if string_arg(arguments, "confirm_text")? != "OK" {
        return Err(ApiError::bad_request("confirm_text must equal OK"));
    }
    Ok(())
}

pub(crate) fn secret_query_arg(arguments: &Value) -> Result<String, ApiError> {
    let query = string_arg(arguments, "query")?;
    if query.trim().is_empty() {
        return Err(ApiError::bad_request(
            "query must not be empty or whitespace-only",
        ));
    }
    // Keys are restricted to ASCII environment-variable names. Preserve spaces
    // and punctuation: search is a literal substring, not a pattern or prefix.
    Ok(query.to_ascii_lowercase())
}

pub(crate) fn secret_keys_arg(arguments: &Value) -> Result<Vec<String>, ApiError> {
    let Some(value) = arguments.get("secret_keys") else {
        return Ok(vec![]);
    };
    let keys: Vec<String> = serde_json::from_value(value.clone()).map_err(|_| {
        ApiError::bad_request(
            "secret_keys must be an array of uppercase environment variable names",
        )
    })?;
    crate::secrets::normalize_keys(&keys)
}

pub(crate) fn privilege_arg(arguments: &Value) -> Result<PrivilegeLevel, ApiError> {
    #[cfg(windows)]
    {
        if arguments.get("sudo").is_some() {
            return Err(ApiError::bad_request(
                "Windows requires privilege_level instead of sudo",
            ));
        }
        match arguments.get("privilege_level").and_then(Value::as_str) {
            Some("current-user") => Ok(PrivilegeLevel::CurrentUser),
            Some("as-administrator") => Ok(PrivilegeLevel::Administrator),
            Some("nt-authority/system") => Ok(PrivilegeLevel::System),
            _ => Err(ApiError::bad_request(
                "privilege_level must be current-user, as-administrator, or nt-authority/system",
            )),
        }
    }
    #[cfg(not(windows))]
    {
        if arguments.get("privilege_level").is_some() {
            return Err(ApiError::bad_request(
                "this platform requires boolean sudo, not privilege_level",
            ));
        }
        arguments
            .get("sudo")
            .and_then(Value::as_bool)
            .map(PrivilegeLevel::from_sudo)
            .ok_or_else(|| ApiError::bad_request("sudo must be an explicit boolean"))
    }
}

fn string_arg<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, ApiError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::bad_request(missing_string_argument(name)))
}

pub(crate) fn timeout_arg(arguments: &Value) -> Result<Option<u64>, ApiError> {
    match arguments.get("timeout_seconds") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|seconds| *seconds > 0)
            .map(Some)
            .ok_or_else(|| ApiError::bad_request(INVALID_TIMEOUT)),
    }
}

#[cfg(test)]
mod tests;
