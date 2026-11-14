use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::{session, Agent};

const MAX_RUNNING: usize = 8;
const TAIL_CHARS: usize = 4000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Command,
    Turn,
}

struct Job {
    session: String,
    kind: Kind,
    token: CancellationToken,
}

struct Finished {
    command: String,
    state: &'static str,
    code: String,
    output: PathBuf,
}

#[derive(Default)]
pub struct Jobs {
    running: Mutex<HashMap<String, Job>>,
    finished: Mutex<HashMap<String, Vec<Finished>>>,
}

fn short(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= max {
        line.to_owned()
    } else {
        format!("{}…", line.chars().take(max).collect::<String>())
    }
}

impl Jobs {
    fn insert(&self, id: &str, session: &str, kind: Kind, token: &CancellationToken) {
        if let Ok(mut map) = self.running.lock() {
            map.insert(
                id.to_owned(),
                Job {
                    session: session.to_owned(),
                    kind,
                    token: token.clone(),
                },
            );
        }
    }

    fn remove(&self, id: &str) {
        if let Ok(mut map) = self.running.lock() {
            map.remove(id);
        }
    }

    fn commands(&self) -> usize {
        self.running.lock().map_or(0, |m| {
            m.values().filter(|j| j.kind == Kind::Command).count()
        })
    }

    pub fn stop(&self, id: &str) -> bool {
        self.running.lock().is_ok_and(|map| {
            map.get(id).is_some_and(|job| {
                job.token.cancel();
                true
            })
        })
    }

    pub fn cancel_turns(&self, session: &str) {
        if let Ok(map) = self.running.lock() {
            for job in map
                .values()
                .filter(|j| j.kind == Kind::Turn && j.session == session)
            {
                job.token.cancel();
            }
        }
    }

    pub fn cancel_all(&self) {
        if let Ok(map) = self.running.lock() {
            for job in map.values() {
                job.token.cancel();
            }
        }
    }

    fn push_finished(&self, session: &str, finished: Finished) {
        if let Ok(mut map) = self.finished.lock() {
            map.entry(session.to_owned()).or_default().push(finished);
        }
    }

    fn take_finished(&self, session: &str) -> Vec<Finished> {
        self.finished
            .lock()
            .ok()
            .and_then(|mut map| map.remove(session))
            .unwrap_or_default()
    }
}

pub fn start_command(
    agent: &Agent,
    session: &str,
    cwd: &std::path::Path,
    command: &str,
) -> Result<String, String> {
    let Some(me) = agent.me.upgrade() else {
        return Err("the agent is shutting down".into());
    };
    if agent.jobs.commands() >= MAX_RUNNING {
        return Err(format!(
            "{MAX_RUNNING} background commands are already running; wait for one to finish"
        ));
    }
    let id = format!("bash-{}", uuid::Uuid::new_v4().simple());
    let dir = std::env::temp_dir().join("harness-api-agent");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let output = dir.join(format!("{id}.log"));
    let file = std::fs::File::create(&output).map_err(|e| format!("{}: {e}", output.display()))?;
    let err_file = file
        .try_clone()
        .map_err(|e| format!("{}: {e}", output.display()))?;
    let mut process = crate::tools::shell(command);
    process
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(err_file))
        .kill_on_drop(true);
    let mut child = process
        .spawn()
        .map_err(|e| format!("cannot start the shell: {e}"))?;
    let token = CancellationToken::new();
    agent.jobs.insert(&id, session, Kind::Command, &token);
    let path = output.to_string_lossy().into_owned();
    agent.update(
        session,
        &json!({
            "sessionUpdate": "async_task_spawned",
            "asyncTaskId": id,
            "name": short(command, 60),
            "taskType": "local_bash",
            "description": short(command, 120),
            "showInTranscript": true,
            "canStop": true,
            "outputFilePath": path
        }),
    );
    let session = session.to_owned();
    let command = command.to_owned();
    let task = id.clone();
    tokio::spawn(async move {
        let status = tokio::select! {
            () = token.cancelled() => {
                let _ = child.kill().await;
                None
            }
            status = child.wait() => status.ok(),
        };
        let (state, code) = match status {
            None => ("stopped", "stopped".to_owned()),
            Some(status) => {
                let code = status
                    .code()
                    .map_or_else(|| "killed".to_owned(), |c| c.to_string());
                (
                    if status.success() {
                        "completed"
                    } else {
                        "failed"
                    },
                    code,
                )
            }
        };
        me.jobs.remove(&task);
        let summary = if state == "stopped" {
            "stopped".to_owned()
        } else {
            format!("exit code {code}")
        };
        me.update(
            &session,
            &json!({"sessionUpdate": "async_task_state_update", "asyncTaskId": task, "state": state, "summary": summary}),
        );
        me.jobs.push_finished(
            &session,
            Finished {
                command,
                state,
                code,
                output,
            },
        );
        wake(me, session).await;
    });
    Ok(format!(
        "Started in the background as task {id}. Output: {path}. You will get its result in a new turn when it ends; do not wait or poll for it."
    ))
}

async fn tail(path: &std::path::Path) -> String {
    let bytes = tokio::fs::read(path).await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let count = text.chars().count();
    if count <= TAIL_CHARS {
        return text.trim_end().to_owned();
    }
    let rest: String = text.chars().skip(count - TAIL_CHARS).collect();
    format!("… {rest}").trim_end().to_owned()
}

async fn notification(finished: &[Finished]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for item in finished {
        let tail = tail(&item.output).await;
        let _ = write!(
            out,
            "<task-notification>\n<command>{}</command>\n<status>{}</status>\n<exit-code>{}</exit-code>\n<output-file>{}</output-file>\n<output-tail>\n{tail}\n</output-tail>\n</task-notification>\n",
            item.command,
            item.state,
            item.code,
            item.output.display(),
        );
    }
    out.push_str(
        "A background command you started has ended. Look at its result and continue the work.",
    );
    out
}

async fn wake(agent: Arc<Agent>, id: String) {
    let Some(session) = agent.session(&id) else {
        return;
    };
    let mut s = session.lock().await;
    loop {
        let finished = agent.jobs.take_finished(&id);
        if finished.is_empty() {
            break;
        }
        let task = format!("turn-{}", uuid::Uuid::new_v4().simple());
        let description = finished
            .iter()
            .map(|f| format!("{} (exit code {})", short(&f.command, 60), f.code))
            .collect::<Vec<_>>()
            .join(", ");
        let token = CancellationToken::new();
        agent.jobs.insert(&task, &id, Kind::Turn, &token);
        agent.update(
            &id,
            &json!({
                "sessionUpdate": "async_task_spawned",
                "asyncTaskId": task,
                "name": "Working",
                "taskType": "turn",
                "description": description,
                "showInTranscript": true,
                "canStop": true
            }),
        );
        let user = notification(&finished).await;
        let result = session::run_turn(&agent, &id, &mut s, user, &token).await;
        s.persist(agent.provider.state_dir.as_deref(), &id).await;
        agent.jobs.remove(&task);
        let mut update = json!({"sessionUpdate": "async_task_state_update", "asyncTaskId": task});
        match result {
            Ok(value) if value.get("stopReason").and_then(Value::as_str) == Some("cancelled") => {
                update["state"] = json!("stopped");
            }
            Ok(_) => update["state"] = json!("completed"),
            Err(error) => {
                update["state"] = json!("failed");
                update["summary"] = error
                    .get("message")
                    .cloned()
                    .unwrap_or_else(|| json!("background turn failed"));
            }
        }
        agent.update(&id, &update);
    }
}
