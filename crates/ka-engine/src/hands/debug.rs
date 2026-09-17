//! The `debug` hand: DAP as a curated MVP action set (roadmap 8.2 —
//! omp ships 28; ka ships the load-bearing ~16). Control-flow actions
//! are Exec tier; inspection is Read. Registered only under
//! `[debug] enable = true`, inert in `--safe-mode` like every
//! customization tier.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::{Value, json};

use super::{Hand, HandContext, HandDef, ToolOutput};
use crate::dap::{DebugManager, Wait};

/// Actions that drive the debuggee (spawn it, mutate its run state, or
/// end the session) — Exec tier. Everything else inspects.
const EXEC_ACTIONS: &[&str] = &[
    "start",
    "break",
    "clear",
    "continue",
    "next",
    "step_in",
    "step_out",
    "pause",
    "disconnect",
];

pub struct DebugHand {
    mgr: Arc<DebugManager>,
}

impl DebugHand {
    pub fn new(mgr: Arc<DebugManager>) -> Self {
        Self { mgr }
    }

    fn session(&self, args: &Value) -> Result<Arc<crate::dap::Session>, String> {
        self.mgr
            .session(args.get("session").and_then(Value::as_str))
            .inspect(|s| s.touch_pub())
            .map_err(|e| format!("debug: {e}"))
    }
}

impl Hand for DebugHand {
    fn def(&self) -> HandDef {
        HandDef {
            name: "debug".to_string(),
            description: "Drive a debug adapter (DAP) over stdio: launch or attach a program, \
                set breakpoints, step, inspect the stack and variables, evaluate expressions. \
                Control-flow actions (start/break/clear/continue/next/step_in/step_out/pause/\
                disconnect) run at Exec clearance; inspection (sessions/breaks/threads/stack/\
                vars/eval/output) reads. Blocking actions wait (bounded) for the next stop and \
                report where it landed. Adapters come from the embedded catalog plus the \
                [debug.adapters] overlay."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["start", "break", "breaks", "clear", "continue", "next",
                                 "step_in", "step_out", "pause", "threads", "stack", "vars",
                                 "eval", "output", "sessions", "disconnect"],
                        "description": "The debug operation"
                    },
                    "adapter": { "type": "string", "description": "Catalog adapter name (start)" },
                    "mode": { "type": "string", "enum": ["launch", "attach"], "description": "start (default launch)" },
                    "program": { "type": "string", "description": "Executable to launch (start)" },
                    "args": { "type": "array", "items": { "type": "string" }, "description": "Program arguments (start)" },
                    "pid": { "type": "integer", "description": "Process id to attach (start, mode attach)" },
                    "stop_on_entry": { "type": "boolean", "description": "start: break at entry (default true)" },
                    "breaks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "file": { "type": "string" },
                                "lines": { "type": "array", "items": { "type": "integer" } }
                            },
                            "required": ["file", "lines"]
                        },
                        "description": "start: breakpoints to set before configurationDone"
                    },
                    "file": { "type": "string", "description": "break: file to (re)set breakpoints in" },
                    "lines": { "type": "array", "items": { "type": "integer" }, "description": "break: 1-based lines" },
                    "expr": { "type": "string", "description": "eval: expression to evaluate" },
                    "levels": { "type": "integer", "description": "stack: frame depth (default 10)" },
                    "session": { "type": "string", "description": "Session id (default newest)" }
                },
                "required": ["action"]
            }),
            clearance: super::Clearance::Exec,
            read_only: false,
        }
    }

    /// Control flow is Exec; inspection reads; unknown actions fail
    /// closed at Exec.
    fn clearance_for(&self, args: &Value) -> super::Clearance {
        match args.get("action").and_then(Value::as_str) {
            Some(a) if EXEC_ACTIONS.contains(&a) => super::Clearance::Exec,
            Some("sessions" | "breaks" | "threads" | "stack" | "vars" | "eval" | "output") => {
                super::Clearance::Read
            }
            _ => super::Clearance::Exec,
        }
    }

    fn execute<'a>(
        &'a self,
        args: &'a Value,
        ctx: &'a HandContext,
    ) -> Pin<Box<dyn Future<Output = ToolOutput> + Send + 'a>> {
        Box::pin(async move {
            let Some(action) = args.get("action").and_then(Value::as_str) else {
                return ToolOutput::err("debug: need 'action'");
            };
            match action {
                "start" => self.start(args, ctx).await,
                "break" => {
                    let (Some(file), Some(lines)) =
                        (args.get("file").and_then(Value::as_str), lines_of(args))
                    else {
                        return ToolOutput::err("debug: break needs 'file' and 'lines'");
                    };
                    let s = self.session(args);
                    match s {
                        Ok(s) => match s.set_breakpoints(file, &lines).await {
                            Ok(text) => ToolOutput::ok(format!("debug: {text}")),
                            Err(e) => ToolOutput::err(format!("debug: {e}")),
                        },
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "breaks" => {
                    let s = self.session(args);
                    match s {
                        Ok(s) => {
                            let bps = s.breakpoints();
                            if bps.is_empty() {
                                ToolOutput::ok("debug: no breakpoints set".to_string())
                            } else {
                                let rows: Vec<String> = bps
                                    .iter()
                                    .map(|(f, l)| {
                                        format!(
                                            "{f}: {}",
                                            l.iter()
                                                .map(|n| n.to_string())
                                                .collect::<Vec<_>>()
                                                .join(",")
                                        )
                                    })
                                    .collect();
                                ToolOutput::ok(format!("debug:\n{}", rows.join("\n")))
                            }
                        }
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "clear" => {
                    let s = self.session(args);
                    match s {
                        Ok(s) => match s.clear_breakpoints().await {
                            Ok(n) => ToolOutput::ok(format!("debug: cleared {n} file(s)")),
                            Err(e) => ToolOutput::err(format!("debug: {e}")),
                        },
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "continue" | "next" | "step_in" | "step_out" => {
                    let what = match action {
                        "continue" => "continue",
                        "next" => "next",
                        "step_in" => "stepIn",
                        _ => "stepOut",
                    };
                    let s = self.session(args);
                    match s {
                        Ok(s) => match s.resume(what).await {
                            Ok(Wait::Stopped) => match s.stopped_at().await {
                                Ok(at) => ToolOutput::ok(format!("debug: stopped at {at}")),
                                Err(e) => ToolOutput::err(format!("debug: {e}")),
                            },
                            Ok(Wait::Ended) => ToolOutput::ok("debug: debuggee exited".to_string()),
                            Ok(Wait::Running) => ToolOutput::ok(
                                "debug: still running (no stop within the cap) — poll `stack` or `output`".to_string(),
                            ),
                            Err(e) => ToolOutput::err(format!("debug: {e}")),
                        },
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "pause" => {
                    let s = self.session(args);
                    match s {
                        Ok(s) => match s.pause().await {
                            Ok(Wait::Stopped) => match s.stopped_at().await {
                                Ok(at) => ToolOutput::ok(format!("debug: paused at {at}")),
                                Err(e) => ToolOutput::err(format!("debug: {e}")),
                            },
                            Ok(Wait::Ended) => ToolOutput::ok("debug: debuggee exited".to_string()),
                            Ok(Wait::Running) => ToolOutput::ok("debug: still running".to_string()),
                            Err(e) => ToolOutput::err(format!("debug: {e}")),
                        },
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "threads" => {
                    let s = self.session(args);
                    match s {
                        Ok(s) => match s.threads().await {
                            Ok(rows) => ToolOutput::ok(format!("debug:\n{}", rows.join("\n"))),
                            Err(e) => ToolOutput::err(format!("debug: {e}")),
                        },
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "stack" => {
                    let levels = args.get("levels").and_then(Value::as_u64).unwrap_or(10);
                    let s = self.session(args);
                    match s {
                        Ok(s) => match s.stack(levels).await {
                            Ok(rows) if rows.is_empty() => ToolOutput::ok(
                                "debug: no stack frames (running or exited?)".to_string(),
                            ),
                            Ok(rows) => ToolOutput::ok(format!("debug:\n{}", rows.join("\n"))),
                            Err(e) => ToolOutput::err(format!("debug: {e}")),
                        },
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "vars" => {
                    let s = self.session(args);
                    match s {
                        Ok(s) => match s.variables(20).await {
                            Ok(rows) if rows.is_empty() => {
                                ToolOutput::ok("debug: no scopes reported".to_string())
                            }
                            Ok(rows) => ToolOutput::ok(format!("debug:\n{}", rows.join("\n"))),
                            Err(e) => ToolOutput::err(format!("debug: {e}")),
                        },
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "eval" => {
                    let Some(expr) = args.get("expr").and_then(Value::as_str) else {
                        return ToolOutput::err("debug: eval needs 'expr'");
                    };
                    let s = self.session(args);
                    match s {
                        Ok(s) => match s.evaluate(expr).await {
                            Ok(result) => ToolOutput::ok(format!("debug: {result}")),
                            Err(e) => ToolOutput::err(format!("debug: {e}")),
                        },
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "output" => {
                    let s = self.session(args);
                    match s {
                        Ok(s) => {
                            let lines = s.output_lines();
                            if lines.is_empty() {
                                ToolOutput::ok("debug: no adapter output".to_string())
                            } else {
                                let mut tail: Vec<String> =
                                    lines.into_iter().rev().take(40).collect::<Vec<_>>();
                                tail.reverse();
                                ToolOutput::ok(format!("debug:\n{}", tail.join("\n")))
                            }
                        }
                        Err(e) => ToolOutput::err(e),
                    }
                }
                "sessions" => {
                    let sessions = self.mgr.sessions();
                    if sessions.is_empty() {
                        ToolOutput::ok("debug: no live sessions".to_string())
                    } else {
                        let rows: Vec<String> = sessions
                            .iter()
                            .map(|s| {
                                format!(
                                    "{}  breakpoints in {} file(s)",
                                    s.id(),
                                    s.breakpoints().len()
                                )
                            })
                            .collect();
                        ToolOutput::ok(format!("debug:\n{}", rows.join("\n")))
                    }
                }
                "disconnect" => {
                    let s = self.session(args);
                    match s {
                        Ok(s) => {
                            let id = s.id().to_string();
                            match s.disconnect().await {
                                Ok(()) => {
                                    self.mgr.remove(&id);
                                    ToolOutput::ok(format!("debug: session {id} disconnected"))
                                }
                                Err(e) => ToolOutput::err(format!("debug: {e}")),
                            }
                        }
                        Err(e) => ToolOutput::err(e),
                    }
                }
                other => ToolOutput::err(format!(
                    "debug: unknown action {other:?} (start/break/breaks/clear/continue/next/\
                     step_in/step_out/pause/threads/stack/vars/eval/output/sessions/disconnect)"
                )),
            }
        })
    }
}

fn lines_of(args: &Value) -> Option<Vec<u64>> {
    Some(
        args.get("lines")?
            .as_array()?
            .iter()
            .map(|v| v.as_u64().unwrap_or(0))
            .collect(),
    )
}

impl DebugHand {
    /// Launch or attach, set pre-configuration breakpoints, run
    /// configurationDone, and wait for the first stop.
    async fn start(&self, args: &Value, ctx: &HandContext) -> ToolOutput {
        let Some(adapter) = args.get("adapter").and_then(Value::as_str) else {
            return ToolOutput::err("debug: start needs 'adapter'");
        };
        let attach = args.get("mode").and_then(Value::as_str) == Some("attach");
        let mut launch_args = if attach {
            let pid = args.get("pid").and_then(Value::as_u64);
            match pid {
                Some(pid) => json!({ "pid": pid }),
                None => {
                    return ToolOutput::err("debug: attach needs 'pid'");
                }
            }
        } else {
            let Some(program) = args.get("program").and_then(Value::as_str) else {
                return ToolOutput::err("debug: launch needs 'program'");
            };
            let cwd: PathBuf = match args.get("cwd").and_then(Value::as_str) {
                Some(c) => PathBuf::from(c),
                None => ctx.cwd.clone(),
            };
            json!({
                "program": program,
                "args": args.get("args").cloned().unwrap_or(json!([])),
                "cwd": cwd.display().to_string(),
                "stopOnEntry": args
                    .get("stop_on_entry")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
            })
        };
        if attach {
            if let Some(program) = args.get("program").and_then(Value::as_str) {
                launch_args["program"] = json!(program);
            }
        }
        let mut breaks: Vec<(String, Vec<u64>)> = Vec::new();
        if let Some(items) = args.get("breaks").and_then(Value::as_array) {
            for b in items {
                let (Some(file), Some(lines)) = (
                    b.get("file").and_then(Value::as_str),
                    b.get("lines").and_then(|l| {
                        l.as_array().map(|a| {
                            a.iter()
                                .map(|v| v.as_u64().unwrap_or(0))
                                .collect::<Vec<_>>()
                        })
                    }),
                ) else {
                    continue;
                };
                breaks.push((file.to_string(), lines));
            }
        }
        match self
            .mgr
            .start(adapter, !attach, launch_args, &breaks, &ctx.cwd)
            .await
        {
            Ok(s) => {
                let id = s.id().to_string();
                s.touch_pub();
                match s.wait_stop().await {
                    Wait::Stopped => match s.stopped_at().await {
                        Ok(at) => ToolOutput::ok(format!(
                            "debug: session {id} started — stopped at {at}\n(inspect with \
                             stack/vars/eval; step with continue/next/step_in/step_out)"
                        )),
                        Err(e) => ToolOutput::err(format!("debug: {e}")),
                    },
                    Wait::Ended => {
                        ToolOutput::ok(format!("debug: session {id} started — debuggee exited"))
                    }
                    Wait::Running => ToolOutput::ok(format!(
                        "debug: session {id} started — still running (no stop within the cap); \
                         poll `stack` or `output`"
                    )),
                }
            }
            Err(e) => ToolOutput::err(format!("debug: {e}")),
        }
    }
}
