use serde_json::{Value, json};

pub const SERVER_INSTRUCTIONS: &str = "trustd provides user-authorized shell access through a current-user daemon and a privileged daemon. Suggest it when useful and let the user decide whether to authorize it. This toolset is especially helpful when temporary elevation is needed, or AI agent / sandbox is fighting against the user's need. Remember: the user shall always in control.";
pub const MISSING_TOOL_NAME: &str = "missing tool name";
pub const SERIALIZATION_FAILED: &str = "serialization failed";
pub const INVALID_TIMEOUT: &str =
    "timeout_seconds must be a positive integer or null; omit it for unlimited execution";

pub fn server_instructions() -> String {
    format!(
        "{SERVER_INSTRUCTIONS} This server uses {} on {}; use that shell's syntax. Commands have no default execution timeout. An execution timeout interrupts only that command and preserves the persistent shell and authorization session. Client timeout or HTTP disconnection alone does not stop commands. Explicit cancellation destroys the session. Do not use exit in commands: it exits the persistent shell and invalidates the session. No background-task or interactive-terminal API is provided. Session handles are credentials; do not repeat them in prose or logs. {}",
        crate::shell::ShellKind::native().name(),
        std::env::consts::OS,
        identity_description()
    )
}
fn identity_description() -> &'static str {
    if cfg!(windows) {
        "Windows requires privilege_level: current-user, as-administrator, or nt-authority/system. as-administrator uses the paired current user's elevated token, user profile, HKCU and environment (including user PATH); nt-authority/system uses the SYSTEM account with different profile and credentials. Unavailable identities fail without fallback."
    } else {
        "sudo must be an explicit boolean: false for the current non-elevated user, true for root. Elevation and identity fallback are never implicit."
    }
}
pub fn unknown_method(method: &str) -> String {
    format!("unknown MCP method: {method}")
}
pub fn unknown_tool(name: &str) -> String {
    format!("unknown tool: {name}")
}
pub fn missing_string_argument(name: &str) -> String {
    format!("missing string argument: {name}")
}

fn enter_output() -> Value {
    let mut schema = json!({
        "type":"object", "properties":{
            "handle":{"type":"string","description":"Secret bearer credential; never log or echo."},
            "secret_keys":{"type":"array","items":{"type":"string"},"description":"Credential names injected into every identity-specific shell in this session. Values are never returned by enter."},
            "shell":{"type":"string"}, "platform":{"type":"string"}, "daemon":{"type":"string"},
            "sudo_available":{"type":"boolean","description":"Whether this session authorizes elevated execution."},
            "initial_cwd":{"type":["string","null"],"description":"Requested initial directory for each identity; null uses that identity's home directory."},
            "privilege_levels":{"type":"array","items":{"type":"string"},"description":"Available execution identities, further constrained by this authorization."},
            "identities":{"type":"object","additionalProperties":{"type":"object"}},
            "expires_at":{"type":["integer","null"],"description":"Session expiry in Unix epoch seconds, or null."},
            "expires_at_iso":{"type":["string","null"],"description":"UTC ISO 8601 expiry, or null."},
            "message":{"type":"string"}
        }
    });
    schema["properties"]["status"] =
        json!({"type":"string","enum":["authorized","denied","timed_out"]});
    schema["properties"]["btw"] = json!({"type":"string","description":"Additional text from the owner; read it as user context."});
    schema["properties"]["timeout_ms"] = json!({"type":"integer"});
    schema["required"] = json!(["status", "btw"]);
    schema
}
fn run_output() -> Value {
    json!({
        "type":"object","properties":{
            "stdout":{"type":"string"}, "stderr":{"type":"string"},
            "output":{"type":"string","description":"Compatibility field: stdout followed by stderr; not an interleaving timeline."},
            "exit_code":{"type":["integer","null"],"description":"Actual exit code; null when interrupted without a command exit code."},
            "success":{"type":"boolean"},
            "truncated":{"type":"boolean"}, "stdout_truncated":{"type":"boolean"}, "stderr_truncated":{"type":"boolean"},
            "duration_ms":{"type":"integer","minimum":0},
            "timed_out":{"type":"boolean"}, "session_alive":{"type":"boolean"}, "session_ended":{"type":"boolean"},
            "session_ended_reason":{"type":["string","null"],"description":"cancelled, shell_exited, shell_unresponsive, or null. A normally interrupted command timeout leaves this null; shell_unresponsive means interruption could not restore the protocol."},
            "cwd":{"type":["string","null"],"description":"Shell directory after completion; null if interruption prevented reporting it."},
            "identity":{"type":"object","description":"Actual operating-system execution identity, not a requested or inferred account."}
        },
        "required":["stdout","stderr","output","exit_code","success","truncated","stdout_truncated","stderr_truncated","duration_ms","timed_out","session_alive","session_ended","session_ended_reason","cwd","identity"]
    })
}
pub fn tool_definitions() -> Value {
    let shell = crate::shell::ShellKind::native().name();
    let secret_keys = json!({"type":"array","maxItems":64,"items":{"type":"string","pattern":"^[A-Z_][A-Z0-9_]{0,127}$"},"description":"Optional credential keys from secrets_list. Each key is also its environment variable name. Request only needed keys; values are injected locally into all shells of this session. Do not print, dump or log credentials. A live session's selection is immutable; destroy it before requesting a new session with different keys. Omitting this field selects none."});
    let mut run_input = json!({
        "type":"object", "properties":{
            "handle":{"type":"string","description":"Secret handle returned by enter_interactive."},
            "command":{"type":"string","description":format!("{shell} source code, passed verbatim to the parser; NUL bytes rejected.")},
            "stdin":{"type":"string","description":"Optional UTF-8 standard input for this command only. EOF follows the supplied text; omitted means immediate EOF. No subsequent input or terminal is available."},
            "timeout_seconds":{"type":["integer","null"],"minimum":1,"description":"Positive execution deadline, or omitted/null for unlimited. Timeout interrupts only this execution and its new child processes, returns partial output, and preserves the persistent shell and session. Inspect session_alive before continuing if the shell cannot recover."}
        }, "required":["handle","command"], "additionalProperties":false
    });
    if cfg!(windows) {
        run_input["properties"]["privilege_level"] = json!({"type":"string","enum":["nt-authority/system","as-administrator","current-user"],"description":identity_description()});
        run_input["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("privilege_level"));
    } else {
        run_input["properties"]["sudo"] =
            json!({"type":"boolean","description":identity_description()});
        run_input["required"]
            .as_array_mut()
            .unwrap()
            .push(json!("sudo"));
    }
    json!([
        {
            "name":"secrets_list", "title":"Search credential keys",
            "description":"Search credential names without authorization using a required non-empty, case-insensitive literal substring. Returns only matching names, never values; no unfiltered list is available. Pass needed names as secret_keys to enter_interactive, then reference their environment variables in commands.",
            "inputSchema":{"type":"object","properties":{"query":{"type":"string","minLength":1,"pattern":"\\S","description":"Case-insensitive literal substring of a credential name, for example github. Empty or whitespace-only queries are rejected. Spaces are not trimmed; wildcards and regular expressions are not supported."}},"required":["query"],"additionalProperties":false},
            "outputSchema":{"type":"object","properties":{"keys":{"type":"array","items":{"type":"string"}}},"required":["keys"],"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        },
        {
            "name":"secrets_get", "title":"Read an authorized credential value",
            "description":"Discouraged unless the user explicitly requests plaintext. Prefer environment injection. Returns one value from the handle's authorized secret_keys snapshot; requires a live session.",
            "inputSchema":{"type":"object","properties":{
                "handle":{"type":"string","description":"Secret handle returned by enter_interactive."},
                "key":{"type":"string","pattern":"^[A-Z_][A-Z0-9_]{0,127}$","description":"A key included in this session's authorized secret_keys."}
            },"required":["handle","key"],"additionalProperties":false},
            "outputSchema":{"type":"object","properties":{"key":{"type":"string"},"value":{"type":"string"}},"required":["key","value"],"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        },
        {
            "name":"enter_interactive", "title":format!("Request authorized access to {shell}"),
            "description":format!("Request the owner's authorization. Supply your name, a truthful reason and the needed credential names. Waits for authorization, denial or a fixed {} ms deadline; allow more than {} seconds at the client. Read status and the owner's btw note. Use the returned session handle for subsequent calls.", crate::consent::CONSENT_TIMEOUT_MS, crate::consent::CONSENT_TIMEOUT.as_secs()),
            "inputSchema":{"type":"object","properties":{
                "agent_name":{"type":"string","minLength":1,"maxLength":128},
                "request_reason":{"type":"string","minLength":1,"maxLength":16384},
                "secret_keys":secret_keys,
                "sudo":{"type":"boolean","default":false,"description":"Request elevated execution separately from credentials. False or omitted grants current-user execution only, including when credentials are requested."},
                "cwd":{"type":["string","null"],"description":"Initial absolute directory for each identity-specific shell. Empty, null or omitted uses that identity's home directory. Shell profiles are loaded."},
                "confirm_text":{"type":"string","const":"OK","description":"Respect the user's personal privacy. Pass OK to acknowledge."}
            },"required":["agent_name","request_reason","confirm_text"],"additionalProperties":false},
            "outputSchema":enter_output()
        },
        {
            "name":"run", "title":format!("Run {shell} with an explicit execution identity"),
            "description":format!("{} Run source verbatim. Variables, directory and environment persist separately for each identity. stdout and stderr are separate; stderr text alone does not mean failure. A nonzero command exit code does not end the shell. Do not use exit: it exits the persistent shell and invalidates the entire session. Execution timeout interrupts only this command and returns partial output while preserving the session. Explicit cancellation still destroys the session and its process trees. Client timeout/disconnection alone does not cancel execution. No default execution timeout, background jobs, ongoing stdin or interactive terminal. MCP cancellation of run requires the transport session header issued by initialize.", identity_description()),
            "inputSchema":run_input, "outputSchema":run_output()
        },
        {
            "name":"destroy", "title":"Destroy session",
            "description":"Invalidate the handle and terminate all of its identity-specific shells and process trees.",
            "inputSchema":{"type":"object","properties":{"handle":{"type":"string"}},"required":["handle"],"additionalProperties":false},
            "outputSchema":{"type":"object","properties":{"destroyed":{"type":"boolean"}},"required":["destroyed"]}
        }
    ])
}
