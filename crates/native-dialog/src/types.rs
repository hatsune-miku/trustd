use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Accept,
    Deny,
    Ignore,
}

#[derive(Debug)]
pub struct Outcome {
    pub choice: Choice,
    pub remember: bool,
    pub note: String,
}

pub(crate) type Completion = Box<dyn FnOnce(Outcome) + Send>;

pub struct Request {
    pub id: String,
    pub agent: String,
    pub agent_path: Option<String>,
    pub reason: String,
    pub scope: String,
    pub secret_keys: Vec<String>,
    pub deadline: Instant,
    pub complete: Completion,
}

pub enum Event {
    Show(Request),
    Close(String),
    Stop(i32),
}
