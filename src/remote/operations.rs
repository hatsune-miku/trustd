//! Operations a target device accepts, with their argument schemas, as
//! reported by `host_info`.
use serde_json::{Value, json};

pub(crate) fn operation_schemas() -> Value {
    let text = json!({"type":"string"});
    let number = json!({"type":"integer","minimum":0});
    let boolean = json!({"type":"boolean"});
    let mut run = json!({"session_ref":text,"command":text,"stdin":text,"timeout_seconds":{"type":["integer","null"],"minimum":1}});
    let mut required = vec!["session_ref", "command"];
    if cfg!(windows) {
        run["privilege_level"] = json!({"type":"string","enum":["current-user","as-administrator","nt-authority/system"]});
        required.push("privilege_level");
    } else {
        run["sudo"] = boolean.clone();
        required.push("sudo");
    }
    let mut job = run.clone();
    job["idempotency_key"] = text.clone();
    let mut job_required = required.clone();
    job_required.push("idempotency_key");
    let specs = vec![
        (
            "logs_page",
            "Read this device's command or audit records on the target host.",
            json!({"kind":{"type":"string","enum":["command","audit"]},"before":number,"limit":number}),
            vec!["kind"],
        ),
        (
            "secrets_list",
            "List all credential names within this device's grant.",
            json!({}),
            vec![],
        ),
        (
            "session_open",
            "Open a remote session; load profiles, default cwd is the target identity's home.",
            json!({"secret_keys":{"type":"array","items":text},"sudo":boolean,"cwd":{"type":["string","null"]}}),
            vec![],
        ),
        (
            "session_close",
            "Close the session; independent jobs continue.",
            json!({"session_ref":text}),
            vec!["session_ref"],
        ),
        (
            "run",
            "Run in the persistent shell. No default timeout. Do not retry an ambiguous execution.",
            run,
            required,
        ),
        (
            "job_start",
            "Submit a durable task with a stable idempotency_key. Wait with job_wait instead of polling. Output goes to stdout_path/stderr_path on the target: read it with job_output, or process it there with run (for example compress it first). No default runtime limit.",
            job,
            job_required,
        ),
        (
            "job_get",
            "Inspect task status and result.",
            json!({"job_id":text}),
            vec!["job_id"],
        ),
        (
            "job_wait",
            "Wait until the task finishes or timeout_seconds pass (default 300, at most 3600), then return it; check status to tell which.",
            json!({"job_id":text,"timeout_seconds":{"type":"integer","minimum":1}}),
            vec!["job_id"],
        ),
        (
            "job_list",
            "List this device's tasks.",
            json!({"offset":number,"limit":number}),
            vec![],
        ),
        (
            "job_cancel",
            "Cancel this job and its process tree.",
            json!({"job_id":text}),
            vec!["job_id"],
        ),
        (
            "job_output",
            "Read stdout/stderr as text, or as base64 data when the bytes are not text or encoding is base64. tail reads the last N bytes; next_offset continues losslessly.",
            json!({"job_id":text,"stream":{"type":"string","enum":["stdout","stderr"]},"offset":number,"tail":number,"limit":number,"encoding":{"type":"string","enum":["text","base64"]}}),
            vec!["job_id"],
        ),
        (
            "file_stat",
            "Inspect an absolute remote path; compute optional checksum on this host.",
            json!({"session_ref":text,"path":text,"checksum":boolean}),
            vec!["session_ref", "path"],
        ),
        (
            "file_read",
            "Read an absolute remote path as text (at most 256 KiB), or as base64 data (at most 1 MiB) when the bytes are not text or encoding is base64. tail reads the last N bytes.",
            json!({"session_ref":text,"path":text,"offset":number,"tail":number,"limit":number,"encoding":{"type":"string","enum":["text","base64"]}}),
            vec!["session_ref", "path"],
        ),
        (
            "file_write",
            "Upload a base64 chunk of at most 1 MiB. Stable transfer_id is a UUID; complete returns sha256 and verifies it when supplied. Existing files require replace.",
            json!({"session_ref":text,"path":text,"transfer_id":text,"offset":number,"data":text,"complete":boolean,"sha256":text,"replace":boolean}),
            vec!["session_ref", "path", "transfer_id", "offset", "data"],
        ),
    ];
    json!(specs.into_iter().map(|(name,description,properties,required)| {
        let mut value = json!({"name":name,"description":description,
            "inputSchema":{"type":"object","properties":properties,"additionalProperties":false},"outputSchema":{"type":"object","properties":{},"additionalProperties":true}});
        if !required.is_empty() { value["inputSchema"]["required"] = json!(required); }
        value
    }).collect::<Vec<_>>())
}
